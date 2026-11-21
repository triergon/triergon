//! Source facts from original transport inputs.
#[path = "../../../xtask/src/sha256.rs"]
mod sha256;

use std::{path::PathBuf, sync::atomic::AtomicBool};
use triergon_core::container::{CodecId, Event, TrackInfo};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::compat::input::ts188_carrier_offset;
use triergon_mpegts::{
    FaultKind, PacketFormat, ParseLimits, Reader, ReaderEvent, ReaderLimits, TraceEvent,
    TransportStream,
};

fn original(name: &str, hash: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.research/fate-samples/mpegts")
        .join(name);
    assert_eq!(sha256::file_hex(&path).unwrap(), hash);
    let bytes = std::fs::read(&path).expect("original FATE sample in the local cache");
    assert_eq!(sha256::hex(&bytes), hash, "original input changed");
    bytes
}

// Polynomial division independent of the production incremental section CRC.
fn crc(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for byte in bytes {
        value ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            value = (value << 1)
                ^ if value & 0x8000_0000 == 0 {
                    0
                } else {
                    0x04c1_1db7
                };
        }
    }
    value
}

// An empty packet selection is the public source-only program reader. It must
// still discover every track, retain source clocks and report malformed input.
fn read(bytes: &[u8], program: Option<u16>, chunk: usize, grant: u64) -> Vec<String> {
    let pool = Budget::new(8 << 20).unwrap();
    let mut reader = Reader::new_with_packet_tracks(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        ReaderLimits {
            program,
            ..ReaderLimits::default()
        },
        32768,
        &[],
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut facts = Vec::new();
    let mut ended = false;
    for _ in 0..2_000_000 {
        match reader.step(&mut WorkBudget::new(grant), &cancel).unwrap() {
            Step::NeedInput(request) => {
                let at = usize::try_from(request.offset).unwrap();
                assert!(at <= bytes.len());
                let end = bytes.len().min(at + request.maximum.min(chunk));
                if at == end {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    reader
                        .provide(
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                Bytes::copy_from_slice(&pool, &bytes[at..end]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::Yield => {}
            Step::End(_) => {
                ended = true;
                break;
            }
            Step::Ready(Event::Other(ReaderEvent::Source(source))) => match source.event() {
                TraceEvent::Pat(table) => {
                    facts.push(format!("pat {:?}", table.programs.as_slice()));
                }
                TraceEvent::ProgramMap {
                    map,
                    active,
                    configuration_epoch,
                } => {
                    assert_eq!(crc(map.source.as_ref()), 0);
                    facts.push(format!(
                        "map {} {} {} {} {} {:?}",
                        map.header.extension,
                        map.header.version,
                        map.pcr_pid,
                        active,
                        configuration_epoch,
                        map.streams
                            .as_slice()
                            .iter()
                            .map(|s| (s.pid, s.stream_type))
                            .collect::<Vec<_>>()
                    ));
                    facts.push(format!(
                        "tracks {:?}",
                        reader
                            .tracks()
                            .iter()
                            .map(|t| (t.id(), t.codec()))
                            .collect::<Vec<_>>()
                    ));
                }
                TraceEvent::Pes(pes) => {
                    let header = pes
                        .header_source_ranges
                        .as_slice()
                        .iter()
                        .flat_map(|range| {
                            assert_eq!(range.entity, SourceId(1));
                            let at = usize::try_from(range.offset).unwrap();
                            bytes[at..at + range.bytes].iter().copied()
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(&header[..3], &[0, 0, 1]);
                    let timestamp = |at: usize| {
                        let p = &header[at..at + 5];
                        assert!(p[0] & 1 == 1 && p[2] & 1 == 1 && p[4] & 1 == 1);
                        (u64::from(p[0] & 0x0e) << 29)
                            | (u64::from(p[1]) << 22)
                            | (u64::from(p[2] & 0xfe) << 14)
                            | (u64::from(p[3]) << 7)
                            | u64::from(p[4] >> 1)
                    };
                    assert_eq!(header[7] >> 6, 3);
                    assert_eq!(pes.header.pts_raw33, Some(timestamp(9)));
                    assert_eq!(pes.header.dts_raw33, Some(timestamp(14)));
                    let mut payload = Vec::new();
                    for (part, range) in pes.payload().zip(pes.source_ranges()) {
                        assert_eq!(range.entity, SourceId(1));
                        assert_eq!(part.len(), range.bytes);
                        let at = usize::try_from(range.offset).unwrap();
                        assert_eq!(part, &bytes[at..at + range.bytes]);
                        payload.extend_from_slice(part);
                    }
                    assert_eq!(payload.len(), pes.payload_len);
                    facts.push(format!(
                        "pes {} {:?} {:?} {} {}",
                        pes.pid,
                        pes.header.pts_raw33,
                        pes.header.dts_raw33,
                        pes.payload_len,
                        sha256::hex(&payload)
                    ));
                }
                TraceEvent::Diagnostic(fault) => facts.push(format!(
                    "fault {:?} {:?} {} {:?}",
                    fault.kind, fault.pid, fault.location.offset, fault.error
                )),
                TraceEvent::Finished => facts.push("finished".into()),
                _ => {}
            },
            Step::Ready(Event::Other(ReaderEvent::Transport(packet))) => {
                let at = usize::try_from(packet.source_offset).unwrap();
                assert_eq!(packet.source.as_ref(), &bytes[at..at + 188]);
                if let Some(adaptation) = packet.adaptation {
                    let mut at = 6;
                    for (mask, clock) in [(0x10, adaptation.pcr), (0x08, adaptation.opcr)] {
                        if bytes[packet.source_offset as usize + 5] & mask != 0 {
                            let raw = &packet.source.as_ref()[at..at + 6];
                            let base = (u64::from(raw[0]) << 25)
                                | (u64::from(raw[1]) << 17)
                                | (u64::from(raw[2]) << 9)
                                | (u64::from(raw[3]) << 1)
                                | u64::from(raw[4] >> 7);
                            let extension = (u16::from(raw[4] & 1) << 8) | u16::from(raw[5]);
                            let clock = clock.expect("wire clock retained");
                            assert_eq!((clock.base, clock.extension), (base, extension));
                            at += 6;
                        } else {
                            assert!(clock.is_none());
                        }
                    }
                }
            }
            Step::Ready(Event::Other(ReaderEvent::Continuity { .. })) => {}
            other => panic!("unexpected source-only step: {other:?}"),
        }
    }
    assert!(ended, "bounded reader did not finish");
    facts
}

#[test]
#[ignore = "requires hash-pinned original mpegts/pmtchange.ts in the FATE cache"]
fn original_pmt_replacement_retains_tracks_clocks_and_faults() {
    let bytes = original(
        "pmtchange.ts",
        "5185db2052de1174629acdf13382eb42449ebfa7ef2b09a822d9867ed8c80860",
    );
    // These are CRC-valid foreign short sections, not corrupt PMTs. Keep the
    // strict identity diagnostics while allowing later genuine PMTs to surface.
    for at in [13912, 189128, 191760, 194392, 290836] {
        assert_eq!(crc(&bytes[at + 5..at + 29]), 0);
        assert_eq!(&bytes[at..at + 4], &[0x47, 0x40, 0x49, bytes[at + 3]]);
        assert_eq!(
            &bytes[at + 4..at + 29],
            &[
                0, 0xc0, 0, 0x15, 0, 0, 4, 0, 0xff, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0xa7, 0xfe,
                0x26, 8,
            ]
        );
    }
    let facts = read(&bytes, Some(4), 32768, 4096);
    assert_eq!(
        facts.join("\n"),
        include_str!("data/original-pmt-program.txt").trim_end()
    );
    assert_eq!(facts, read(&bytes, Some(4), 1, 64));
    assert_eq!(facts, read(&bytes, None, 187, 128));
}

#[test]
#[ignore = "requires hash-pinned original mpegts/loewe.ts in the FATE cache"]
fn original_unaligned_program_reports_faults_at_aligned_offsets() {
    let bytes = original(
        "loewe.ts",
        "f93d2b5d78e4f04027c635e8e1dd866dcf35c233a0fa508f3bcc6a656fba286e",
    );
    let facts = read(&bytes, Some(769), 32768, 4096);
    // The original sync grid is one byte later; do not strip that byte.
    assert!(bytes[1..].chunks_exact(188).all(|p| p[0] == 0x47));
    let mut expected =
        vec!["fault Transport Some(1797) 0 Unsupported(\"scrambled TS payload\")".to_owned()];
    for at in (188..99828).step_by(188) {
        expected.push(if at == 23124 {
            assert_eq!(&bytes[at..at + 4], &bytes[..4]);
            format!("fault Transport Some(1797) {at} Unsupported(\"scrambled TS payload\")")
        } else {
            assert_ne!(bytes[at], 0x47);
            format!("fault Transport None {at} Invalid(\"TS sync byte\")")
        });
    }
    expected.push("fault Transport None 99828 Truncated".into());
    expected.push("finished".into());
    assert_eq!(facts, expected);
    assert_eq!(facts, read(&bytes, Some(769), 1, 64));
}

#[test]
fn startup_requires_a_unique_complete_sync_grid() {
    let mut bytes = [0; 8 * 188];
    assert_eq!(ts188_carrier_offset(&bytes[..188]), Err(Error::Truncated));
    assert!(ts188_carrier_offset(&bytes).is_err());
    for carrier in bytes[187..].chunks_exact_mut(188) {
        carrier[..4].copy_from_slice(&[0x47, 0, 0, 0x10]);
    }
    assert_eq!(ts188_carrier_offset(&bytes), Ok(187));
    for carrier in bytes.chunks_exact_mut(188) {
        carrier[..4].copy_from_slice(&[0x47, 0, 0, 0x10]);
    }
    // The first added header overwrote the header spanning the chunk boundary.
    assert_eq!(ts188_carrier_offset(&bytes), Ok(0));
    for carrier in bytes[12..].chunks_exact_mut(188) {
        carrier[..4].copy_from_slice(&[0x47, 0, 0, 0x10]);
    }
    assert_eq!(
        ts188_carrier_offset(&bytes),
        Err(Error::Unsupported(
            "TS188 startup with competing sync grids"
        ))
    );
}

#[test]
#[ignore = "requires hash-pinned original mpegts/loewe.ts in the FATE cache"]
fn original_program_discovery_uses_the_probed_source_offset() {
    discover_original_program(false);
}

#[test]
#[ignore = "requires hash-pinned original mpegts/loewe.ts in the FATE cache"]
fn original_program_discovery_uses_seekable_startup() {
    discover_original_program(true);
}

fn discover_original_program(startup: bool) {
    let bytes = original(
        "loewe.ts",
        "f93d2b5d78e4f04027c635e8e1dd866dcf35c233a0fa508f3bcc6a656fba286e",
    );
    let offset = ts188_carrier_offset(&bytes).unwrap() as u64;
    assert_eq!(offset, 1);
    let pool = Budget::new(8 << 20).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        },
        32 * 1024,
    )
    .unwrap();
    if startup {
        let mut startup = triergon_mpegts::compat::input::Ts188Startup::new(&input).unwrap();
        let cancel = AtomicBool::new(false);
        loop {
            match startup
                .poll(&mut input, &mut WorkBudget::new(64), &cancel)
                .unwrap()
            {
                Step::NeedInput(request) => {
                    let at = request.offset as usize;
                    let end = bytes.len().min(at + request.maximum.min(187));
                    input
                        .provide(
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                Bytes::copy_from_slice(&pool, &bytes[at..end]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
                Step::Yield => {}
                Step::Ready(actual) => {
                    assert_eq!(actual, offset);
                    break;
                }
                other => panic!("unexpected startup: {other:?}"),
            }
        }
    } else {
        input.request_range(offset).unwrap();
    }
    let limits = ParseLimits::default();
    let mut stream = TransportStream::new(pool.clone(), PacketFormat::Ts188, limits, 769).unwrap();
    let cancel = AtomicBool::new(false);
    let mut pat = None;
    let mut pmt = None;
    let mut next_carrier = offset;
    let mut truncated = None;
    let mut ended = false;
    for _ in 0..bytes.len() {
        match stream
            .poll(&mut input, &mut WorkBudget::new(4096), &cancel)
            .unwrap()
        {
            Step::NeedInput(request) => {
                let at = usize::try_from(request.offset).unwrap();
                let end = (at + request.maximum).min(bytes.len());
                if at == end {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    let chunk = Bytes::copy_from_slice(&pool, &bytes[at..end]).unwrap();
                    input
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, chunk)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::Ready(TraceEvent::Transport(packet)) => {
                assert_eq!(packet.source_offset, next_carrier);
                let at = usize::try_from(next_carrier).unwrap();
                assert_eq!(packet.source.as_ref(), &bytes[at..at + 188]);
                next_carrier += 188;
            }
            Step::Ready(TraceEvent::Diagnostic(fault)) if fault.kind == FaultKind::Transport => {
                if let Some(rejected) = &fault.rejected {
                    assert_eq!(rejected.source_offset(), next_carrier);
                    let at = usize::try_from(next_carrier).unwrap();
                    assert_eq!(rejected.bytes().as_ref(), &bytes[at..at + 188]);
                    next_carrier += 188;
                } else {
                    assert_eq!(fault.error, Error::Truncated);
                    assert!(truncated.replace(fault.location.offset).is_none());
                }
            }
            Step::Ready(TraceEvent::Pat(table)) => {
                pat = Some(table.programs.as_slice().to_vec());
            }
            Step::Ready(TraceEvent::ProgramMap {
                map, active: true, ..
            }) => {
                pmt = Some((
                    map.header.extension,
                    map.pcr_pid,
                    map.streams
                        .as_slice()
                        .iter()
                        .map(|stream| (stream.pid, stream.codec()))
                        .collect::<Vec<_>>(),
                ));
            }
            Step::Ready(_) | Step::Yield => {}
            Step::End(_) => {
                ended = true;
                break;
            }
            blocked @ Step::Backpressure(_) => panic!("unexpected transport step: {blocked:?}"),
        }
    }
    assert!(ended);
    assert_eq!(next_carrier, 99_829);
    assert_eq!(truncated, Some(next_carrier));
    let pat = pat.unwrap();
    assert!(
        pat.iter()
            .any(|entry| (entry.program_number, entry.pid) == (769, 1363))
    );
    assert_eq!(
        pmt,
        Some((
            769,
            1361,
            vec![(1361, Some(CodecId::Hevc)), (1362, Some(CodecId::Aac))]
        ))
    );
}
