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

/// Traverse the original entity through startup and retain the strict source PES
/// boundary separately from the demuxer's packet-completion policy.
#[test]
#[ignore = "requires hash-pinned original mpegts/loewe.ts"]
fn original_startup_complete_source_pes() {
    let path = std::env::var_os("TRIERGON_LOEWE_SOURCE").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.research/fate-samples/mpegts/loewe.ts")
        },
        PathBuf::from,
    );
    assert_eq!(
        sha256::file_hex(&path).unwrap(),
        "f93d2b5d78e4f04027c635e8e1dd866dcf35c233a0fa508f3bcc6a656fba286e"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 100_000);
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(
        sha256::hex(&bytes),
        "f93d2b5d78e4f04027c635e8e1dd866dcf35c233a0fa508f3bcc6a656fba286e"
    );
    let a = complete_source_pes(&bytes, 32768, 4096);
    assert_eq!(a, complete_source_pes(&bytes, 187, 64));
    assert_eq!(
        a.join("\n"),
        include_str!("data/original-loewe-startup.txt").trim_end()
    );
    assert_eq!(a, complete_source_pes(&bytes, 1, 64));
    // The first reference packet begins before either program table is known.
    // These are physical source coordinates, including the leading byte.
    assert_eq!(&bytes[753..757], &[0x47, 0x45, 0x51, 0x10]);
    assert_eq!(&bytes[757..766], &[0, 0, 1, 0xe0, 0, 0, 0x83, 0xc0, 10]);
    let timestamp = |at: usize| {
        let p = &bytes[at..at + 5];
        assert!(p[0] & 1 == 1 && p[2] & 1 == 1 && p[4] & 1 == 1);
        (u64::from(p[0] & 0x0e) << 29)
            | (u64::from(p[1]) << 22)
            | (u64::from(p[2] & 0xfe) << 14)
            | (u64::from(p[3]) << 7)
            | u64::from(p[4] >> 1)
    };
    assert_eq!(timestamp(766), 2_863_340_144);
    assert_eq!(timestamp(771), 2_863_336_544);
}

fn complete_source_pes(bytes: &[u8], chunk: usize, grant: u64) -> Vec<String> {
    complete_source_operation(bytes, chunk, grant, false)
}

fn complete_source_operation(bytes: &[u8], chunk: usize, grant: u64, replay: bool) -> Vec<String> {
    complete_source_with_cuts(bytes, chunk, grant, replay, false)
}

fn complete_source_with_cuts(
    bytes: &[u8],
    chunk: usize,
    grant: u64,
    replay: bool,
    cuts: bool,
) -> Vec<String> {
    let pool = Budget::new(8 << 20).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        },
        32768,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut startup = triergon_mpegts::compat::input::Ts188Startup::new(&input).unwrap();
    let mut stream = TransportStream::new(
        pool.clone(),
        PacketFormat::Ts188,
        ParseLimits::default(),
        769,
    )
    .unwrap();
    let mut started = false;
    let mut rows = Vec::new();
    let mut carriers = 0;
    let mut payload_bytes = 0;
    let mut assembler = None;
    let mut audio_assembler = None;
    let mut selected_audio = None;
    let mut selected_video = None;
    let mut replaying = false;
    let mut cutters = Vec::new();
    let mut starts = [0_u64; 2];
    for _ in 0..2_000_000 {
        let step = if started {
            stream
                .poll(&mut input, &mut WorkBudget::new(grant), &cancel)
                .unwrap()
        } else {
            match startup
                .poll(&mut input, &mut WorkBudget::new(grant), &cancel)
                .unwrap()
            {
                Step::Ready(offset) => {
                    assert_eq!(offset, 1);
                    started = true;
                    continue;
                }
                Step::NeedInput(r) => Step::NeedInput(r),
                Step::Yield => Step::Yield,
                other => panic!("unexpected startup: {other:?}"),
            }
        };
        match step {
            Step::NeedInput(r) => {
                let at = usize::try_from(r.offset).unwrap();
                let end = bytes.len().min(at + r.maximum.min(chunk));
                if at == end {
                    input.finish(r.source, r.token).unwrap();
                } else {
                    input
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                Bytes::copy_from_slice(&pool, &bytes[at..end]).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::Yield => {}
            Step::Ready(TraceEvent::Transport(packet)) => {
                assert_eq!(packet.source_offset, 1 + carriers * 188);
                let at = packet.source_offset as usize;
                assert_eq!(packet.source.as_ref(), &bytes[at..at + 188]);
                carriers += 1;
                for (index, cutter) in cutters.iter_mut().enumerate() {
                    let cutter: &mut triergon_mpegts::compat::pes::PesCutter = cutter;
                    cutter.push(&packet).unwrap();
                    drain_cuts(cutter, starts[index], &mut rows);
                    if packet.pid == cutter.pid() && packet.payload_unit_start {
                        starts[index] = packet.source_offset;
                    }
                }
                if replaying
                    && (Some(packet.pid) == selected_video || Some(packet.pid) == selected_audio)
                    && let Some(payload) = packet.payload
                {
                    let assembler = if Some(packet.pid) == selected_video {
                        assembler.as_mut().unwrap()
                    } else {
                        audio_assembler.as_mut().unwrap()
                    };
                    triergon_mpegts::PesAssembler::push(
                        assembler,
                        payload,
                        packet.payload_unit_start,
                        SourceId(1),
                        packet.payload_source_offset,
                    )
                    .unwrap();
                    drain_replay(assembler, &mut rows, &cancel, bytes);
                }
            }
            Step::Ready(TraceEvent::Pes(pes)) => {
                let mut digest = 0_u64;
                let mut size = 0;
                for (part, range) in pes.payload().zip(pes.source_ranges()) {
                    assert_eq!(range.entity, SourceId(1));
                    let at = range.offset as usize;
                    assert_eq!(part, &bytes[at..at + range.bytes]);
                    for byte in part {
                        digest = digest.wrapping_mul(31).wrapping_add(u64::from(*byte));
                    }
                    size += part.len();
                }
                assert_eq!(size, pes.payload_len);
                payload_bytes += size;
                rows.push(format!(
                    "pes {} {:?} {:?} {} {} {}",
                    pes.pid,
                    pes.header.pts_raw33,
                    pes.header.dts_raw33,
                    pes.header_source_ranges.as_slice()[0].offset,
                    size,
                    digest
                ));
            }
            Step::Ready(TraceEvent::Diagnostic(fault)) => {
                if let Some(rejected) = &fault.rejected {
                    assert_eq!(rejected.source_offset(), 1 + carriers * 188);
                    let at = rejected.source_offset() as usize;
                    assert_eq!(rejected.bytes().as_ref(), &bytes[at..at + 188]);
                    if replaying {
                        // Every refused full carrier is adaptation-only: no
                        // elementary bytes require nonconforming admission.
                        assert_eq!(bytes[at + 3] & 0x30, 0x20);
                        assert_eq!(bytes[at + 4], 183);
                    }
                    carriers += 1;
                }
                rows.push(format!(
                    "fault {:?} {} {:?}",
                    fault.kind, fault.location.offset, fault.error
                ));
            }
            Step::Ready(TraceEvent::Finished) => {
                if replay && !replaying {
                    let pid = selected_video.expect("source PMT identifies video");
                    assembler = Some(
                        triergon_mpegts::PesAssembler::new(
                            pid,
                            true,
                            pool.clone(),
                            ParseLimits::default(),
                            triergon_mpegts::SourceEpochs::default(),
                        )
                        .unwrap(),
                    );
                    audio_assembler = Some(
                        triergon_mpegts::PesAssembler::new(
                            selected_audio.expect("source PMT identifies audio"),
                            false,
                            pool.clone(),
                            ParseLimits::default(),
                            triergon_mpegts::SourceEpochs::default(),
                        )
                        .unwrap(),
                    );
                    if cuts {
                        for pid in [pid, selected_audio.unwrap()] {
                            cutters.push(
                                triergon_mpegts::compat::pes::PesCutter::new(
                                    pid,
                                    pool.clone(),
                                    ParseLimits::default(),
                                )
                                .unwrap(),
                            );
                        }
                    }
                    input.request_range(1).unwrap();
                    stream = TransportStream::new(
                        pool.clone(),
                        PacketFormat::Ts188,
                        ParseLimits::default(),
                        769,
                    )
                    .unwrap();
                    rows.push(format!("replay {pid}"));
                    carriers = 0;
                    replaying = true;
                    continue;
                }
                if let Some(assembler) = &mut assembler {
                    assembler.end_of_source();
                    drain_replay(assembler, &mut rows, &cancel, bytes);
                }
                if let Some(assembler) = &mut audio_assembler {
                    assembler.end_of_source();
                    drain_replay(assembler, &mut rows, &cancel, bytes);
                }
                for (index, cutter) in cutters.iter_mut().enumerate() {
                    cutter.finish().unwrap();
                    drain_cuts(cutter, starts[index], &mut rows);
                }
                rows.push("finished".into());
                rows.push(format!(
                    "eof carriers {carriers} payload_bytes {payload_bytes}"
                ));
                return rows;
            }
            Step::Ready(TraceEvent::Pat(table)) => {
                rows.push(format!(
                    "pat {}",
                    table.source_ranges.as_slice()[0].range.offset
                ));
                assert!(
                    table
                        .programs
                        .as_slice()
                        .iter()
                        .any(|p| (p.program_number, p.pid) == (769, 1363))
                );
            }
            Step::Ready(TraceEvent::ProgramMap {
                map, active: true, ..
            }) => {
                assert_eq!(map.header.extension, 769);
                assert_eq!(map.pcr_pid, 1361);
                assert_eq!(
                    map.streams
                        .as_slice()
                        .iter()
                        .map(|s| (s.pid, s.stream_type))
                        .collect::<Vec<_>>(),
                    [(1361, 0x24), (1362, 0x0f)]
                );
                rows.push(format!("pmt {}", map.source_ranges.as_slice()[0].offset));
                selected_video = map
                    .streams
                    .as_slice()
                    .iter()
                    .find(|s| s.stream_type == 0x24)
                    .map(|s| s.pid);
                selected_audio = map
                    .streams
                    .as_slice()
                    .iter()
                    .find(|s| s.stream_type == 0x0f)
                    .map(|s| s.pid);
            }
            Step::Ready(_) => {}
            Step::End(_) => {
                stream.end_of_source().unwrap();
            }
            other @ Step::Backpressure(_) => panic!("unexpected traversal: {other:?}"),
        }
    }
    panic!("source traversal exceeded work bound");
}

fn drain_replay(
    assembler: &mut triergon_mpegts::PesAssembler,
    rows: &mut Vec<String>,
    cancel: &AtomicBool,
    bytes: &[u8],
) {
    for _ in 0..100_000 {
        let step = match assembler.poll(&mut WorkBudget::new(64), cancel) {
            Ok(step) => step,
            Err(error) => {
                assert_eq!(error, Error::Truncated);
                rows.push(format!("replay fault {error:?}"));
                return;
            }
        };
        match step {
            triergon_mpegts::AssemblyStep::Output(pes) => {
                for (part, range) in pes.payload().zip(pes.source_ranges()) {
                    assert_eq!(range.entity, SourceId(1));
                    let at = usize::try_from(range.offset).unwrap();
                    assert_eq!(part, &bytes[at..at + range.bytes]);
                }
                let payload: Vec<_> = pes.payload().flatten().copied().collect();
                rows.push(format!(
                    "replayed {} {:?} {:?} {} {}",
                    pes.pid,
                    pes.header.pts_raw33,
                    pes.header.dts_raw33,
                    payload.len(),
                    sha256::hex(&payload)
                ));
            }
            triergon_mpegts::AssemblyStep::Yield => {}
            triergon_mpegts::AssemblyStep::NeedPayload
            | triergon_mpegts::AssemblyStep::Finished => return,
        }
    }
    panic!("replay assembly exceeded work bound");
}

#[test]
#[ignore = "requires hash-pinned original mpegts/loewe.ts"]
fn original_discovery_and_strict_carrier_replay() {
    let bytes = original(
        "loewe.ts",
        "f93d2b5d78e4f04027c635e8e1dd866dcf35c233a0fa508f3bcc6a656fba286e",
    );
    assert_eq!(
        sha256::hex(&bytes),
        "f93d2b5d78e4f04027c635e8e1dd866dcf35c233a0fa508f3bcc6a656fba286e"
    );
    let rows = complete_source_operation(&bytes, 32768, 4096, true);
    let packets: Vec<_> = rows
        .iter()
        .filter(|row| row.starts_with("replayed "))
        .collect();
    assert_eq!(packets.len(), 22);
    let payload_bytes: usize = packets
        .iter()
        .map(|row| {
            row.split_whitespace()
                .nth(4)
                .unwrap()
                .parse::<usize>()
                .unwrap()
        })
        .sum();
    assert_eq!(payload_bytes, 82_876);
    assert_eq!(
        rows.join("\n"),
        include_str!("data/original-loewe-replay.txt").trim_end()
    );
    assert_eq!(rows, complete_source_operation(&bytes, 187, 64, true));
    assert_eq!(rows, complete_source_operation(&bytes, 1, 64, true));
}

fn drain_cuts(
    cutter: &mut triergon_mpegts::compat::pes::PesCutter,
    position: u64,
    rows: &mut Vec<String>,
) {
    while let Some(packet) = cutter.pop() {
        let digest = packet
            .data
            .as_ref()
            .iter()
            .fold(0_u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(*b)));
        rows.push(format!(
            "cut {} {:?} {:?} {} {:?} {} {}",
            cutter.pid(),
            packet.pts_raw33,
            packet.dts_raw33,
            position,
            packet.boundary,
            packet.data.len(),
            digest
        ));
    }
}

#[test]
#[ignore = "requires hash-pinned original mpegts/loewe.ts"]
fn original_terminal_pes_compatibility() {
    let bytes = original(
        "loewe.ts",
        "f93d2b5d78e4f04027c635e8e1dd866dcf35c233a0fa508f3bcc6a656fba286e",
    );
    assert_eq!(
        sha256::hex(&bytes),
        "f93d2b5d78e4f04027c635e8e1dd866dcf35c233a0fa508f3bcc6a656fba286e"
    );
    let rows = complete_source_with_cuts(&bytes, 32768, 4096, true, true);
    let cuts: Vec<_> = rows
        .iter()
        .filter(|r| r.starts_with("cut "))
        .cloned()
        .collect();
    assert_eq!(cuts.len(), 23);
    assert_eq!(
        cuts.iter()
            .map(|r| r
                .split_whitespace()
                .nth(6)
                .unwrap()
                .parse::<usize>()
                .unwrap())
            .sum::<usize>(),
        85_990
    );
    assert!(rows.iter().any(|r| r == "replay fault Truncated"));
    assert_eq!(
        rows.iter()
            .filter(|row| !row.starts_with("cut "))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
        include_str!("data/original-loewe-replay.txt").trim_end()
    );
    // Compare all 23 records in delivery order. No timestamp fill-in, sorting,
    // payload selection or exclusion is applied to the candidate observation.
    let references: Vec<Vec<u64>> = include_str!("data/original-loewe-reference.txt")
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            line.split_whitespace()
                .map(|n| n.parse().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(references.len(), cuts.len());
    let mut missing_dts = Vec::new();
    let mut prior_reference = 0_u64;
    for (index, (cut, reference)) in cuts.iter().zip(&references).enumerate() {
        let fields: Vec<_> = cut.split_whitespace().collect();
        let value = |at: usize| fields[at].parse::<u64>().unwrap();
        let time = |at: usize| {
            fields[at]
                .strip_prefix("Some(")
                .and_then(|s| s.strip_suffix(')'))
                .map(|s| s.parse::<u64>().unwrap())
        };
        assert_eq!(value(1), reference[1]);
        assert_eq!(time(2), Some(reference[2]));
        assert_eq!(value(4), reference[5]);
        assert_eq!(value(6), reference[7]);
        if time(3) != Some(reference[3]) {
            assert_eq!(time(3), None);
            missing_dts.push(index + 1);
        }
        let stream_id = if reference[1] == 1361 { 0xe0 } else { 0xc0 };
        let fold = |h: u64, n: u64| h.wrapping_mul(31).wrapping_add(n);
        // The reference digest includes metadata before payload and MPEG-TS
        // stream-id side data after it. Reconstruct every cumulative digest
        // using the actual candidate payload polynomial; this independently
        // checks every payload without silently filling its missing DTS.
        let mut payload_check = prior_reference;
        for n in &reference[..8] {
            payload_check = fold(payload_check, *n);
        }
        let power = 31_u64.wrapping_pow(u32::try_from(value(6)).unwrap());
        payload_check = payload_check.wrapping_mul(power).wrapping_add(value(7));
        for n in [1, 19, 1, stream_id] {
            payload_check = fold(payload_check, n);
        }
        assert_eq!(
            payload_check,
            reference[8],
            "reference payload at record {}",
            index + 1
        );
        prior_reference = reference[8];
    }
    assert_eq!(missing_dts, [2, 6, 10, 12, 15, 17, 20, 23]);
    assert_eq!(prior_reference, 8_005_868_861_097_246_031);
    assert_eq!(references.last().unwrap()[6], 2); // Reference corrupt flag.
    // CutPes has no flags/duration/side-data projection: the reconstructed
    // reference fold above proves payload correspondence, not full parity.
    assert_eq!(
        cuts.last().unwrap(),
        "cut 1362 Some(2863301624) None 75389 LogicalEof 3114 10622192325936484760"
    );
    for (chunk, grant) in [(187, 64), (1, 64)] {
        assert_eq!(
            rows,
            complete_source_with_cuts(&bytes, chunk, grant, true, true)
        );
    }
}

#[test]
#[ignore = "requires hash-pinned original mpegts/loewe.ts"]
fn original_registered_observer_metadata() {
    use triergon_mpegts::compat::pes::{
        DemuxClock, DemuxObserver, DemuxProfile, DemuxScope, RegisteredCarriage, SourceContext,
        TransportFaultRecovery, TransportObservation,
    };
    use triergon_mpegts::{AssemblyStep, SourceEpochs, parse_transport};
    let bytes = original(
        "loewe.ts",
        "f93d2b5d78e4f04027c635e8e1dd866dcf35c233a0fa508f3bcc6a656fba286e",
    );
    let reference: Vec<Vec<u64>> = include_str!("data/original-loewe-reference.txt")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split_whitespace().map(|v| v.parse().unwrap()).collect())
        .collect();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output =
        std::process::Command::new(root.join(".research/ts188-comparison-reference/ffprobe"))
            .args([
                "-v",
                "error",
                "-fflags",
                "+noparse+nofillin",
                "-select_streams",
                "p:769",
                "-show_packets",
                "-show_data",
                "-show_entries",
                "packet=data",
                "-of",
                "compact",
            ])
            .arg(root.join(".research/fate-samples/mpegts/loewe.ts"))
            .output()
            .unwrap();
    assert!(output.status.success());
    let dump = String::from_utf8(output.stdout).unwrap();
    let reference_payloads: Vec<Vec<u8>> = dump
        .lines()
        .filter(|line| line.starts_with("packet|data="))
        .map(|line| {
            line.split("\\n")
                .skip(1)
                .filter_map(|row| row.split_once(": ").map(|(_, data)| data))
                .flat_map(|data| {
                    let hex = data.split("  ").next().unwrap().replace(' ', "");
                    (0..hex.len())
                        .step_by(2)
                        .map(move |i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                })
                .collect()
        })
        .collect();
    assert_eq!(reference_payloads.len(), 23);
    for (chunk, grant) in [(1, 1), (187, 64), (32768, 4096)] {
        let pool = Budget::new(8 << 20).unwrap();
        let epochs = SourceEpochs::default();
        let mut observers: Vec<_> = [
            (1361, 0x24, 0xe0, RegisteredCarriage::Hevc),
            (1362, 0x0f, 0xc0, RegisteredCarriage::AacAdts),
        ]
        .into_iter()
        .map(|(pid, stream_type, stream_id, carriage)| {
            let scope = DemuxScope {
                program: 769,
                pid,
                stream_type,
                stream_id,
                entity: SourceId(1),
                epochs,
            };
            TransportFaultRecovery::new(
                DemuxObserver::new_registered(
                    DemuxProfile::PesPackets,
                    scope,
                    carriage,
                    pool.clone(),
                    ParseLimits::default(),
                )
                .unwrap(),
            )
            .unwrap()
        })
        .collect();
        let mut clock = DemuxClock::new(769).unwrap();
        let cancel = AtomicBool::new(false);
        let mut rows = Vec::new();
        let mut digest = 0_u64;
        let mut drain = |observer: &mut TransportFaultRecovery| {
            loop {
                match observer.poll(&mut WorkBudget::new(grant), &cancel).unwrap() {
                    AssemblyStep::Output(TransportObservation::Accepted(packet)) => {
                        let time = clock
                            .project_header(
                                packet.pid,
                                packet.source_epochs,
                                packet.time_header(),
                                &mut WorkBudget::new(1),
                                &cancel,
                            )
                            .unwrap()
                            .unwrap();
                        for span in packet.spans.as_slice() {
                            let at = span.source.offset as usize;
                            assert_eq!(span.source.entity, SourceId(1));
                            assert_eq!(span.as_ref(), &bytes[at..at + span.source.bytes]);
                        }
                        let payload: Vec<_> = packet.payload().flatten().copied().collect();
                        assert_eq!(
                            payload,
                            reference_payloads[rows.len()],
                            "payload record {}",
                            rows.len() + 1
                        );
                        let fields = [
                            u64::from(packet.pid == 1362),
                            u64::from(packet.pid),
                            time.pts90.unwrap() as u64,
                            time.dts90.unwrap() as u64,
                            packet.duration90_raw as u64,
                            packet.position as u64,
                            u64::from(packet.flags),
                            packet.payload_len as u64,
                        ];
                        for n in fields {
                            digest = digest.wrapping_mul(31).wrapping_add(n);
                        }
                        for part in packet.payload() {
                            for byte in part {
                                digest = digest.wrapping_mul(31).wrapping_add(u64::from(*byte));
                            }
                        }
                        for n in [1, 19, 1, u64::from(packet.stream_id)] {
                            digest = digest.wrapping_mul(31).wrapping_add(n);
                        }
                        rows.push([fields.as_slice(), &[digest]].concat());
                    }
                    AssemblyStep::Output(other) => panic!("unexpected observation: {other:?}"),
                    AssemblyStep::Yield => {}
                    AssemblyStep::NeedPayload | AssemblyStep::Finished => break,
                }
            }
        };
        let mut started = [false; 2];
        let mut assembled = Vec::new();
        let mut index = 0;
        for input in bytes[1..].chunks(chunk) {
            assembled.extend_from_slice(input);
            while assembled.len() >= 188 {
                let carrier = &assembled[..188];
                let parsed = parse_transport(
                    Bytes::copy_from_slice(&pool, carrier).unwrap(),
                    PacketFormat::Ts188,
                    1 + index as u64 * 188,
                );
                assembled.drain(..188);
                let ordinal = index;
                index += 1;
                let Ok(mut packet) = parsed else {
                    let at = 1 + ordinal * 188;
                    assert_eq!(bytes[at + 3] & 0x30, 0x20);
                    assert_eq!(bytes[at + 4], 183);
                    continue;
                };
                packet.source_id = Some(SourceId(1));
                if let Some((slot, observer)) = observers
                    .iter_mut()
                    .enumerate()
                    .find(|(_, o)| o.scope().pid == packet.pid)
                {
                    if !started[slot] && !packet.payload_unit_start {
                        continue;
                    }
                    started[slot] = true;
                    observer
                        .push(
                            packet,
                            SourceContext {
                                source_event_ordinal: ordinal as u64,
                                epochs,
                                byte_run_epoch: None,
                            },
                        )
                        .unwrap();
                    drain(observer);
                }
            }
        }
        for observer in &mut observers {
            observer.end_of_source().unwrap();
            drain(observer);
        }
        assert_eq!(rows, reference, "input chunk {chunk}, work grant {grant}");
    }
}
