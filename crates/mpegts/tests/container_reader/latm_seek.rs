use super::*;

// Author the syntax fields, including each length immediately before its payload.
fn envelope(count: u8, payloads: &[&[u8]]) -> Vec<u8> {
    mux_envelope(count, payloads, true)
}

fn mux_envelope(count: u8, payloads: &[&[u8]], configured: bool) -> Vec<u8> {
    let mut mux = Vec::new();
    let mut position = 0;
    let mut write = |value: u16, width: usize| {
        for shift in (0..width).rev() {
            if position % 8 == 0 {
                mux.push(0);
            }
            *mux.last_mut().unwrap() |= ((value >> shift & 1) as u8) << (7 - position % 8);
            position += 1;
        }
    };
    if configured {
        write(1, 3); // in-band, version zero, common time framing
        write(u16::from(count - 1), 6);
        write(0, 7); // one program, one layer
        write(0x1190, 16); // AAC-LC, 48 kHz stereo
        write(0, 3); // frameLengthType
        write(255, 8); // buffer fullness
        write(0, 2); // no other data or CRC
    } else {
        write(1, 1); // useSameStreamMux
    }
    for payload in payloads {
        let mut length = payload.len();
        while length >= 255 {
            write(255, 8);
            length -= 255;
        }
        write(length as u16, 8);
        for &byte in *payload {
            write(u16::from(byte), 8);
        }
    }
    let [high, low] = (mux.len() as u16).to_be_bytes();
    let mut frame = vec![0x56, 0xe0 | high, low];
    frame.extend(mux);
    frame
}

fn set(frame: &mut [u8], bit: usize, width: usize, value: u16) {
    for shift in 0..width {
        let position = bit + shift;
        let mask = 1 << (7 - position % 8);
        frame[3 + position / 8] &= !mask;
        frame[3 + position / 8] |= ((value >> (width - 1 - shift) & 1) as u8) * mask;
    }
}

#[test]
fn latm_subframes_preserve_complete_split_envelopes_and_indexed_suffixes() {
    check_split_suffixes(false);
}

#[test]
fn latm_large_inherited_envelopes_preserve_clocks_sources_and_indexed_suffixes() {
    check_split_suffixes(true);
}

fn check_split_suffixes(inherited: bool) {
    for count in [2, 14, 64] {
        let large = [0x55; 510]; // Includes a terminating zero length octet.
        let small = [0x33; 3];
        let mut payloads = vec![
            if count == 14 {
                large.as_slice()
            } else {
                small.as_slice()
            };
            count
        ];
        payloads[0] = &large;
        let frame = envelope(count as u8, &payloads);
        let dependent = if inherited {
            mux_envelope(count as u8, &payloads, false)
        } else {
            vec![0x56, 0xe0, 3, 0x80, 0xf0, 0]
        };
        let mut ts = Transport::new();
        ts.tables(&[(0x11, PID, &[])], 0);
        ts.pes(PID, 0xc0, &frame[..9], Some(90_000), Some(89_000));
        ts.pes(PID, 0xc0, &frame[9..], Some(91_000), None);
        if inherited {
            ts.pes(PID, 0xc0, &dependent, Some(93_000), None);
        }
        ts.pes(PID, 0xc0, &dependent, None, None);
        let pool = Budget::new(1 << 20).unwrap();
        let mut baseline = reader(&pool, &ts.bytes, PacketFormat::Ts188);
        let mut original = Vec::new();
        while let Some(event) = next(&mut baseline, &pool, &ts.bytes, 7, 1).unwrap() {
            if let Event::Packet(packet) = event {
                original.push(packet);
            }
        }
        assert_eq!(original.len(), if inherited { 3 } else { 2 });
        assert_eq!(original[0].sources().len(), 2);
        assert_eq!(original[0].pts(), Some(90_000));
        assert_eq!(original[0].dts(), Some(89_000));
        if inherited {
            assert_eq!(original[1].pts(), Some(93_000));
        }
        assert_eq!(original.last().unwrap().pts(), None);
        let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
        let request = SeekRequest {
            track: TrackId(PID.into()),
            target: clock(95_000),
            policy: SeekPolicy::PlaybackPreroll,
        };
        seeking.request_seek_and_index(request, 8).unwrap();
        for method in [SeekMethod::BoundedScan, SeekMethod::Index] {
            let mut announced = false;
            let mut index = 0;
            while let Some(event) = next(&mut seeking, &pool, &ts.bytes, 1, 1).unwrap() {
                match event {
                    Event::Seek(seek) => {
                        assert_eq!(seek.method, method);
                        assert_eq!(seek.landing, clock(90_000));
                        assert_eq!(seek.discard_until, request.target);
                        announced = true;
                    }
                    Event::Packet(packet) => {
                        assert!(announced);
                        let expected = &original[index];
                        assert_eq!(packet.payload(), expected.payload());
                        assert_eq!(packet.elementary_range(), expected.elementary_range());
                        assert_eq!(packet.pts(), expected.pts());
                        assert_eq!(packet.dts(), expected.dts());
                        assert_eq!(packet.clock().is_some(), expected.clock().is_some());
                        if let Some((a, b)) = packet.clock().zip(expected.clock()) {
                            assert_eq!(a.key, b.key);
                            assert_eq!(a.quarantined, b.quarantined);
                            assert_eq!(a.pts, b.pts);
                            assert_eq!(a.dts, b.dts);
                        }
                        assert_eq!(packet.epochs(), expected.epochs());
                        assert_eq!(packet.duration(), None);
                        assert_eq!(
                            packet.track_info().configuration_epoch(),
                            expected.track_info().configuration_epoch()
                        );
                        assert_eq!(packet.sources().len(), expected.sources().len());
                        for (actual, expected) in packet.sources().iter().zip(expected.sources()) {
                            assert_eq!(actual.payload_range, expected.payload_range);
                            assert_eq!(actual.elementary_range, expected.elementary_range);
                            let TraceEvent::Pes(a) = actual.pes.event() else {
                                panic!("PES source");
                            };
                            let TraceEvent::Pes(b) = expected.pes.event() else {
                                panic!("PES source");
                            };
                            assert_eq!(
                                a.source_ranges().collect::<Vec<_>>(),
                                b.source_ranges().collect::<Vec<_>>()
                            );
                            assert_eq!(
                                a.header_source_ranges.as_slice(),
                                b.header_source_ranges.as_slice()
                            );
                        }
                        index += 1;
                    }
                    Event::Other(_) => (),
                }
            }
            assert!(announced);
            assert_eq!(index, original.len());
            if method == SeekMethod::BoundedScan {
                seeking.request_seek(request).unwrap();
            }
        }
        drop(original);
        baseline.close();
        seeking.close();
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn latm_subframes_refuse_incomplete_or_ambiguous_landings() {
    let valid = envelope(2, &[&[1, 2], &[3, 4, 5]]);
    let mut cases = Vec::new();
    // Exact field offsets in AudioMuxElement, not offsets guessed from bytes.
    for (bit, width, value) in [
        (0, 1, 1),    // useSameStreamMux
        (1, 1, 1),    // audioMuxVersion
        (2, 1, 0),    // asynchronous framing
        (9, 4, 1),    // multiple programs
        (13, 3, 1),   // multiple layers
        (16, 5, 5),   // explicit SBR instead of AAC-LC
        (21, 4, 15),  // explicit frequency
        (25, 4, 0),   // program config element
        (29, 1, 1),   // 960 samples
        (30, 1, 1),   // core coder dependency
        (31, 1, 1),   // extension flag
        (32, 3, 1),   // fixed frame length
        (43, 1, 1),   // other data
        (44, 1, 1),   // CRC
        (3, 6, 2),    // missing third subframe
        (3, 6, 0),    // undeclared second subframe
        (69, 8, 200), // second payload exceeds envelope
    ] {
        let mut frame = valid.clone();
        set(&mut frame, bit, width, value);
        cases.push(frame);
    }
    cases.push(envelope(2, &[&[1], &[]]));
    cases.push(envelope(2, &[&[], &[1]]));
    // Complete LOAS length, but an incomplete final LATM payload.
    let mut truncated = valid.clone();
    truncated.pop();
    truncated[2] -= 1;
    cases.push(truncated);
    let mut extra = valid.clone();
    extra.push(0);
    extra[2] += 1;
    cases.push(extra);
    let pool = Budget::new(1 << 20).unwrap();
    for (index, frame) in cases.iter().enumerate() {
        let mut ts = Transport::new();
        ts.tables(&[(0x11, PID, &[])], 0);
        ts.pes(PID, 0xc0, frame, Some(90_000), None);
        let mut seeking = reader(&pool, &ts.bytes, PacketFormat::Ts188);
        seeking
            .request_seek(SeekRequest {
                track: TrackId(PID.into()),
                target: clock(95_000),
                policy: SeekPolicy::PreviousRandomAccess,
            })
            .unwrap();
        loop {
            match next(&mut seeking, &pool, &ts.bytes, 7, 1) {
                Err(Error::Unsupported(_)) => break,
                Ok(Some(Event::Other(_))) => (),
                result => panic!("case {index} accepted or failed unexpectedly: {result:?}"),
            }
        }
        seeking.close();
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[derive(Debug, PartialEq, Eq)]
struct EntityPacket {
    payload: Vec<u8>,
    pts: Option<i64>,
    range: std::ops::Range<u64>,
    sources: Vec<EntityPacketSource>,
}

#[derive(Debug, PartialEq, Eq)]
struct EntityPacketSource {
    elementary: std::ops::Range<u64>,
    payload: std::ops::Range<usize>,
    header_ranges: Vec<triergon_mpegts::SourceRange>,
    payload_ranges: Vec<triergon_mpegts::SourceRange>,
}

fn entity_packet(packet: &Packet) -> EntityPacket {
    EntityPacket {
        payload: packet.payload().to_vec(),
        pts: packet.pts(),
        range: packet.elementary_range(),
        sources: packet
            .sources()
            .iter()
            .map(|source| {
                let TraceEvent::Pes(pes) = source.pes.event() else {
                    panic!("PES source")
                };
                EntityPacketSource {
                    elementary: source.elementary_range.clone(),
                    payload: source.payload_range.clone(),
                    header_ranges: pes.header_source_ranges.as_slice().to_vec(),
                    payload_ranges: pes.source_ranges().collect(),
                }
            })
            .collect(),
    }
}

#[derive(Clone, Copy, Debug)]
enum EntityMode {
    Replay,
    Retain,
    PressureBefore,
    PressureAfter,
    CancelBefore,
    CancelAfter,
    Multiple,
    Epoch,
    Indexed,
}

fn entity_run(
    ts: &Transport,
    split: usize,
    mode: EntityMode,
) -> (
    triergon_core::container::SeekReceipt,
    Vec<EntityPacket>,
    usize,
) {
    use triergon_mpegts::SeekEntity;
    let parts = [&ts.bytes[..split], &ts.bytes[split..]];
    let plan = [
        SeekEntity {
            source: ID,
            length: parts[0].len() as u64,
        },
        SeekEntity {
            source: SourceId(99),
            length: parts[1].len() as u64,
        },
    ];
    let pool = Budget::new(1 << 20).unwrap();
    let caps = SourceCapabilities {
        length: Some(parts[0].len() as u64),
        random_access: true,
    };
    let tracks = if matches!(mode, EntityMode::Multiple) {
        vec![TrackId(PID.into()), TrackId(257)]
    } else {
        vec![TrackId(PID.into())]
    };
    let mut reader = if matches!(mode, EntityMode::Replay) {
        Reader::new(&pool, ID, caps, ReaderLimits::default(), 4096).unwrap()
    } else {
        Reader::new_with_packet_tracks(&pool, ID, caps, ReaderLimits::default(), 4096, &tracks)
            .unwrap()
    };
    let seek = SeekRequest {
        track: TrackId(PID.into()),
        target: clock(95_000),
        policy: SeekPolicy::PlaybackPreroll,
    };
    let start = |reader: &mut Reader| {
        if matches!(mode, EntityMode::Epoch) {
            reader
                .request_seek_in_entities_in_epoch(
                    seek,
                    &plan,
                    triergon_mpegts::SourceEpochs {
                        clock: 0,
                        configuration: 2,
                    },
                )
                .unwrap();
        } else if matches!(mode, EntityMode::Indexed) {
            reader
                .request_seek_in_entities_and_index(seek, &plan, 8)
                .unwrap();
        } else {
            reader.request_seek_in_entities(seek, &plan).unwrap();
        }
    };
    start(&mut reader);
    let mut injected = false;
    let mut reservation = None;
    let mut prefix = 0;
    let mut receipt = None;
    let mut packets = Vec::new();
    for _ in 0..100_000 {
        match reader.step(&mut WorkBudget::new(97), &OFF) {
            Ok(Step::NeedInput(input)) => {
                let part = if input.source == ID {
                    parts[0]
                } else {
                    parts[1]
                };
                let at = input.offset as usize;
                if input.source == ID && at == 0 && input.maximum > 0 {
                    prefix += 1;
                }
                if at == part.len() {
                    reader.finish(input.source, input.token).unwrap();
                    continue;
                }
                let end = part.len().min(at + input.maximum.min(188));
                let bytes = Bytes::copy_from_slice(&pool, &part[at..end]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(input.source, input.token, input.offset, bytes).unwrap(),
                    )
                    .unwrap();
                let after = input.source != ID;
                if !injected
                    && ((matches!(mode, EntityMode::PressureBefore | EntityMode::CancelBefore)
                        && input.source == ID
                        && at >= 188)
                        || (matches!(mode, EntityMode::PressureAfter | EntityMode::CancelAfter)
                            && after))
                {
                    injected = true;
                    if matches!(mode, EntityMode::CancelBefore | EntityMode::CancelAfter) {
                        assert!(matches!(
                            reader.step(&mut WorkBudget::new(1), &AtomicBool::new(true)),
                            Err(Error::Cancelled)
                        ));
                        reader.reset().unwrap();
                        start(&mut reader);
                    } else {
                        reservation =
                            Some(pool.reserve(pool.available().saturating_sub(64)).unwrap());
                    }
                }
            }
            Ok(Step::Ready(Event::Seek(value))) => {
                assert!(receipt.replace(value).is_none());
            }
            Ok(Step::Ready(Event::Packet(packet))) => {
                assert!(receipt.is_some());
                packets.push(entity_packet(&packet));
            }
            Ok(Step::Ready(Event::Other(_)) | Step::Yield) => (),
            Ok(Step::Backpressure(_)) | Err(Error::Backpressure { .. })
                if reservation.is_some() =>
            {
                reservation = None;
            }
            Ok(Step::End(end)) => {
                assert_eq!(end.source, SourceId(99));
                assert!(reservation.is_none());
                reader.close();
                assert_eq!(pool.used(), Budget::control_storage_bytes());
                return (receipt.unwrap(), packets, prefix);
            }
            other => panic!("{mode:?}: {other:?}"),
        }
    }
    panic!("{mode:?} did not finish")
}

#[test]
fn entity_retention_matches_replay_for_split_latm_and_fallbacks() {
    let first = envelope(1, &[&[0x11; 300]]);
    let second = mux_envelope(1, &[&[0x22; 30]], false);
    let third = mux_envelope(1, &[&[0x33; 31]], false);
    let mut ts = Transport::new();
    ts.tables(&[(0x11, PID, &[])], 0);
    ts.pes(PID, 0xc0, &first, Some(90_000), None);
    let split = 3 * 188;
    ts.pes(PID, 0xc0, &second, Some(93_000), None);
    ts.pes(PID, 0xc0, &third, None, None);
    let (receipt, packets, reads) = entity_run(&ts, split, EntityMode::Replay);
    assert_eq!(reads, 2);
    assert_eq!(
        packets
            .iter()
            .map(|p| p.payload.clone())
            .collect::<Vec<_>>(),
        [first, second, third]
    );
    assert_eq!(packets[0].sources.len(), 1);
    assert_eq!(
        packets[0].sources[0],
        EntityPacketSource {
            elementary: 0..311,
            payload: 0..311,
            header_ranges: vec![triergon_mpegts::SourceRange {
                entity: ID,
                offset: 380,
                bytes: 14
            }],
            payload_ranges: vec![
                triergon_mpegts::SourceRange {
                    entity: ID,
                    offset: 394,
                    bytes: 170
                },
                triergon_mpegts::SourceRange {
                    entity: SourceId(99),
                    offset: 47,
                    bytes: 141
                }
            ],
        }
    );
    for mode in [
        EntityMode::Retain,
        EntityMode::PressureBefore,
        EntityMode::PressureAfter,
        EntityMode::CancelBefore,
        EntityMode::CancelAfter,
        EntityMode::Multiple,
        EntityMode::Epoch,
        EntityMode::Indexed,
    ] {
        let (actual_receipt, actual_packets, reads) = entity_run(&ts, split, mode);
        assert_eq!(actual_receipt, receipt, "{mode:?}");
        assert_eq!(actual_packets, packets, "{mode:?}");
        assert_eq!(
            reads,
            if matches!(mode, EntityMode::Retain) {
                1
            } else {
                2
            },
            "{mode:?}"
        );
    }
}

#[test]
fn entity_retention_stays_disabled_over_size_limit() {
    let frame = envelope(1, &[&[0x41; 300]]);
    let mut ts = Transport::new();
    ts.tables(&[(0x11, PID, &[])], 0);
    ts.pes(PID, 0xc0, &frame, Some(90_000), None);
    for _ in 0..1400 {
        ts.payload(0x1fff, &[0]);
    }
    let split = 3 * 188;
    let (expected, packets, _) = entity_run(&ts, split, EntityMode::Replay);
    let (actual, output, reads) = entity_run(&ts, split, EntityMode::Retain);
    assert_eq!(actual, expected);
    assert_eq!(output, packets);
    assert_eq!(reads, 2);
}
