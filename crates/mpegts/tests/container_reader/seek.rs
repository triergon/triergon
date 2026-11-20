use super::*;
use triergon_core::container::{SeekMethod, SeekPolicy, SeekRequest};

fn sample(kind: u8, raw: &[u8], layout: &str, times: impl Fn(usize) -> u64) -> Transport {
    let mut ts = Transport::new();
    ts.tables(&[(kind, PID, &[])], 0);
    for (index, line) in layout.lines().enumerate() {
        let mut fields = line.split_ascii_whitespace();
        let start: usize = fields.next().unwrap().parse().unwrap();
        let length: usize = fields.next().unwrap().parse().unwrap();
        ts.pes(
            PID,
            0xe0,
            &raw[start..start + length],
            Some(times(index)),
            None,
        );
    }
    ts
}

fn clock(ticks: u64) -> Time {
    Time::new(ticks.into(), 90_000).unwrap()
}

fn reader(pool: &Budget, raw: &[u8], format: PacketFormat) -> Reader {
    Reader::new(
        pool,
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
    )
    .unwrap()
}

fn next(
    reader: &mut Reader,
    pool: &Budget,
    raw: &[u8],
    chunk: usize,
    grant: u64,
) -> Result<Option<Event<Packet, ReaderEvent>>, Error> {
    for _ in 0..1_000_000 {
        let mut work = WorkBudget::new(grant);
        let step = reader.step(&mut work, &OFF)?;
        assert!(work.spent() <= grant);
        match step {
            Step::Ready(event) => return Ok(Some(event)),
            Step::End(_) => return Ok(None),
            Step::Yield => (),
            Step::NeedInput(request) => {
                let offset = usize::try_from(request.offset).unwrap();
                if offset == raw.len() {
                    reader.finish(ID, request.token)?;
                } else {
                    let end = raw.len().min(offset + request.maximum.min(chunk));
                    let bytes = Bytes::copy_from_slice(pool, &raw[offset..end])?;
                    reader.provide(&InputWindow::new(ID, request.token, request.offset, bytes)?)?;
                }
            }
            Step::Backpressure(pressure) => panic!("unexpected backpressure {pressure:?}"),
        }
    }
    panic!("seek did not progress");
}

#[test]
fn cold_and_repeated_seeks_deliver_original_pictures_and_preroll() {
    for (kind, format, raw, layout, index, offset, chunk, grant) in [
        (
            0x1b,
            PacketFormat::Ts188,
            include_bytes!("../../../framing/fixtures/raw-annex-b/avc-aud0.h264").as_slice(),
            include_str!("../../../framing/fixtures/raw-annex-b/avc-aud0.packets"),
            4,
            3304,
            7,
            1,
        ),
        (
            0x24,
            PacketFormat::M2ts192,
            include_bytes!("../../../framing/fixtures/raw-annex-b/hevc-aud0.hevc").as_slice(),
            include_str!("../../../framing/fixtures/raw-annex-b/hevc-aud0.packets"),
            10,
            10257,
            4096,
            8192,
        ),
    ] {
        let ts = sample(kind, raw, layout, |i| 90_000 + i as u64 * 3000);
        let m2ts = ts.m2ts();
        let input = if format == PacketFormat::Ts188 {
            &ts.bytes
        } else {
            &m2ts
        };
        let first = 90_000 + index * 3000;
        let pool = Budget::new(32 << 20).unwrap();
        let mut reader = reader(&pool, input, format);
        for (policy, target, time, expected_offset) in [
            (SeekPolicy::PlaybackPreroll, first + 1300, first, offset),
            (SeekPolicy::PreviousRandomAccess, 91_300, 90_000, 0),
        ] {
            let old = reader.request().unwrap();
            let start = ContainerReader::request_seek(
                &mut reader,
                SeekRequest {
                    track: TrackId(PID.into()),
                    target: clock(target),
                    policy,
                },
            )
            .unwrap();
            assert_eq!(start.offset, 0);
            assert_ne!(start.token.generation(), old.token.generation());
            let stale = InputWindow::new(
                ID,
                old.token,
                old.offset,
                Bytes::copy_from_slice(&pool, &input[..1]).unwrap(),
            )
            .unwrap();
            assert!(matches!(
                reader.provide(&stale),
                Err(Error::StaleGeneration { .. })
            ));
            let mut receipt = None;
            let mut packets = 0;
            while let Some(event) = next(&mut reader, &pool, input, chunk, grant).unwrap() {
                match event {
                    Event::Seek(value) => assert!(receipt.replace(value).is_none()),
                    Event::Packet(packet) if packets == 0 => {
                        let proof = receipt.expect("seek receipt precedes packets");
                        assert_eq!(proof.landing, clock(time));
                        assert_eq!(proof.requested, clock(target));
                        assert_eq!(
                            proof.discard_until,
                            clock(if policy == SeekPolicy::PlaybackPreroll {
                                target
                            } else {
                                time
                            })
                        );
                        assert_eq!(proof.method, SeekMethod::BoundedScan);
                        assert_eq!(proof.scanned_bytes, input.len() as u64);
                        assert_eq!(proof.replays_from_start, expected_offset == 0);
                        assert_eq!(packet.pts(), Some(time as i64));
                        assert_eq!(packet.elementary_range().start, expected_offset);
                        let range = packet.elementary_range();
                        assert_eq!(
                            packet.payload(),
                            &raw[range.start as usize..range.end as usize]
                        );
                        packets += 1;
                    }
                    Event::Packet(_) => packets += 1,
                    Event::Other(_) => (),
                }
            }
            assert!(packets >= 2);
        }
    }
}

#[test]
fn seek_compares_presentation_times_even_when_access_points_are_reordered() {
    let raw = include_bytes!("../../../framing/fixtures/raw-annex-b/avc-aud0.h264");
    let layout = include_str!("../../../framing/fixtures/raw-annex-b/avc-aud0.packets");
    let ts = sample(0x1b, raw, layout, |i| match i {
        4 => 200_000,
        8 => 180_000,
        _ => 90_000 + i as u64 * 3000,
    });
    let pool = Budget::new(32 << 20).unwrap();
    let mut reader = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    reader
        .request_seek(SeekRequest {
            track: TrackId(PID.into()),
            target: clock(190_000),
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .unwrap();
    while let Some(event) = next(&mut reader, &pool, &ts.bytes, 4096, 8192).unwrap() {
        match event {
            Event::Seek(receipt) => {
                assert_eq!(receipt.landing, clock(180_000));
                return;
            }
            Event::Packet(_) => panic!("unexpected packet before seek"),
            Event::Other(_) => (),
        }
    }
    panic!("missing seek receipt");
}

#[test]
fn seek_refusals_do_not_masquerade_as_landings() {
    let raw = include_bytes!("../../../framing/fixtures/raw-annex-b/avc-aud0.h264");
    let layout = include_str!("../../../framing/fixtures/raw-annex-b/avc-aud0.packets");
    let ts = sample(0x1b, raw, layout, |i| 90_000 + i as u64 * 3000);
    let pool = Budget::new(32 << 20).unwrap();
    let request = SeekRequest {
        track: TrackId(PID.into()),
        target: clock(1),
        policy: SeekPolicy::PreviousRandomAccess,
    };
    let mut disabled = Reader::new(
        &pool,
        ID,
        SourceCapabilities {
            length: Some(ts.bytes.len() as u64),
            random_access: true,
        },
        ReaderLimits {
            maximum_seek_scan_bytes: 1024,
            ..ReaderLimits::default()
        },
        4096,
    )
    .unwrap();
    let old = disabled.request().unwrap().token;
    assert!(matches!(
        disabled.request_seek(request),
        Err(Error::Policy("MPEG-TS seek scan bytes"))
    ));
    assert_eq!(disabled.request().unwrap().token, old);

    let mut reader = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    reader.request_seek(request).unwrap();
    assert!(matches!(
        next(&mut reader, &pool, &ts.bytes, 4096, 8192),
        Err(Error::Unsupported(
            "MPEG-TS has no random-access picture at or before the target"
        ))
    ));
    reader.reset().unwrap();
    reader.request_seek(request).unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
}

#[test]
fn seek_refuses_an_ambiguous_period_zero_wrap() {
    let raw = include_bytes!("../../../framing/fixtures/raw-annex-b/avc-aud0.h264");
    let layout = include_str!("../../../framing/fixtures/raw-annex-b/avc-aud0.packets");
    let period = 1_u64 << 33;
    let ts = sample(0x1b, raw, layout, |i| {
        (period - 12_000 + i as u64 * 3000) % period
    });
    let pool = Budget::new(32 << 20).unwrap();
    let mut reader = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    reader
        .request_seek(SeekRequest {
            track: TrackId(PID.into()),
            target: clock(period - 1),
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .unwrap();
    assert!(matches!(
        next(&mut reader, &pool, &ts.bytes, 4096, 8192),
        Err(Error::Unsupported(
            "MPEG-TS seek across an ambiguous clock wrap"
        ))
    ));
}

#[test]
fn retained_seek_matches_replay_packets_tables_and_carrier_order() {
    let raw = include_bytes!("../../../framing/fixtures/raw-annex-b/avc-aud0.h264");
    let layout = include_str!("../../../framing/fixtures/raw-annex-b/avc-aud0.packets");
    let mut ts = sample(0x1b, raw, layout, |i| 90_000 + i as u64 * 3000);
    ts.tables(&[(0x1b, PID, &[])], 0);
    let mut traces = Vec::new();
    for selected in [false, true] {
        let pool = Budget::new(1 << 20).unwrap();
        let mut seeking = if selected {
            Reader::new_with_packet_tracks(
                &pool,
                ID,
                SourceCapabilities {
                    length: Some(ts.bytes.len() as u64),
                    random_access: true,
                },
                ReaderLimits::default(),
                4096,
                &[TrackId(PID.into())],
            )
            .unwrap()
        } else {
            reader(&pool, &ts.bytes, PacketFormat::Ts188)
        };
        let request = seeking
            .request_seek(SeekRequest {
                track: TrackId(PID.into()),
                target: clock(107_000),
                policy: SeekPolicy::PlaybackPreroll,
            })
            .unwrap();
        let generation = request.token.generation();
        let mut trace = Vec::new();
        while let Some(event) = next(&mut seeking, &pool, &ts.bytes, 4096, 4096).unwrap() {
            let signature = match event {
                Event::Seek(landing) => {
                    assert_eq!(
                        seeking.request().unwrap().token.generation() == generation,
                        selected
                    );
                    format!("seek {landing:?}")
                }
                Event::Packet(packet) => format!(
                    "packet {:?} {:?} {}",
                    packet.elementary_range(),
                    packet.pts(),
                    packet.payload().len()
                ),
                Event::Other(ReaderEvent::Transport(packet)) => {
                    format!("carrier {} {}", packet.source_offset, packet.pid)
                }
                Event::Other(ReaderEvent::Continuity { pid, outcome }) => {
                    format!("continuity {pid} {outcome:?}")
                }
                Event::Other(ReaderEvent::Source(source)) => match source.event() {
                    TraceEvent::Pat(_) => format!("PAT {}", seeking.tracks().len()),
                    TraceEvent::ProgramMap { active, .. } => {
                        format!("PMT {active} {}", seeking.tracks().len())
                    }
                    TraceEvent::Pes(pes) => {
                        format!("PES {} {}", pes.elementary_offset, pes.payload_len)
                    }
                    TraceEvent::Finished => "end".into(),
                    _ => continue,
                },
                Event::Other(_) => continue,
            };
            trace.push(signature);
        }
        seeking.close();
        assert_eq!(pool.used(), Budget::control_storage_bytes());
        traces.push(trace);
    }
    assert_eq!(traces[0], traces[1]);
}
