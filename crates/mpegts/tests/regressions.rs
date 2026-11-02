//! Authored before the implementation. No Cargo execution is authorised in this lane.
//! All byte vectors are original; no reference implementation or media is imported.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, Time, WorkBudget,
};
use triergon_mpegts::{
    AssemblyStep, Continuity, ContinuityOutcome, FrameState, PacketFormat, ParseLimits,
    PesAssembler, SectionAssembler, SourceEpochs, TransportFramer, parse_transport,
    psi::{PatReader, PmtReader, PsiCrc32, TableStep},
};

const PID: u16 = 0x100;
const PAT: &[u8] = &[
    0x00, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00, 0x00, 0x01, 0xf0, 0x00, 0x2a, 0xb1, 0x04, 0xb2,
];

fn pool() -> Budget {
    Budget::new(4 * 1024 * 1024).unwrap()
}
fn bytes(pool: &Budget, data: &[u8]) -> Bytes {
    Bytes::copy_from_slice(pool, data).unwrap()
}
fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}

// Independent test construction uses the mathematical polynomial division, not
// the production CRC state update.
fn crc(data: &[u8]) -> u32 {
    let mut remainder = 0xffff_ffff_u64;
    for byte in data {
        let mut dividend = (remainder << 8) ^ (u64::from(*byte) << 32);
        for bit in (32..40).rev() {
            if dividend & (1_u64 << bit) != 0 {
                dividend ^= 0x1_04c1_1db7_u64 << (bit - 32);
            }
        }
        remainder = dividend;
    }
    remainder as u32
}

fn pmt(descriptors: &[u8]) -> Vec<u8> {
    let length = 18 + descriptors.len();
    let mut section = vec![
        2,
        0xb0 | (length >> 8) as u8,
        length as u8,
        0,
        1,
        0xc1,
        0,
        0,
        0xe1,
        0,
        0xf0,
        0,
        0x1b,
        0xe1,
        0,
        0xf0 | (descriptors.len() >> 8) as u8,
        descriptors.len() as u8,
    ];
    section.extend_from_slice(descriptors);
    section.extend_from_slice(&crc(&section).to_be_bytes());
    section
}

fn stamp(raw: u64, prefix: u8) -> [u8; 5] {
    [
        prefix << 4 | ((raw >> 29) as u8 & 14) | 1,
        (raw >> 22) as u8,
        ((raw >> 14) as u8 & 254) | 1,
        (raw >> 7) as u8,
        ((raw << 1) as u8) | 1,
    ]
}

fn pes(data: &[u8], pts: Option<u64>, dts: Option<u64>, unbounded: bool) -> Vec<u8> {
    let optional = if dts.is_some() {
        10
    } else if pts.is_some() {
        5
    } else {
        0
    };
    let length = if unbounded {
        0
    } else {
        3 + optional + data.len()
    };
    let mut result = vec![
        0,
        0,
        1,
        0xe0,
        (length >> 8) as u8,
        length as u8,
        0x80,
        if dts.is_some() {
            0xc0
        } else if pts.is_some() {
            0x80
        } else {
            0
        },
        optional as u8,
    ];
    if let Some(raw) = pts {
        result.extend_from_slice(&stamp(raw, if dts.is_some() { 3 } else { 2 }));
    }
    if let Some(raw) = dts {
        result.extend_from_slice(&stamp(raw, 1));
    }
    result.extend_from_slice(data);
    result
}

fn packet(data: &[u8], cc: u8, pusi: bool, pcr: Option<(u64, u16)>) -> [u8; 188] {
    assert!(data.len() <= if pcr.is_some() { 176 } else { 184 });
    let mut result = [0xff; 188];
    result[..4].copy_from_slice(&[0x47, 1 | if pusi { 0x40 } else { 0 }, 0, cc | 0x30]);
    let length = 183 - data.len();
    result[4] = length as u8;
    if length != 0 {
        result[5] = if pcr.is_some() { 0x10 } else { 0 };
    }
    if let Some((base, extension)) = pcr {
        let value = (base << 15) | (63 << 9) | u64::from(extension);
        result[6..12].copy_from_slice(&value.to_be_bytes()[2..]);
    }
    result[188 - data.len()..].copy_from_slice(data);
    result
}

fn section(pool: &Budget, data: &[u8], work_units: u64) -> triergon_mpegts::psi::Section {
    let mut assembler = SectionAssembler::new(0, pool.clone(), ParseLimits::default()).unwrap();
    let mut payload = vec![0];
    payload.extend_from_slice(data);
    assembler
        .push(bytes(pool, &payload), true, SourceId(7), 1_u64 << 40)
        .unwrap();
    loop {
        match assembler
            .poll(&mut WorkBudget::new(work_units), &cancel())
            .unwrap()
        {
            AssemblyStep::Output(result) => return result,
            AssemblyStep::Yield => {}
            other => panic!("section not completed: {other:?}"),
        }
    }
}

#[test]
fn crc_and_pat_preserve_source_and_network_program_identity() {
    assert_eq!(PsiCrc32::of(b"123456789"), 0x0376_e6e7);
    assert_eq!(PsiCrc32::of(PAT), 0);
    let pool = pool();
    let original = section(&pool, PAT, 1);
    assert_eq!(original.bytes().as_ref(), PAT);
    assert_eq!(original.source_ranges()[0].offset, (1_u64 << 40) + 1);
    let mut reader = PatReader::new(&original, &pool, ParseLimits::default()).unwrap();
    let result = loop {
        match reader.poll(&mut WorkBudget::new(1), &cancel()).unwrap() {
            TableStep::Output(table) => break table,
            TableStep::Yield => {}
            TableStep::Done => panic!("PAT disappeared"),
        }
    };
    assert_eq!(result.programs.as_slice()[0].program_number, 1);
    assert_eq!(result.programs.as_slice()[0].pid, 0x1000);
    drop(reader);
    drop(original);
    assert_eq!(result.source.as_ref(), PAT);
}

#[test]
fn section_pointer_can_complete_a_partial_section_then_start_another() {
    let pool = pool();
    let mut assembler = SectionAssembler::new(0, pool.clone(), ParseLimits::default()).unwrap();
    let mut first = vec![0];
    first.extend_from_slice(&PAT[..7]);
    assembler
        .push(bytes(&pool, &first), true, SourceId(1), 0)
        .unwrap();
    assert!(matches!(
        assembler
            .poll(&mut WorkBudget::new(100), &cancel())
            .unwrap(),
        AssemblyStep::NeedPayload
    ));
    let mut second = vec![9];
    second.extend_from_slice(&PAT[7..]);
    second.extend_from_slice(PAT);
    assembler
        .push(bytes(&pool, &second), true, SourceId(1), 188)
        .unwrap();
    let mut sections = Vec::new();
    loop {
        match assembler.poll(&mut WorkBudget::new(1), &cancel()).unwrap() {
            AssemblyStep::Output(item) => sections.push(item),
            AssemblyStep::Yield => {}
            AssemblyStep::NeedPayload => break,
            AssemblyStep::Finished => panic!("not EOF"),
        }
    }
    assert_eq!(sections.len(), 2);
    assert_eq!(sections[0].bytes().as_ref(), PAT);
    assert_eq!(sections[1].bytes().as_ref(), PAT);
    assert_eq!(sections[0].source_ranges().len(), 2);
}

#[test]
fn bad_crc_remains_observable_and_cannot_become_a_pat() {
    let pool = pool();
    let mut broken = PAT.to_vec();
    broken[15] ^= 1;
    let original = section(&pool, &broken, 3);
    assert_eq!(original.crc_remainder(), Some(0x04c1_1db7));
    assert!(PatReader::new(&original, &pool, ParseLimits::default()).is_err());
    assert_eq!(original.bytes().as_ref(), broken);
}

#[test]
fn pmt_preserves_ordered_unknown_and_multilingual_descriptor_bytes() {
    let pool = pool();
    let descriptors = [
        0xee, 3, 0x47, 0, 0xff, 0x0a, 8, b'e', b'n', b'g', 0, b'f', b'r', b'a', 2,
    ];
    let original = section(&pool, &pmt(&descriptors), 5);
    let mut reader = PmtReader::new(&original, &pool, ParseLimits::default()).unwrap();
    let result = loop {
        match reader.poll(&mut WorkBudget::new(1), &cancel()).unwrap() {
            TableStep::Output(value) => break value,
            TableStep::Yield => {}
            TableStep::Done => panic!("PMT disappeared"),
        }
    };
    assert_eq!(result.pcr_pid, PID);
    assert_eq!(
        result.streams.as_slice()[0].descriptor_bytes.as_ref(),
        descriptors
    );
    let mut descriptors = result.streams.as_slice()[0].descriptors();
    assert_eq!(descriptors.next().unwrap().unwrap().tag, 0xee);
    assert_eq!(descriptors.next().unwrap().unwrap().data.len(), 8);
    assert!(descriptors.next().is_none());
}

#[test]
fn descriptors_cannot_overrun_crc_or_accept_multi_section_pmt() {
    let pool = pool();
    let malformed = section(&pool, &pmt(&[0x0a, 8, b'e', b'n', b'g', 0]), 8);
    let mut reader = PmtReader::new(&malformed, &pool, ParseLimits::default()).unwrap();
    assert!(reader.poll(&mut WorkBudget::new(100), &cancel()).is_err());
    let mut invalid = pmt(&[]);
    invalid[6] = 1;
    invalid[7] = 1;
    let end = invalid.len() - 4;
    let checksum = crc(&invalid[..end]);
    invalid[end..].copy_from_slice(&checksum.to_be_bytes());
    let original = section(&pool, &invalid, 100);
    assert!(PmtReader::new(&original, &pool, ParseLimits::default()).is_err());
}

#[test]
fn transport_192_preserves_prefix_pcr_and_large_physical_offsets() {
    let pool = pool();
    let ts = packet(b"opaque", 15, true, Some(((1 << 33) - 1, 299)));
    let mut source = vec![0x80, 0x01, 0x02, 0x03];
    source.extend_from_slice(&ts);
    let parsed =
        parse_transport(bytes(&pool, &source), PacketFormat::M2ts192, 1_u64 << 40).unwrap();
    assert_eq!(parsed.prefix.as_ref().unwrap().as_ref(), &[0x80, 1, 2, 3]);
    assert_eq!(parsed.payload.as_ref().unwrap().as_ref(), b"opaque");
    assert_eq!(parsed.payload_source_offset, (1_u64 << 40) + 192 - 6);
    let pcr = parsed.adaptation.as_ref().unwrap().pcr.unwrap();
    assert_eq!(pcr.base, (1 << 33) - 1);
    assert_eq!(pcr.extension, 299);
    assert_eq!(pcr.ticks_27mhz(), ((1 << 33) - 1) * 300 + 299);
}

#[test]
fn continuity_wrap_adaptation_duplicate_and_changed_pcr_are_distinct() {
    let pool = pool();
    let mut state = Continuity::new(PID).unwrap();
    let first = parse_transport(
        bytes(&pool, &packet(b"abc", 15, true, Some((1, 0)))),
        PacketFormat::Ts188,
        0,
    )
    .unwrap();
    assert_eq!(state.observe(&first).unwrap(), ContinuityOutcome::First);
    let duplicate = parse_transport(
        bytes(&pool, &packet(b"abc", 15, true, Some((2, 1)))),
        PacketFormat::Ts188,
        188,
    )
    .unwrap();
    assert_eq!(
        state.observe(&duplicate).unwrap(),
        ContinuityOutcome::Duplicate
    );
    assert_eq!(
        state.observe(&duplicate).unwrap(),
        ContinuityOutcome::ExcessDuplicate
    );
    let next = parse_transport(
        bytes(&pool, &packet(b"next", 0, true, None)),
        PacketFormat::Ts188,
        376,
    )
    .unwrap();
    assert_eq!(state.observe(&next).unwrap(), ContinuityOutcome::Continuous);
    let mut only = packet(&[], 0, false, None);
    only[3] = 0x20;
    let only = parse_transport(bytes(&pool, &only), PacketFormat::Ts188, 564).unwrap();
    assert_eq!(state.observe(&only).unwrap(), ContinuityOutcome::Continuous);
    let lost = parse_transport(
        bytes(&pool, &packet(b"x", 3, true, None)),
        PacketFormat::Ts188,
        752,
    )
    .unwrap();
    assert_eq!(
        state.observe(&lost).unwrap(),
        ContinuityOutcome::Lost {
            expected: 1,
            actual: 3
        }
    );
}

#[test]
fn malformed_adaptation_and_pcr_cannot_reach_payload() {
    let pool = pool();
    let mut bad = packet(b"x", 0, true, None);
    bad[4] = 183;
    assert!(parse_transport(bytes(&pool, &bad), PacketFormat::Ts188, 0).is_err());
    let bad = packet(b"x", 0, true, Some((0, 300)));
    assert!(parse_transport(bytes(&pool, &bad), PacketFormat::Ts188, 0).is_err());
    let mut bad = packet(b"x", 0, true, None);
    bad[3] = 0;
    assert!(parse_transport(bytes(&pool, &bad), PacketFormat::Ts188, 0).is_err());
}

#[test]
fn framing_survives_every_input_byte_and_work_yield_without_duplicate_packets() {
    let pool = pool();
    let mut reader = TransportFramer::new(pool.clone(), PacketFormat::Ts188);
    let source = packet(b"hello", 0, true, None);
    let mut outputs = 0;
    for (offset, byte) in source.iter().enumerate() {
        let mut consumed = 0;
        while consumed == 0 {
            let step = reader.push(&[*byte], offset as u64, &mut WorkBudget::new(1), &cancel());
            consumed += step.consumed;
            match step.state {
                FrameState::Packet(packet) => {
                    outputs += 1;
                    assert_eq!(packet.payload.unwrap().as_ref(), b"hello");
                }
                FrameState::NeedInput | FrameState::Yield => {}
                FrameState::Error(error) => panic!("unexpected {error:?}"),
            }
        }
    }
    assert_eq!(outputs, 1);
    reader.finish().unwrap();
}

#[test]
fn pes_header_can_cross_every_payload_boundary_and_keeps_raw33_bits() {
    let pool = pool();
    let mut reader = PesAssembler::new(
        PID,
        true,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    let source = pes(b"original", Some(2), Some((1 << 33) - 6), false);
    let mut result = None;
    for (position, byte) in source.iter().enumerate() {
        reader
            .push(
                bytes(&pool, &[*byte]),
                position == 0,
                SourceId(9),
                1000 + position as u64,
            )
            .unwrap();
        loop {
            match reader.poll(&mut WorkBudget::new(1), &cancel()).unwrap() {
                AssemblyStep::Output(value) => result = Some(value),
                AssemblyStep::Yield => {}
                AssemblyStep::NeedPayload => break,
                AssemblyStep::Finished => panic!("not EOF"),
            }
        }
    }
    let packet = result.unwrap();
    assert_eq!(packet.header.pts_raw33, Some(2));
    assert_eq!(packet.header.dts_raw33, Some((1 << 33) - 6));
    assert_eq!(packet.payload_len, 8);
    assert_eq!(
        packet
            .payload()
            .flat_map(|part| part.iter().copied())
            .collect::<Vec<_>>(),
        b"original"
    );
    assert_eq!(packet.source_ranges().map(|r| r.bytes).sum::<usize>(), 8);
    assert_eq!(
        packet
            .header_source_ranges
            .as_slice()
            .iter()
            .map(|r| r.bytes)
            .sum::<usize>(),
        19
    );
}

#[test]
fn pes_is_not_an_access_unit_and_contiguous_segment_end_does_not_flush_it() {
    let pool = pool();
    let mut reader = PesAssembler::new(
        PID,
        true,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    let source = pes(b"\0\0", Some(0), None, true);
    reader
        .push(bytes(&pool, &source), true, SourceId(1), 0)
        .unwrap();
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap(),
        AssemblyStep::NeedPayload
    ));
    reader
        .push(
            bytes(&pool, &pes(b"\x01\x09\xf0", None, None, false)),
            true,
            SourceId(2),
            0,
        )
        .unwrap();
    let first = match reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap() {
        AssemblyStep::Output(value) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        first.payload().flatten().copied().collect::<Vec<_>>(),
        b"\0\0"
    );
    let next = match reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap() {
        AssemblyStep::Output(value) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(next.elementary_offset, 2);
    assert_eq!(next.header.pts_raw33, None);
    assert_eq!(next.header.dts_raw33, None);
    assert_eq!(
        next.payload().flatten().copied().collect::<Vec<_>>(),
        b"\x01\x09\xf0"
    );
}

#[test]
fn incomplete_finite_pes_is_truncation_and_zero_length_audio_is_invalid() {
    let pool = pool();
    let mut reader = PesAssembler::new(
        PID,
        true,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    let source = pes(b"missing", Some(0), None, false);
    reader
        .push(
            bytes(&pool, &source[..source.len() - 2]),
            true,
            SourceId(1),
            0,
        )
        .unwrap();
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap(),
        AssemblyStep::NeedPayload
    ));
    reader.end_of_source();
    assert_eq!(
        reader
            .poll(&mut WorkBudget::new(100), &cancel())
            .unwrap_err(),
        Error::Truncated
    );
    let mut audio = PesAssembler::new(
        PID,
        false,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    let mut source = pes(b"x", None, None, true);
    source[3] = 0xc0;
    audio
        .push(bytes(&pool, &source), true, SourceId(1), 0)
        .unwrap();
    assert!(audio.poll(&mut WorkBudget::new(100), &cancel()).is_err());
}

#[test]
fn cancellation_and_limits_leave_the_input_observable_for_explicit_recovery() {
    let pool = pool();
    let mut reader = PesAssembler::new(
        PID,
        true,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    reader
        .push(
            bytes(&pool, &pes(b"keep", None, None, false)),
            true,
            SourceId(4),
            500,
        )
        .unwrap();
    let cancelled = cancel();
    cancelled.store(true, Ordering::Relaxed);
    assert_eq!(
        reader
            .poll(&mut WorkBudget::new(100), &cancelled)
            .unwrap_err(),
        Error::Cancelled
    );
    cancelled.store(false, Ordering::Relaxed);
    let item = match reader.poll(&mut WorkBudget::new(100), &cancelled).unwrap() {
        AssemblyStep::Output(value) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        item.payload().flatten().copied().collect::<Vec<_>>(),
        b"keep"
    );
    let limits = ParseLimits {
        pes_bytes: 16,
        ..ParseLimits::default()
    };
    let mut bounded =
        PesAssembler::new(PID, true, pool.clone(), limits, SourceEpochs::default()).unwrap();
    bounded
        .push(
            bytes(&pool, &pes(&[0; 40], None, None, false)),
            true,
            SourceId(1),
            0,
        )
        .unwrap();
    assert!(matches!(
        bounded.poll(&mut WorkBudget::new(100), &cancel()),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn retained_owned_pes_keeps_its_hidden_source_capacity_charged_after_parser_drop() {
    let pool = pool();
    let baseline = pool.used();
    let mut storage = Vec::with_capacity(4096);
    storage.extend_from_slice(&pes(b"retained", None, None, false));
    let source = Bytes::try_from_vec(&pool, storage).unwrap();
    let mut reader = PesAssembler::new(
        PID,
        true,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    reader.push(source, true, SourceId(1), 0).unwrap();
    let item = match reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap() {
        AssemblyStep::Output(value) => value,
        other => panic!("{other:?}"),
    };
    drop(reader);
    assert!(pool.used() >= baseline + 4096);
    assert_eq!(
        item.payload().flatten().copied().collect::<Vec<_>>(),
        b"retained"
    );
    drop(item);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn entity_eof_is_separate_from_logical_pes_eof_in_the_concrete_cursor() {
    let pool = pool();
    let original = vec![0x5a; 300];
    let data = pes(&original, Some(90_000), None, false);
    let first = packet(&data[..180], 0, true, None);
    let second = packet(&data[180..], 1, false, None);
    let mut input = InputCursor::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(188),
            random_access: false,
        },
        192,
    )
    .unwrap();
    let mut framer = TransportFramer::new(pool.clone(), PacketFormat::Ts188);
    let mut assembler = PesAssembler::new(
        PID,
        true,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    for (index, source) in [first, second].iter().enumerate() {
        if index != 0 {
            input
                .continue_source(
                    SourceId(2),
                    SourceCapabilities {
                        length: Some(188),
                        random_access: false,
                    },
                )
                .unwrap();
        }
        let request = input.request();
        input
            .provide(
                &InputWindow::new(
                    request.source,
                    request.token.clone(),
                    0,
                    bytes(&pool, source),
                )
                .unwrap(),
            )
            .unwrap();
        input.finish(request.source, request.token).unwrap();
        let packet = loop {
            match framer
                .poll(&mut input, &mut WorkBudget::new(1), &cancel())
                .unwrap()
            {
                Step::Ready(value) => break value,
                Step::Yield => {}
                other => panic!("{other:?}"),
            }
        };
        let source_id = packet.source_id.unwrap();
        assembler
            .push(
                packet.payload.unwrap(),
                packet.payload_unit_start,
                source_id,
                packet.payload_source_offset,
            )
            .unwrap();
        match assembler
            .poll(&mut WorkBudget::new(1000), &cancel())
            .unwrap()
        {
            AssemblyStep::NeedPayload if index == 0 => {}
            AssemblyStep::Output(pes) if index == 1 => {
                assert_eq!(
                    pes.payload().flatten().copied().collect::<Vec<_>>(),
                    original
                );
                assert_eq!(
                    pes.source_ranges()
                        .map(|range| range.entity)
                        .collect::<Vec<_>>(),
                    [SourceId(1), SourceId(2)]
                );
                assert_eq!(
                    pes.header.raw_pts_time().unwrap(),
                    Some(Time::new(1, 1).unwrap())
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            framer
                .poll(&mut input, &mut WorkBudget::new(1), &cancel())
                .unwrap(),
            Step::End(_)
        ));
    }
}

#[test]
fn transport_failure_and_reset_cannot_be_reinterpreted_as_eof_or_old_packet_bytes() {
    let pool = pool();
    let mut input = InputCursor::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        192,
    )
    .unwrap();
    let mut framer = TransportFramer::new(pool.clone(), PacketFormat::Ts188);
    let request = input.request();
    let source = packet(b"never fabricate", 0, true, None);
    input
        .provide(
            &InputWindow::new(
                request.source,
                request.token.clone(),
                0,
                bytes(&pool, &source[..50]),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        framer
            .poll(&mut input, &mut WorkBudget::new(100), &cancel())
            .unwrap(),
        Step::NeedInput(_)
    ));
    input
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        framer.poll(&mut input, &mut WorkBudget::new(100), &cancel()),
        Err(Error::Transport)
    ));
    input.reset(0).unwrap();
    assert!(matches!(
        framer.poll(&mut input, &mut WorkBudget::new(100), &cancel()),
        Err(Error::StaleGeneration { .. })
    ));
    framer.discard();
    assert!(matches!(
        framer
            .poll(&mut input, &mut WorkBudget::new(100), &cancel())
            .unwrap(),
        Step::NeedInput(_)
    ));
}

#[test]
fn psi_backpressure_keeps_completed_and_partial_source_bytes_for_exactly_once_retry() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = SectionAssembler::new(0, pool.clone(), ParseLimits::default()).unwrap();
    let mut first = vec![0];
    first.extend_from_slice(&PAT[..8]);
    reader
        .push(bytes(&pool, &first), true, SourceId(1), 0)
        .unwrap();
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap(),
        AssemblyStep::NeedPayload
    ));
    reader
        .push(bytes(&pool, &PAT[8..]), false, SourceId(1), 188)
        .unwrap();
    let held = pool.reserve(pool.available() - 1).unwrap();
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(100), &cancel()),
        Err(Error::Backpressure { .. })
    ));
    assert!(PAT.starts_with(reader.partial_bytes()));
    drop(held);
    let section = match reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap() {
        AssemblyStep::Output(value) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(section.bytes().as_ref(), PAT);
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap(),
        AssemblyStep::NeedPayload
    ));
}

#[test]
fn pes_span_backpressure_and_one_unit_work_do_not_change_packet_boundaries() {
    let pool = Budget::new(8192).unwrap();
    let data = pes(b"unchanged", None, None, false);
    let mut reader = PesAssembler::new(
        PID,
        true,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    reader
        .push(bytes(&pool, &data[..9]), true, SourceId(1), 0)
        .unwrap();
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap(),
        AssemblyStep::NeedPayload
    ));
    reader
        .push(bytes(&pool, &data[9..]), false, SourceId(1), 188)
        .unwrap();
    let held = pool.reserve(pool.available() - 1).unwrap();
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(100), &cancel()),
        Err(Error::Backpressure { .. })
    ));
    drop(held);
    let output = loop {
        match reader.poll(&mut WorkBudget::new(1), &cancel()).unwrap() {
            AssemblyStep::Output(value) => break value,
            AssemblyStep::Yield => {}
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(output.spans().len(), 1);
    assert_eq!(
        output.payload().flatten().copied().collect::<Vec<_>>(),
        b"unchanged"
    );
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(100), &cancel()).unwrap(),
        AssemblyStep::NeedPayload
    ));
}

#[test]
fn network_pid_zero_program_is_preserved_as_a_non_playable_association() {
    let pool = pool();
    let mut raw = PAT.to_vec();
    raw[8] = 0;
    raw[9] = 0;
    raw[10] = 0xe0;
    raw[11] = 0x10;
    let checksum = crc(&raw[..12]);
    raw[12..].copy_from_slice(&checksum.to_be_bytes());
    let original = section(&pool, &raw, 1);
    let mut reader = PatReader::new(&original, &pool, ParseLimits::default()).unwrap();
    let result = match reader.poll(&mut WorkBudget::new(1), &cancel()).unwrap() {
        TableStep::Output(value) => value,
        other => panic!("{other:?}"),
    };
    assert!(result.programs.as_slice()[0].is_network_pid());
    assert_eq!(result.programs.as_slice()[0].pid, 0x10);
    drop(original);
    assert_eq!(result.source_ranges.as_slice()[0].offset, (1_u64 << 40) + 1);
}

#[test]
fn a_header_only_finite_pes_finalizes_without_needing_another_source_byte() {
    let pool = pool();
    let mut reader = PesAssembler::new(
        PID,
        true,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    reader
        .push(
            bytes(&pool, &pes(b"", None, None, false)),
            true,
            SourceId(1),
            0,
        )
        .unwrap();
    let result = loop {
        match reader.poll(&mut WorkBudget::new(1), &cancel()).unwrap() {
            AssemblyStep::Output(value) => break value,
            AssemblyStep::Yield => {}
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(result.payload_len, 0);
    assert!(result.spans().is_empty());
    assert_eq!(result.header.as_bytes(), &[0, 0, 1, 0xe0, 0, 3, 0x80, 0, 0]);
    reader.end_of_source();
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(1), &cancel()).unwrap(),
        AssemblyStep::Finished
    ));
}

#[test]
fn forbidden_timing_flags_and_marker_bits_preserve_bad_header_for_diagnosis() {
    let pool = pool();
    for marker in [false, true] {
        let mut raw = pes(b"invalid", Some(0), None, false);
        if marker {
            raw[11] &= 0xfe;
        } else {
            raw[7] = 0x40;
        }
        let mut reader = PesAssembler::new(
            PID,
            true,
            pool.clone(),
            ParseLimits::default(),
            SourceEpochs::default(),
        )
        .unwrap();
        reader
            .push(bytes(&pool, &raw), true, SourceId(1), 0)
            .unwrap();
        assert!(matches!(
            reader.poll(&mut WorkBudget::new(100), &cancel()),
            Err(Error::Invalid(_))
        ));
        assert_eq!(reader.partial_header(), &raw[..14]);
    }
}
