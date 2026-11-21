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

#[test]
fn audio_seek_preserves_timed_and_untimed_frames() {
    for ac3 in [false, true] {
        let (mut ts, mut limits) = audio_eof_transport(ac3);
        let format = if ac3 {
            PacketFormat::M2ts192
        } else {
            PacketFormat::Ts188
        };
        limits.format = format;
        let frame = audio_eof_frame(ac3);
        let stream = if ac3 { 0xbd } else { 0xc0 };
        ts.pes(PID, stream, &frame, Some(90_000), None);
        ts.pes(PID, stream, &frame.repeat(2), Some(99_000), Some(98_910));
        ts.pes(PID, stream, &frame, None, None);
        ts.pes(PID, stream, &frame, Some(117_000), None);
        let bytes = if ac3 { ts.m2ts() } else { ts.bytes };
        for selected in [false, true] {
            let pool = Budget::new(1 << 20).unwrap();
            let caps = SourceCapabilities {
                length: Some(bytes.len() as u64),
                random_access: true,
            };
            let track = TrackId(PID.into());
            let mut reader = if selected {
                Reader::new_with_packet_tracks(&pool, ID, caps, limits, 4096, &[track])
            } else {
                Reader::new(&pool, ID, caps, limits, 4096)
            }
            .unwrap();
            let request = SeekRequest {
                track,
                target: clock(100_000),
                policy: SeekPolicy::PlaybackPreroll,
            };
            let generation = reader.request_seek(request).unwrap().token.generation();
            let mut landed = false;
            let mut clocks = Vec::new();
            while let Some(event) = next(&mut reader, &pool, &bytes, 127, 17).unwrap() {
                match event {
                    Event::Seek(result) => {
                        assert!(!landed);
                        assert_eq!(result.landing, clock(99_000));
                        assert_eq!(result.discard_until, request.target);
                        assert_eq!(result.scanned_bytes, bytes.len() as u64);
                        assert!(!result.replays_from_start);
                        assert_eq!(
                            reader.request().unwrap().token.generation() == generation,
                            selected
                        );
                        landed = true;
                    }
                    Event::Packet(packet) => {
                        assert!(landed);
                        assert_eq!(packet.payload(), frame);
                        let offset = ((clocks.len() + 1) * frame.len()) as u64;
                        assert_eq!(
                            packet.elementary_range(),
                            offset..offset + frame.len() as u64
                        );
                        clocks.push((packet.pts(), packet.dts()));
                    }
                    Event::Other(_) => (),
                }
            }
            assert_eq!(
                clocks,
                [
                    (Some(99_000), Some(98_910)),
                    (None, None),
                    (None, None),
                    (Some(117_000), None)
                ]
            );
        }
    }
}

#[test]
fn audio_seek_needs_a_source_timestamp_at_or_before_the_target() {
    let frame = adts(b"audio");
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &frame, None, None);
    ts.pes(PID, 0xc0, &frame, Some(117_000), None);
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    reader
        .request_seek(SeekRequest {
            track: TrackId(PID.into()),
            target: clock(100_000),
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .unwrap();
    loop {
        match next(&mut reader, &pool, &ts.bytes, 4096, 8192) {
            Ok(Some(Event::Other(_))) => (),
            Err(Error::Unsupported(
                "MPEG-TS has no timestamped audio frame at or before the target",
            )) => break,
            event => panic!("unexpected seek result {event:?}"),
        }
    }
}

#[test]
fn repeated_seeks_use_the_index_and_land_on_the_last_tied_timestamp() {
    let frame = adts(b"audio");
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    for pts in [90_000, 101_000, 101_000, 110_000] {
        ts.pes(PID, 0xc0, &frame, Some(pts), None);
    }
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    let mut request = SeekRequest {
        track: TrackId(PID.into()),
        target: clock(92_000),
        policy: SeekPolicy::PlaybackPreroll,
    };
    reader.request_seek_and_index(request, 3).unwrap();
    for (method, time, packet_offset) in [
        (SeekMethod::BoundedScan, clock(90_000), 0),
        (SeekMethod::Index, clock(101_000), 2 * frame.len()),
        (SeekMethod::BoundedScan, clock(101_000), 2 * frame.len()),
    ] {
        let result = loop {
            if let Event::Seek(result) = next(&mut reader, &pool, &ts.bytes, 127, 43)
                .unwrap()
                .unwrap()
            {
                break result;
            }
        };
        assert_eq!(result.method, method);
        assert_eq!(result.landing, time);
        assert_eq!(result.discard_until, request.target);
        assert_eq!(
            result.scanned_bytes,
            if method == SeekMethod::Index {
                0
            } else {
                ts.bytes.len() as u64
            }
        );
        let packet = match next(&mut reader, &pool, &ts.bytes, 127, 43)
            .unwrap()
            .unwrap()
        {
            Event::Packet(packet) => packet,
            event => panic!("expected landing packet, got {event:?}"),
        };
        assert_eq!(packet.elementary_range().start, packet_offset as u64);
        assert_eq!(packet.payload(), frame);
        if method == SeekMethod::Index {
            reader.reset().unwrap();
        }
        request.target = clock(105_000);
        reader.request_seek(request).unwrap();
    }
    reader.close();
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn a_full_or_nonmonotonic_index_falls_back_to_a_scan() {
    for (times, cap) in [
        ([90_000, 101_000, 110_000], 2),
        ([90_000, 110_000, 101_000], 3),
    ] {
        let frame = adts(b"audio");
        let mut ts = Transport::new();
        ts.tables(&[(0x0f, PID, &[])], 0);
        for pts in times {
            ts.pes(PID, 0xc0, &frame, Some(pts), None);
        }
        let pool = Budget::new(1 << 20).unwrap();
        let mut reader = reader(&pool, &ts.bytes, PacketFormat::Ts188);
        let request = SeekRequest {
            track: TrackId(PID.into()),
            target: clock(105_000),
            policy: SeekPolicy::PreviousRandomAccess,
        };
        reader.request_seek_and_index(request, cap).unwrap();
        for _ in 0..2 {
            loop {
                match next(&mut reader, &pool, &ts.bytes, 4096, 8192)
                    .unwrap()
                    .unwrap()
                {
                    Event::Seek(result) => {
                        assert_eq!(result.method, SeekMethod::BoundedScan);
                        assert_eq!(result.landing, clock(101_000));
                        break;
                    }
                    Event::Other(_) => (),
                    event @ Event::Packet(_) => panic!("unexpected {event:?}"),
                }
            }
            reader.request_seek(request).unwrap();
        }
    }
}

#[test]
fn layer_three_seek_replays_from_the_previous_empty_reservoir() {
    for (stream, header, length, low_bit) in [
        (0x03, [0xff, 0xfa, 0x14, 0xc0], 96, 7),
        (0x04, [0xff, 0xf3, 0x44, 0xc0], 96, 4),
    ] {
        let mut entry = vec![0; length];
        entry[..4].copy_from_slice(&header);
        let mut dependent = entry.clone();
        dependent[low_bit] = if low_bit == 7 { 0x80 } else { 1 };

        let mut ts = Transport::new();
        ts.tables(&[(stream, PID, &[])], 0);
        ts.pes(PID, 0xc0, &entry, Some(90_000), None);
        ts.pes(PID, 0xc0, &dependent, Some(93_000), None);
        ts.pes(PID, 0xc0, &entry, Some(96_000), None);
        let pool = Budget::new(1 << 20).unwrap();
        let mut reader = reader(&pool, &ts.bytes, PacketFormat::Ts188);
        reader
            .request_seek(SeekRequest {
                track: TrackId(PID.into()),
                target: clock(95_000),
                policy: SeekPolicy::PlaybackPreroll,
            })
            .unwrap();
        let mut landed = false;
        let mut output = Vec::new();
        while let Some(event) = next(&mut reader, &pool, &ts.bytes, 4096, 8192).unwrap() {
            match event {
                Event::Seek(result) => {
                    assert!(!landed);
                    assert_eq!(result.landing, clock(90_000));
                    assert_eq!(result.discard_until, clock(95_000));
                    landed = true;
                }
                Event::Packet(packet) => {
                    assert!(landed);
                    output.push(packet.payload().to_vec());
                }
                Event::Other(_) => (),
            }
        }
        assert_eq!(output, [entry.clone(), dependent, entry]);
    }
}

#[test]
fn seek_across_entities_replays_a_split_pes_and_continues_through_eof() {
    use triergon_mpegts::SeekEntity;
    let frame = adts(&[0x56; 250]);
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    let table_bytes = ts.bytes.len();
    for pts in [90_000, 105_000, 135_000] {
        ts.pes(PID, 0xc0, &frame, Some(pts), None);
    }
    let split = table_bytes + 3 * 188;
    let segments = [&ts.bytes[..split], &ts.bytes[split..]];
    let plan = [
        SeekEntity {
            source: ID,
            length: segments[0].len() as u64,
        },
        SeekEntity {
            source: SourceId(99),
            length: segments[1].len() as u64,
        },
    ];
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = reader(&pool, segments[0], PacketFormat::Ts188);
    let seek = |ticks| SeekRequest {
        track: TrackId(PID.into()),
        target: clock(ticks),
        policy: SeekPolicy::PlaybackPreroll,
    };
    for (target, landed, count, explicit_end, collect_index, method) in [
        (109_000, 105_000, 2, true, false, SeekMethod::BoundedScan),
        (90_000, 90_000, 3, false, false, SeekMethod::BoundedScan),
        (109_000, 105_000, 2, true, true, SeekMethod::BoundedScan),
        (90_000, 90_000, 3, false, false, SeekMethod::Index),
    ] {
        let request = if collect_index {
            reader.request_seek_in_entities_and_index(seek(target), &plan, 16)
        } else {
            reader.request_seek_in_entities(seek(target), &plan)
        }
        .unwrap();
        assert_eq!((request.source, request.offset), (ID, 0));
        let mut read = 0;
        let mut announced = false;
        for _ in 0..20_000 {
            match reader.step(&mut WorkBudget::new(39), &OFF).unwrap() {
                Step::NeedInput(request) => {
                    let segment = segments[usize::from(request.source == plan[1].source)];
                    let start = usize::try_from(request.offset).unwrap();
                    if request.maximum == 0 {
                        assert_eq!(request.source, plan[1].source);
                        reader.finish(request.source, request.token).unwrap();
                    } else {
                        let end = start + request.maximum.min(121);
                        let bytes = Bytes::copy_from_slice(&pool, &segment[start..end]).unwrap();
                        reader
                            .provide(
                                &InputWindow::new(
                                    request.source,
                                    request.token.clone(),
                                    request.offset,
                                    bytes,
                                )
                                .unwrap(),
                            )
                            .unwrap();
                        if explicit_end && request.source == ID && end == segment.len() {
                            reader.finish(request.source, request.token).unwrap();
                        }
                    }
                }
                Step::Ready(Event::Seek(result)) => {
                    assert!(!announced);
                    assert_eq!(result.landing, clock(landed));
                    assert_eq!(result.discard_until, clock(target));
                    let scanned = if method == SeekMethod::BoundedScan {
                        ts.bytes.len() as u64
                    } else {
                        0
                    };
                    assert_eq!(result.method, method);
                    assert_eq!(result.scanned_bytes, scanned);
                    announced = true;
                }
                Step::Ready(Event::Packet(packet)) => {
                    assert!(announced);
                    assert_eq!(packet.payload(), frame);
                    if packet.pts() == Some(105_000) {
                        let TraceEvent::Pes(pes) = packet.sources()[0].pes.event() else {
                            panic!("missing source PES");
                        };
                        assert!(pes.source_ranges().any(|range| range.entity == ID));
                        assert!(
                            pes.source_ranges()
                                .any(|range| range.entity == plan[1].source)
                        );
                    }
                    read += 1;
                }
                Step::Ready(Event::Other(_)) | Step::Yield => (),
                Step::End(end) => {
                    assert_eq!(end.source, plan[1].source);
                    break;
                }
                Step::Backpressure(pressure) => panic!("unexpected pressure {pressure:?}"),
            }
        }
        assert!(announced);
        assert_eq!(read, count);
    }
    reader.close();
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn indexed_seek_recognises_landing_after_a_transport_byte_run_change() {
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    let first = adts(&[0x51; 17]);
    let last = adts(&[0x52; 17]);
    ts.pes(PID, 0xc0, &first, Some(90_000), None);
    ts.counters[usize::from(PID)] = 3;
    ts.pes(PID, 0xc0, &last, Some(90_000), None);
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = reader(&pool, &ts.bytes, PacketFormat::Ts188);
    let request = SeekRequest {
        track: TrackId(PID.into()),
        target: clock(90_000),
        policy: SeekPolicy::PreviousRandomAccess,
    };
    reader.request_seek_and_index(request, 16).unwrap();
    for method in [SeekMethod::BoundedScan, SeekMethod::Index] {
        let mut packets = 0;
        while let Some(event) = next(&mut reader, &pool, &ts.bytes, 127, 39).unwrap() {
            match event {
                Event::Seek(landing) => assert_eq!(landing.method, method),
                Event::Packet(packet) => {
                    assert_eq!(packet.payload(), last);
                    packets += 1;
                }
                Event::Other(_) => (),
            }
        }
        assert_eq!(packets, 1);
        if method == SeekMethod::BoundedScan {
            reader.request_seek(request).unwrap();
        }
    }
}

#[test]
fn latm_seek_requires_a_complete_in_band_configuration() {
    // Version zero, one AAC-LC stereo layer at 48 kHz, frameLengthType zero.
    let configured = [0x56, 0xe0, 8, 0x20, 0, 0x11, 0x90, 0x1f, 0xe0, 0x0f, 0];
    let mut bad_length = configured;
    bad_length[9] = 0x27;
    // One payload using the previous StreamMuxConfig.
    let dependent = [0x56, 0xe0, 3, 0x80, 0xf0, 0];
    let mut ts = Transport::new();
    ts.tables(&[(0x11, PID, &[])], 0);
    ts.pes(PID, 0xc0, &configured, Some(90_000), None);
    ts.pes(PID, 0xc0, &dependent, Some(91_920), None);
    ts.pes(PID, 0xc0, &bad_length, Some(93_840), None);
    ts.pes(PID, 0xc0, &configured, Some(95_760), None);
    let pool = Budget::new(1 << 20).unwrap();
    for (target, landing, expected) in [(94_000, 90_000, 4), (96_000, 95_760, 1)] {
        let mut reader = reader(&pool, &ts.bytes, PacketFormat::Ts188);
        reader
            .request_seek(SeekRequest {
                track: TrackId(PID.into()),
                target: clock(target),
                policy: SeekPolicy::PlaybackPreroll,
            })
            .unwrap();
        let mut announced = false;
        let mut packets = 0;
        while let Some(event) = next(&mut reader, &pool, &ts.bytes, 7, 5).unwrap() {
            match event {
                Event::Seek(result) => {
                    assert!(!announced);
                    assert_eq!(result.landing, clock(landing));
                    assert_eq!(result.discard_until, clock(target));
                    announced = true;
                }
                Event::Packet(packet) => {
                    assert!(announced);
                    assert_eq!(packet.track_info().codec(), CodecId::AacLatm);
                    if packets == 0 {
                        assert_eq!(packet.payload(), configured);
                        assert_eq!(packet.pts(), Some(landing as i64));
                    }
                    if packets == 1 {
                        assert_eq!(packet.payload(), dependent);
                    }
                    packets += 1;
                }
                Event::Other(_) => (),
            }
        }
        assert!(announced);
        assert_eq!(packets, expected);
    }
}
