//! Explicit source epochs: H.222.0 §2.4.3.5 and §2.4.4.9–10.
use super::*;
use std::ops::Range;
use triergon_mpegts::{SourceEpochs, SourceRange};

const NO_ENTRY: Error = Error::Unsupported(
    "MPEG-TS selected epoch has no eligible access unit at or before the target",
);

fn epoch(clock: u64, configuration: u64) -> SourceEpochs {
    SourceEpochs {
        clock,
        configuration,
    }
}

fn request(target: u64) -> SeekRequest {
    SeekRequest {
        track: TrackId(PID.into()),
        target: clock(target),
        policy: SeekPolicy::PlaybackPreroll,
    }
}

fn video(ts: &mut Transport, hevc: bool, base: u64) {
    let (raw, layout): (&[u8], &str) = if hevc {
        (
            include_bytes!("../../../framing/fixtures/raw-annex-b/hevc-aud0.hevc"),
            include_str!("../../../framing/fixtures/raw-annex-b/hevc-aud0.packets"),
        )
    } else {
        (
            include_bytes!("../../../framing/fixtures/raw-annex-b/avc-aud0.h264"),
            include_str!("../../../framing/fixtures/raw-annex-b/avc-aud0.packets"),
        )
    };
    for (index, line) in layout.lines().enumerate() {
        let mut fields = line.split_ascii_whitespace();
        let at: usize = fields.next().unwrap().parse().unwrap();
        let bytes: usize = fields.next().unwrap().parse().unwrap();
        // Reordered presentation clocks with strictly increasing encoded DTS.
        let presentation = match index % 4 {
            1 => index + 2,
            2 | 3 => index - 1,
            _ => index,
        };
        let pts = base + presentation as u64 * 3000;
        let dts = base - 9000 + index as u64 * 3000;
        ts.pes(PID, 0xe0, &raw[at..at + bytes], Some(pts), Some(dts));
    }
}

fn changed_video(configuration: bool) -> Transport {
    let mut ts = Transport::new();
    ts.tables(&[(0x1b, PID, &[])], 0);
    ts.pcr(80_000, false);
    video(&mut ts, false, 90_000);
    if configuration {
        ts.tables(&[(0x24, PID, &[])], 1);
    } else {
        ts.pcr(80_000, true);
        ts.pcr(81_000, false);
    }
    video(&mut ts, configuration, 90_000);
    // The selected landing must not use the final catalogue as its decoder setup.
    ts.tables(&[(0x0f, PID, &[])], 2);
    ts.pes(
        PID,
        0xc0,
        &adts(b"following configuration"),
        Some(90_000),
        None,
    );
    ts
}

#[derive(Debug, PartialEq, Eq)]
struct Origin {
    unit: Range<u64>,
    payload: Range<usize>,
    headers: Vec<SourceRange>,
    ranges: Vec<SourceRange>,
    epochs: SourceEpochs,
    byte_run: u64,
    raw_pts: Option<u64>,
    raw_dts: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    bytes: Vec<u8>,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<Time>,
    epochs: SourceEpochs,
    range: Range<u64>,
    configuration: u64,
    stream_type: u8,
    pmt_version: u8,
    clock_key: Option<triergon_mpegts::clock::ClockKey>,
    origins: Vec<Origin>,
}

fn snapshot(packet: &Packet) -> Snapshot {
    Snapshot {
        bytes: packet.payload().to_vec(),
        pts: packet.pts(),
        dts: packet.dts(),
        duration: packet.sample_duration(),
        epochs: packet.epochs(),
        range: packet.elementary_range(),
        configuration: packet.track_info().configuration_epoch(),
        stream_type: packet.track_info().stream().stream_type,
        pmt_version: packet.track_info().program_map().header.version,
        clock_key: packet.clock().map(|clock| clock.key),
        origins: packet
            .sources()
            .iter()
            .map(|source| {
                let TraceEvent::Pes(pes) = source.pes.event() else {
                    panic!("missing original PES");
                };
                Origin {
                    unit: source.elementary_range.clone(),
                    payload: source.payload_range.clone(),
                    headers: pes.header_source_ranges.as_slice().to_vec(),
                    ranges: pes.source_ranges().collect(),
                    epochs: pes.epochs,
                    byte_run: pes.byte_run_epoch,
                    raw_pts: pes.header.pts_raw33,
                    raw_dts: pes.header.dts_raw33,
                }
            })
            .collect(),
    }
}

fn sequential(pool: &Budget, raw: &[u8], format: PacketFormat) -> Vec<Packet> {
    let mut reader = reader(pool, raw, format);
    let mut packets = Vec::new();
    while let Some(event) = next(&mut reader, pool, raw, 7, 1).unwrap() {
        if let Event::Packet(packet) = event {
            packets.push(packet);
        }
    }
    packets
}

fn refusal(reader: &mut Reader, pool: &Budget, raw: &[u8], expected: Error) {
    loop {
        match next(reader, pool, raw, 127, 17) {
            Ok(Some(Event::Other(_))) => (),
            Err(error) => {
                assert_eq!(error, expected);
                return;
            }
            event => panic!("refusal emitted a landing or packet: {event:?}"),
        }
    }
}

#[test]
fn clock_and_configuration_selection_preserves_the_exact_decode_suffix() {
    for configuration in [false, true] {
        let ts = changed_video(configuration);
        for format in [PacketFormat::Ts188, PacketFormat::M2ts192] {
            let raw = if format == PacketFormat::Ts188 {
                ts.bytes.clone()
            } else {
                ts.m2ts()
            };
            let pool = Budget::new(32 << 20).unwrap();
            let packets = sequential(&pool, &raw, format);
            assert!(packets.windows(2).any(|pair| pair[0].pts() > pair[1].pts()));
            // Initial PAT binding and PMT each advance the configuration epoch.
            let selected = if configuration {
                epoch(0, 3)
            } else {
                epoch(1, 2)
            };
            assert!(packets.iter().any(|packet| packet.epochs() == selected));
            let before = pool.used();
            let mut seeking = Reader::new_with_packet_tracks(
                &pool,
                ID,
                SourceCapabilities {
                    length: Some(raw.len() as u64),
                    random_access: true,
                },
                ReaderLimits {
                    format,
                    ..ReaderLimits::default()
                },
                4096,
                &[TrackId(PID.into())],
            )
            .unwrap();
            // Repeat backwards into the earlier epoch after reaching EOF.
            for epochs in [selected, epoch(0, 2), selected] {
                let target = request(112_000);
                let (at, landing) = packets
                    .iter()
                    .enumerate()
                    .filter(|(_, packet)| {
                        packet.epochs() == epochs
                            && packet.random_access()
                                == triergon_core::container::RandomAccess::Sync
                            && packet
                                .clock()
                                .unwrap()
                                .pts
                                .exact
                                .is_some_and(|time| time <= target.target)
                    })
                    .max_by_key(|(index, packet)| (packet.clock().unwrap().pts.exact, *index))
                    .unwrap();
                let expected: Vec<_> = packets[at..].iter().map(snapshot).collect();
                seeking.request_seek_in_epoch(target, epochs).unwrap();
                let mut received = Vec::new();
                let mut landed = false;
                let mut map_version = None;
                let mut timeline = 0;
                while let Some(event) = next(&mut seeking, &pool, &raw, 127, 17).unwrap() {
                    match event {
                        Event::Seek(receipt) => {
                            assert!(!landed);
                            assert_eq!(receipt.requested, target.target);
                            assert_eq!(
                                receipt.landing,
                                landing.clock().unwrap().pts.exact.unwrap()
                            );
                            assert_eq!(receipt.discard_until, target.target);
                            assert_eq!(receipt.method, SeekMethod::BoundedScan);
                            assert_eq!(receipt.scanned_bytes, raw.len() as u64);
                            assert_eq!(
                                map_version,
                                Some(landing.track_info().program_map().header.version)
                            );
                            assert_eq!(timeline, epochs.clock);
                            landed = true;
                        }
                        Event::Packet(packet) => {
                            assert!(landed);
                            if received.is_empty() {
                                let AccessUnit::Video(unit) = packet.access_unit() else {
                                    panic!("landing must retain its video decoder setup");
                                };
                                let kinds: Vec<_> = unit
                                    .nals()
                                    .iter()
                                    .map(|nal| nal.facts().header.nal_unit_type())
                                    .collect();
                                let required: &[u8] =
                                    if packet.track_info().stream().stream_type == 0x24 {
                                        &[32, 33, 34]
                                    } else {
                                        &[7, 8]
                                    };
                                assert!(required.iter().all(|kind| kinds.contains(kind)));
                            }
                            received.push(snapshot(&packet));
                        }
                        Event::Other(ReaderEvent::Source(source)) => match source.event() {
                            TraceEvent::ProgramMap {
                                map, active: true, ..
                            } => map_version = Some(map.header.version),
                            TraceEvent::Timeline { clock_epoch, .. } => timeline = *clock_epoch,
                            _ => (),
                        },
                        Event::Other(_) => (),
                    }
                }
                assert!(landed);
                assert_eq!(received, expected);
            }
            // The same mixed source stays ambiguous without an explicit pair.
            seeking.request_seek(request(112_000)).unwrap();
            refusal(
                &mut seeking,
                &pool,
                &raw,
                Error::Unsupported("MPEG-TS seek across source epochs"),
            );
            seeking.close();
            assert_eq!(pool.used(), before);
        }
    }
}

#[test]
fn selected_video_epoch_needs_its_own_preceding_random_access_picture() {
    for configuration in [false, true] {
        let mut ts = Transport::new();
        ts.tables(&[(0x1b, PID, &[])], 0);
        video(&mut ts, false, 30_000);
        let selected = if configuration {
            ts.tables(&[(0x24, PID, &[])], 1);
            epoch(0, 3)
        } else {
            ts.pcr(80_000, true);
            epoch(1, 2)
        };
        video(&mut ts, configuration, 90_000);
        let pool = Budget::new(32 << 20).unwrap();
        let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
        seeking
            .request_seek_in_epoch(request(80_000), selected)
            .unwrap();
        refusal(&mut seeking, &pool, &ts.bytes, NO_ENTRY);
    }
}

#[test]
fn replay_refuses_a_changed_landing_without_a_seek_receipt() {
    let make = |pts| {
        let mut ts = Transport::new();
        ts.tables(&[(0x0f, PID, &[])], 0);
        ts.pes(PID, 0xc0, &adts(b"earlier"), Some(90_000), None);
        ts.pcr(80_000, true);
        ts.pes(PID, 0xc0, &adts(b"landing"), Some(pts), None);
        ts.bytes
    };
    let raw = make(90_000);
    let changed = make(93_000);
    assert_eq!(raw.len(), changed.len());
    let pool = Budget::new(1 << 20).unwrap();
    let mut seeking = reader(&pool, &raw, PacketFormat::Ts188);
    seeking
        .request_seek_in_epoch(request(100_000), epoch(1, 2))
        .unwrap();
    let mut starts = 0;
    for _ in 0..100_000 {
        match seeking.step(&mut WorkBudget::new(17), &OFF) {
            Ok(Step::NeedInput(input)) => {
                if input.offset == 0 {
                    starts += 1;
                }
                let raw = if starts == 1 { &raw } else { &changed };
                let at = usize::try_from(input.offset).unwrap();
                if at == raw.len() {
                    seeking.finish(input.source, input.token).unwrap();
                } else {
                    let end = raw.len().min(at + input.maximum);
                    let bytes = Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap();
                    seeking
                        .provide(
                            &InputWindow::new(input.source, input.token, input.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Ok(Step::Ready(Event::Other(_)) | Step::Yield) => (),
            Err(Error::SourceMismatch) => {
                assert_eq!(starts, 2);
                return;
            }
            event => panic!("changed source produced an unexpected result: {event:?}"),
        }
    }
    panic!("changed source made no progress");
}

#[test]
fn selected_epoch_never_borrows_a_preceding_unit_from_another_epoch() {
    for configuration in [false, true] {
        let mut ts = Transport::new();
        ts.tables(&[(0x0f, PID, &[])], 0);
        ts.pes(
            PID,
            0xc0,
            &adts(b"eligible in old epoch"),
            Some(90_000),
            None,
        );
        let selected = if configuration {
            ts.tables(&[(0x0f, PID, &[0x0a, 4, b'e', b'n', b'g', 0])], 1);
            epoch(0, 3)
        } else {
            ts.pcr(80_000, true);
            epoch(1, 2)
        };
        ts.pes(PID, 0xc0, &adts(b"untimed in selected epoch"), None, None);
        ts.pes(PID, 0xc0, &adts(b"later than target"), Some(180_000), None);
        let pool = Budget::new(1 << 20).unwrap();
        for epochs in [selected, epoch(99, 99)] {
            let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
            seeking
                .request_seek_in_epoch(request(100_000), epochs)
                .unwrap();
            refusal(&mut seeking, &pool, &ts.bytes, NO_ENTRY);
        }
    }
}

#[test]
fn selected_clock_still_refuses_ambiguous_wraps_but_ignores_other_clocks() {
    let period = 1_u64 << 33;
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &adts(b"before wrap"), Some(period - 1), None);
    ts.pes(PID, 0xc0, &adts(b"after wrap"), Some(1), None);
    ts.pcr(80_000, true);
    ts.pes(PID, 0xc0, &adts(b"selected"), Some(90_000), None);
    let pool = Budget::new(1 << 20).unwrap();
    let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    seeking
        .request_seek_in_epoch(request(100_000), epoch(1, 2))
        .unwrap();
    let mut packets = 0;
    while let Some(event) = next(&mut seeking, &pool, &ts.bytes, 7, 1).unwrap() {
        if let Event::Packet(packet) = event {
            assert_eq!(packet.payload(), adts(b"selected"));
            packets += 1;
        }
    }
    assert_eq!(packets, 1);
    seeking
        .request_seek_in_epoch(request(period - 1), epoch(0, 2))
        .unwrap();
    refusal(
        &mut seeking,
        &pool,
        &ts.bytes,
        Error::Unsupported("MPEG-TS seek across an ambiguous clock wrap"),
    );
}

#[test]
fn explicit_selection_does_not_reuse_a_timestamp_only_index() {
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &adts(b"only epoch"), Some(90_000), None);
    let pool = Budget::new(1 << 20).unwrap();
    let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    seeking
        .request_seek_and_index(request(100_000), 16)
        .unwrap();
    while next(&mut seeking, &pool, &ts.bytes, 4096, 8192)
        .unwrap()
        .is_some()
    {}
    seeking
        .request_seek_in_epoch(request(100_000), epoch(1, 2))
        .unwrap();
    refusal(&mut seeking, &pool, &ts.bytes, NO_ENTRY);
}

#[test]
fn epoch_selection_cannot_replace_a_missing_clock_anchor() {
    use triergon_mpegts::clock::{
        ClockKey, SourceClockAnchor, SourceClockSample, SourceTimePolicy,
    };
    let ts = changed_video(false);
    let pool = Budget::new(32 << 20).unwrap();
    let mut seeking = Reader::new(
        &pool,
        ID,
        SourceCapabilities {
            length: Some(ts.bytes.len() as u64),
            random_access: true,
        },
        ReaderLimits {
            time_policy: SourceTimePolicy::Bounded(SourceClockAnchor {
                key: ClockKey {
                    tsid: 9,
                    program: PROGRAM,
                    pcr_pid: PID,
                    epoch: 0,
                },
                sample: SourceClockSample::Pts90(90_000),
                maximum_distance27: 90_000 * 300,
            }),
            ..ReaderLimits::default()
        },
        4096,
    )
    .unwrap();
    seeking
        .request_seek_in_epoch(request(112_000), epoch(1, 2))
        .unwrap();
    refusal(
        &mut seeking,
        &pool,
        &ts.bytes,
        Error::Unsupported("MPEG-TS seek point without a resolved PTS"),
    );
}

#[test]
fn tied_landings_stay_within_the_selected_epoch_and_keep_missing_dts() {
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &adts(b"earlier epoch"), Some(90_000), None);
    ts.pcr(80_000, true);
    for (bytes, pts) in [
        (b"first".as_slice(), 90_000),
        (b"last", 90_000),
        (b"tail", 93_000),
    ] {
        ts.pes(PID, 0xc0, &adts(bytes), Some(pts), None);
    }
    let pool = Budget::new(1 << 20).unwrap();
    let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    let mut target = request(91_000);
    target.policy = SeekPolicy::PreviousRandomAccess;
    seeking.request_seek_in_epoch(target, epoch(1, 2)).unwrap();
    let mut packets = Vec::new();
    let mut landed = false;
    while let Some(event) = next(&mut seeking, &pool, &ts.bytes, 7, 1).unwrap() {
        match event {
            Event::Seek(receipt) => {
                assert!(!landed);
                assert_eq!(receipt.landing, clock(90_000));
                assert_eq!(receipt.discard_until, clock(90_000));
                landed = true;
            }
            Event::Packet(packet) => {
                assert!(landed);
                assert_eq!(packet.epochs(), epoch(1, 2));
                assert_eq!(packet.dts(), None);
                packets.push(packet.payload().to_vec());
            }
            Event::Other(_) => (),
        }
    }
    assert!(landed);
    assert_eq!(packets, [adts(b"last"), adts(b"tail")]);
}

#[test]
fn epoch_selection_preserves_input_identity_and_seek_preconditions() {
    let ts = changed_video(false);
    let pool = Budget::new(32 << 20).unwrap();
    for (caps, limits, track, error) in [
        (
            SourceCapabilities {
                length: None,
                random_access: true,
            },
            ReaderLimits::default(),
            PID,
            Error::Unsupported("MPEG-TS seek needs one length-known seekable entity"),
        ),
        (
            SourceCapabilities {
                length: Some(ts.bytes.len() as u64),
                random_access: false,
            },
            ReaderLimits::default(),
            PID,
            Error::Unsupported("MPEG-TS seek needs one length-known seekable entity"),
        ),
        (
            SourceCapabilities {
                length: Some(ts.bytes.len() as u64),
                random_access: true,
            },
            ReaderLimits {
                maximum_seek_scan_bytes: 1,
                ..ReaderLimits::default()
            },
            PID,
            Error::Policy("MPEG-TS seek scan bytes"),
        ),
        (
            SourceCapabilities {
                length: Some(ts.bytes.len() as u64),
                random_access: true,
            },
            ReaderLimits {
                time_policy: triergon_mpegts::clock::SourceTimePolicy::Unanchored,
                ..ReaderLimits::default()
            },
            PID,
            Error::Unsupported("MPEG-TS seek needs a resolved clock"),
        ),
        (
            SourceCapabilities {
                length: Some(ts.bytes.len() as u64),
                random_access: true,
            },
            ReaderLimits::default(),
            0,
            Error::Invalid("MPEG-TS seek track is not selected"),
        ),
    ] {
        let mut seeking = Reader::new(&pool, ID, caps, limits, 4096).unwrap();
        let old = seeking.request().unwrap();
        let mut target = request(112_000);
        target.track = TrackId(track.into());
        assert_eq!(
            seeking
                .request_seek_in_epoch(target, epoch(1, 2))
                .unwrap_err(),
            error
        );
        let unchanged = seeking.request().unwrap();
        assert_eq!(unchanged.token, old.token);
        assert_eq!(unchanged.source, old.source);
        assert_eq!(unchanged.offset, old.offset);
        assert_eq!(unchanged.maximum, old.maximum);
    }
    let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    let old = seeking.request().unwrap();
    let input = seeking
        .request_seek_in_epoch(request(112_000), epoch(1, 2))
        .unwrap();
    assert_eq!(input.source, ID);
    assert_eq!(input.offset, 0);
    assert_ne!(input.token.generation(), old.token.generation());
    let bytes = Bytes::copy_from_slice(&pool, &ts.bytes[..1]).unwrap();
    assert!(matches!(
        seeking.provide(&InputWindow::new(ID, old.token, 0, bytes.clone()).unwrap()),
        Err(Error::StaleGeneration { .. })
    ));
    assert_eq!(
        seeking.provide(&InputWindow::new(SourceId(78), input.token, 0, bytes).unwrap()),
        Err(Error::SourceMismatch)
    );
    assert!(matches!(
        seeking.step(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
}
