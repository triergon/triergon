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
                        Event::Seek(result) => {
                            assert!(!landed);
                            assert_eq!(result.requested, target.target);
                            assert_eq!(result.landing, landing.clock().unwrap().pts.exact.unwrap());
                            assert_eq!(result.discard_until, target.target);
                            assert_eq!(result.method, SeekMethod::BoundedScan);
                            assert_eq!(result.scanned_bytes, raw.len() as u64);
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
fn replay_refuses_a_changed_landing_without_a_seek_result() {
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
            Event::Seek(result) => {
                assert!(!landed);
                assert_eq!(result.landing, clock(90_000));
                assert_eq!(result.discard_until, clock(90_000));
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
fn seek_picks_the_last_tied_packet_after_a_table_version_change() {
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &adts(b"first"), Some(90_000), None);
    ts.tables(&[(0x0f, PID, &[])], 1);
    ts.pes(PID, 0xc0, &adts(b"last"), Some(90_000), None);
    ts.pes(PID, 0xc0, &adts(b"tail"), Some(93_000), None);
    let pool = Budget::new(1 << 20).unwrap();
    let original = sequential(&pool, &ts.bytes, PacketFormat::Ts188);
    assert_eq!(original[0].epochs(), original[1].epochs());
    assert_eq!(
        original[0].elementary_range().start,
        original[1].elementary_range().start
    );
    let expected: Vec<_> = original[1..].iter().map(snapshot).collect();
    for explicit in [false, true] {
        let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
        let target = request(91_000);
        if explicit {
            seeking
                .request_seek_in_epoch(target, original[1].epochs())
                .unwrap();
        } else {
            seeking.request_seek(target).unwrap();
        }
        let mut packets = Vec::new();
        while let Some(event) = next(&mut seeking, &pool, &ts.bytes, 7, 1).unwrap() {
            if let Event::Packet(packet) = event {
                packets.push(snapshot(&packet));
            }
        }
        assert_eq!(packets, expected);
    }
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

// Physical entities retain independent offsets, including a PES straddling EOF.
fn entity_suffix(
    reader: &mut Reader,
    pool: &Budget,
    parts: &[&[u8]],
    plan: &[triergon_mpegts::SeekEntity],
) -> (SeekMethod, Vec<Snapshot>) {
    let mut method = None;
    let mut packets = Vec::new();
    for _ in 0..1_000_000 {
        match reader.step(&mut WorkBudget::new(17), &OFF).unwrap() {
            Step::NeedInput(input) => {
                let part = parts[plan
                    .iter()
                    .position(|entry| entry.source == input.source)
                    .unwrap()];
                let at = usize::try_from(input.offset).unwrap();
                if at == part.len() {
                    reader.finish(input.source, input.token).unwrap();
                } else {
                    let end = part.len().min(at + input.maximum.min(7));
                    let bytes = Bytes::copy_from_slice(pool, &part[at..end]).unwrap();
                    reader
                        .provide(
                            &InputWindow::new(input.source, input.token, input.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::Ready(Event::Seek(seek)) => {
                assert!(method.replace(seek.method).is_none());
                assert_eq!(
                    seek.scanned_bytes,
                    if seek.method == SeekMethod::Index {
                        0
                    } else {
                        plan.iter().map(|entry| entry.length).sum()
                    }
                );
            }
            Step::Ready(Event::Packet(packet)) => {
                assert!(method.is_some());
                packets.push(snapshot(&packet));
            }
            Step::Ready(Event::Other(_)) | Step::Yield => (),
            Step::End(end) => {
                assert_eq!(end.source, plan.last().unwrap().source);
                return (method.unwrap(), packets);
            }
            Step::Backpressure(pressure) => panic!("unexpected pressure {pressure:?}"),
        }
    }
    panic!("entity seek made no progress");
}

#[test]
fn entity_epoch_indexes_preserve_codec_changes_ties_and_exact_suffixes() {
    use triergon_mpegts::SeekEntity;
    for configuration in [false, true] {
        let ts = changed_video(configuration);
        for format in [PacketFormat::Ts188, PacketFormat::M2ts192] {
            let raw = if format == PacketFormat::Ts188 {
                ts.bytes.clone()
            } else {
                ts.m2ts()
            };
            let carrier = if format == PacketFormat::Ts188 {
                188
            } else {
                192
            };
            // The first video PES spans this carrier boundary.
            let split = 4 * carrier;
            let parts = [&raw[..split], &raw[split..]];
            let plan = [
                SeekEntity {
                    source: ID,
                    length: split as u64,
                },
                SeekEntity {
                    source: SourceId(99),
                    length: (raw.len() - split) as u64,
                },
            ];
            let pool = Budget::new(32 << 20).unwrap();
            let mut seeking = reader(&pool, parts[0], format);
            let selected = if configuration {
                epoch(0, 3)
            } else {
                epoch(1, 2)
            };
            let mut expected = Vec::new();
            for (epochs, build, method) in [
                (selected, true, SeekMethod::BoundedScan),
                (selected, false, SeekMethod::Index),
                (epoch(0, 2), false, SeekMethod::BoundedScan),
                (selected, true, SeekMethod::BoundedScan),
                (selected, false, SeekMethod::Index),
            ] {
                if build {
                    seeking
                        .request_seek_in_entities_in_epoch_and_index(
                            request(112_000),
                            &plan,
                            epochs,
                            128,
                        )
                        .unwrap();
                } else {
                    seeking
                        .request_seek_in_entities_in_epoch(request(112_000), &plan, epochs)
                        .unwrap();
                }
                let (actual, suffix) = entity_suffix(&mut seeking, &pool, &parts, &plan);
                assert_eq!(actual, method);
                assert!(!suffix.is_empty());
                assert_eq!(suffix[0].epochs, epochs);
                if epochs == selected {
                    if expected.is_empty() {
                        expected = suffix;
                    } else {
                        assert_eq!(suffix, expected);
                    }
                }
            }
            // Changing an entity identity invalidates reuse, while preserving bytes.
            let mut changed = plan;
            changed[1].source = SourceId(100);
            seeking
                .request_seek_in_entities_in_epoch(request(112_000), &changed, selected)
                .unwrap();
            assert_eq!(
                entity_suffix(&mut seeking, &pool, &parts, &changed).0,
                SeekMethod::BoundedScan
            );
            // Identical IDs with a different carrier-aligned cut are a different plan.
            let recut = split + carrier;
            let recut_parts = [&raw[..recut], &raw[recut..]];
            let mut resized = changed;
            resized[0].length = recut as u64;
            resized[1].length = (raw.len() - recut) as u64;
            seeking
                .request_seek_in_entities_in_epoch(request(112_000), &resized, selected)
                .unwrap();
            assert_eq!(
                entity_suffix(&mut seeking, &pool, &recut_parts, &resized).0,
                SeekMethod::BoundedScan
            );
            seeking.close();
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn explicit_epoch_index_bounds_and_ties_match_the_full_scan() {
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pcr(80_000, true);
    for (label, pts) in [
        (b"first".as_slice(), 90_000),
        (b"last", 90_000),
        (b"tail", 93_000),
    ] {
        ts.pes(PID, 0xc0, &adts(label), Some(pts), None);
    }
    let pool = Budget::new(1 << 20).unwrap();
    for (limit, method) in [(1, SeekMethod::BoundedScan), (2, SeekMethod::Index)] {
        let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
        for build in [true, false] {
            if build {
                seeking
                    .request_seek_in_epoch_and_index(request(91_000), epoch(1, 2), limit)
                    .unwrap();
            } else {
                seeking
                    .request_seek_in_epoch(request(91_000), epoch(1, 2))
                    .unwrap();
            }
            let mut received = Vec::new();
            while let Some(event) = next(&mut seeking, &pool, &ts.bytes, 7, 1).unwrap() {
                match event {
                    Event::Seek(seek) => assert_eq!(
                        seek.method,
                        if build {
                            SeekMethod::BoundedScan
                        } else {
                            method
                        }
                    ),
                    Event::Packet(packet) => received.push(packet.payload().to_vec()),
                    Event::Other(_) => (),
                }
            }
            assert_eq!(received, [adts(b"last"), adts(b"tail")]);
        }
    }
}

#[test]
fn indexed_epoch_landing_keeps_a_pes_split_between_entities() {
    use triergon_mpegts::SeekEntity;
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &adts(b"old epoch"), Some(90_000), None);
    ts.pcr(80_000, true);
    let start = ts.bytes.len();
    let frame = adts(&[0x56; 250]);
    ts.pes(PID, 0xc0, &frame, Some(90_000), None);
    ts.pes(PID, 0xc0, &adts(b"untimed tail"), None, None);
    let split = start + 188;
    let parts = [&ts.bytes[..split], &ts.bytes[split..]];
    let plan = [
        SeekEntity {
            source: ID,
            length: split as u64,
        },
        SeekEntity {
            source: SourceId(99),
            length: parts[1].len() as u64,
        },
    ];
    let pool = Budget::new(1 << 20).unwrap();
    let mut seeking = reader(&pool, parts[0], PacketFormat::Ts188);
    let mut expected = Vec::new();
    for build in [true, false] {
        if build {
            seeking
                .request_seek_in_entities_in_epoch_and_index(request(91_000), &plan, epoch(1, 2), 1)
                .unwrap();
        } else {
            seeking
                .request_seek_in_entities_in_epoch(request(91_000), &plan, epoch(1, 2))
                .unwrap();
        }
        let (method, suffix) = entity_suffix(&mut seeking, &pool, &parts, &plan);
        assert_eq!(
            method,
            if build {
                SeekMethod::BoundedScan
            } else {
                SeekMethod::Index
            }
        );
        assert_eq!(suffix.len(), 2);
        assert_eq!(suffix[0].bytes, frame);
        assert_eq!(suffix[1].pts, None);
        let ranges = &suffix[0].origins[0].ranges;
        assert!(ranges.iter().any(|range| range.entity == ID));
        assert!(ranges.iter().any(|range| range.entity == plan[1].source));
        if build {
            expected = suffix;
        } else {
            assert_eq!(suffix, expected);
        }
    }
}
