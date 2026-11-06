#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    AacAdtsFrame, AacAdtsOutput, AacAdtsParameters, AacAdtsParser, AacAdtsWriter, AacAscParser,
    AacConfigLimits, AacSource, BuildStep, inspect_aac_adts_header,
};
fn source() -> AacSource {
    AacSource {
        source: SourceId(4),
        generation: 7,
        offset: 113,
    }
}
fn params() -> AacAdtsParameters {
    AacAdtsParameters {
        mpeg2_id: false,
        frequency_index: 3,
        channel_configuration: 2,
        buffer_fullness: 0x7ff,
        private_bit: false,
        original_copy: false,
        home: false,
        copyright_id_bit: false,
        copyright_id_start: false,
    }
}
fn literal(
    id: bool,
    crc: Option<u16>,
    frequency: u8,
    channel: u8,
    fullness: u16,
    flags: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut bits = Vec::new();
    for (value, width) in [
        (0xfff, 12),
        (u32::from(id), 1),
        (0, 2),
        (u32::from(crc.is_none()), 1),
        (1, 2),
        (u32::from(frequency), 4),
        (u32::from(flags & 1 != 0), 1),
        (u32::from(channel), 3),
        (u32::from(flags >> 1), 4),
        (
            (payload.len() + if crc.is_some() { 9 } else { 7 }) as u32,
            13,
        ),
        (u32::from(fullness), 11),
        (0, 2),
    ] {
        for at in (0..width).rev() {
            bits.push(value & (1 << at) != 0);
        }
    }
    assert_eq!(bits.len(), 56);
    let mut out = Vec::new();
    for byte in bits.as_chunks::<8>().0 {
        let mut value = 0;
        for bit in byte {
            value = (value << 1) | u8::from(*bit);
        }
        out.push(value)
    }
    if let Some(word) = crc {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}
fn parse(pool: &Budget, raw: Bytes) -> Result<AacAdtsFrame, Error> {
    let mut p = AacAdtsParser::new(pool, source(), raw, 8191)?;
    loop {
        match p.step(&mut WorkBudget::new(1), &AtomicBool::new(false))? {
            BuildStep::Ready(o) => return Ok(o),
            BuildStep::Yield => {}
            BuildStep::Complete => panic!(),
        }
    }
}
fn write(
    pool: &Budget,
    raw: Bytes,
    parameters: AacAdtsParameters,
    work: u64,
) -> Result<AacAdtsOutput, Error> {
    let mut w = AacAdtsWriter::new(pool, source(), raw, parameters, 8191)?;
    for _ in 0..10000 {
        match w.step(&mut WorkBudget::new(work), &AtomicBool::new(false))? {
            BuildStep::Ready(o) => {
                assert!(matches!(
                    w.step(&mut WorkBudget::new(0), &AtomicBool::new(false))?,
                    BuildStep::Complete
                ));
                return Ok(o);
            }
            BuildStep::Yield => {}
            BuildStep::Complete => panic!(),
        }
    }
    panic!("bounded writer failed to finish")
}

#[test]
fn complete_known_frame_and_constructed_header_match_literal_bytes() {
    let expected = [0xff, 0xf1, 0x4c, 0x80, 0x01, 0x3f, 0xfc, 0x21, 0x10];
    let pool = Budget::new(100_000).unwrap();
    let frame = parse(&pool, Bytes::copy_from_slice(&pool, &expected).unwrap()).unwrap();
    assert_eq!(frame.source_bytes().as_ref(), expected);
    assert_eq!(frame.payload_bytes().as_ref(), [0x21, 0x10]);
    assert_eq!(frame.source(), source());
    assert_eq!(frame.crc_word(), None);
    let out = write(
        &pool,
        Bytes::copy_from_slice(&pool, &[0x21, 0x10]).unwrap(),
        params(),
        1,
    )
    .unwrap();
    assert_eq!(out.bytes().as_ref(), expected);
    assert_eq!(out.payload_source(), source());
    assert_eq!(out.payload_bytes().unwrap().as_ref(), [0x21, 0x10]);
    assert_eq!(out.constructed_header(), frame.header());
}

#[test]
fn every_boolean_header_field_frequency_channel_and_fullness_is_independent() {
    let pool = Budget::new(100_000).unwrap();
    for id in [false, true] {
        for flags in 0..32 {
            let raw = literal(id, None, 12, 7, 0x456, flags, &[0x80]);
            let o = parse(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap()).unwrap();
            let h = o.header();
            assert_eq!(h.mpeg2_id(), id);
            assert_eq!(h.private_bit(), flags & 1 != 0);
            assert_eq!(h.original_copy(), flags & 16 != 0);
            assert_eq!(h.home(), flags & 8 != 0);
            assert_eq!(h.copyright_id_bit(), flags & 4 != 0);
            assert_eq!(h.copyright_id_start(), flags & 2 != 0);
            assert_eq!(h.buffer_fullness(), 0x456);
            assert_eq!(o.source_bytes().as_ref(), raw);
        }
    }
    for frequency in 0..13 {
        for channel in 0..8 {
            for fullness in [0, 1, 1024, 2047] {
                let raw = literal(false, None, frequency, channel, fullness, 0, &[0]);
                let h = parse(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap())
                    .unwrap()
                    .header();
                assert_eq!(
                    (
                        h.frequency_index(),
                        h.channel_configuration(),
                        h.buffer_fullness()
                    ),
                    (frequency, channel, fullness)
                );
                assert!(h.coded_frequency_hz().is_some());
                assert_eq!(
                    h.coded_channel_count(),
                    match channel {
                        0 => None,
                        7 => Some(8),
                        _ => Some(channel),
                    }
                );
            }
        }
    }
}

#[test]
fn protected_single_block_retains_exact_crc_word_and_every_payload_byte() {
    let pool = Budget::new(100_000).unwrap();
    for word in [0, 1, 0x8000, 0xffff] {
        let raw = literal(true, Some(word), 4, 0, 0, 31, &[0xff, 0, 0x81, 0]);
        let o = parse(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap()).unwrap();
        assert_eq!(o.crc_word(), Some(word));
        assert_eq!(o.header().single_block_header_bytes(), Some(9));
        assert_eq!(o.payload_bytes().as_ref(), [0xff, 0, 0x81, 0]);
        assert_eq!(o.source_bytes().as_ref(), raw);
        assert!(o.payload_bytes().shares_storage_with(o.source_bytes()));
    }
}

#[test]
fn all_physical_truncations_and_unadvertised_trailing_bytes_are_failures() {
    let pool = Budget::new(100_000).unwrap();
    for crc in [None, Some(0xabcd)] {
        let raw = literal(false, crc, 3, 2, 2047, 0, &[1, 2, 3, 4]);
        for end in 0..raw.len() {
            assert!(
                matches!(
                    parse(&pool, Bytes::copy_from_slice(&pool, &raw[..end]).unwrap()),
                    Err(Error::Truncated)
                ),
                "end={end}"
            );
        }
        let mut extra = raw.clone();
        extra.push(0);
        assert!(matches!(
            parse(&pool, Bytes::copy_from_slice(&pool, &extra).unwrap()),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn fixed_source_view_retains_every_unselected_profile_and_block_layout() {
    let pool = Budget::new(100_000).unwrap();
    for profile in [0, 2, 3] {
        let mut raw = literal(false, None, 3, 2, 0, 0, &[0]);
        raw[2] = (raw[2] & 63) | (profile << 6);
        let h = inspect_aac_adts_header(&raw).unwrap();
        assert_eq!(h.profile_code(), profile);
        assert_eq!(h.raw_prefix().as_slice(), &raw[..7]);
        assert!(matches!(
            parse(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap()),
            Err(Error::Unsupported(_))
        ));
    }
    for crc in [None, Some(0)] {
        for blocks in 1..4 {
            let mut raw = literal(false, crc, 3, 2, 0, 0, &[0; 16]);
            raw[6] |= blocks;
            let h = inspect_aac_adts_header(&raw).unwrap();
            assert_eq!(h.raw_blocks_minus_one(), blocks);
            assert_eq!(h.single_block_header_bytes(), None);
            assert!(matches!(
                parse(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap()),
                Err(Error::Unsupported(_))
            ));
        }
    }
}

#[test]
fn malformed_sync_layer_frequency_and_declared_header_extent_are_explicit() {
    let pool = Budget::new(100_000).unwrap();
    let base = literal(false, None, 3, 2, 0, 0, &[0]);
    let mut cases = Vec::new();
    let mut x = base.clone();
    x[0] = 0;
    cases.push(x);
    let mut x = base.clone();
    x[1] |= 2;
    cases.push(x);
    for index in 13..16 {
        let mut x = base.clone();
        x[2] = (x[2] & 0xc3) | (index << 2);
        cases.push(x);
    }
    let mut x = base.clone();
    x[3] &= 0xfc;
    x[4] = 0;
    x[5] &= 31;
    cases.push(x);
    for raw in cases {
        assert!(matches!(
            parse(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap()),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn all_selected_frame_sizes_include_empty_payload_as_an_explicit_wire_issue() {
    let pool = Budget::new(100_000).unwrap();
    for size in [0, 1, 2, 128, 8184] {
        let payload: Vec<_> = (0..size).map(|n| n as u8).collect();
        let raw = literal(false, None, 3, 2, 2047, 0, &payload);
        let frame = parse(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap()).unwrap();
        assert_eq!(frame.source_bytes().len(), size + 7);
        assert_eq!(frame.payload_bytes().as_ref(), payload);
        assert_eq!(
            frame.header().selected_wire_issues() & 32,
            if size == 0 { 32 } else { 0 }
        );
        for work in [1, 7, 4096] {
            let out = write(
                &pool,
                Bytes::copy_from_slice(&pool, &payload).unwrap(),
                params(),
                work,
            )
            .unwrap();
            assert_eq!(out.bytes().as_ref(), raw);
        }
    }
    assert!(matches!(
        AacAdtsWriter::new(
            &pool,
            source(),
            Bytes::copy_from_slice(&pool, &[0; 8185]).unwrap(),
            params(),
            100_000
        ),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn tiny_complete_frames_cannot_retain_a_maximum_sized_output_allocation() {
    let pool = Budget::new(1000).unwrap();
    let baseline = pool.used();
    let out = write(
        &pool,
        Bytes::copy_from_slice(&pool, &[0; 112]).unwrap(),
        params(),
        1,
    )
    .unwrap();
    assert_eq!(out.bytes().len(), 119);
    assert!(out.bytes().retained_capacity() < 1000);
    assert!(pool.used() < 1000);
    drop(out);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn reserve_and_freeze_retry_preserve_real_copied_output_and_zero_work() {
    let pool = Budget::new(2000).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xa5; 64]).unwrap();
    let baseline = pool.used();
    let mut blocker = BudgetVec::<u8>::new(&pool);
    blocker.try_reserve_exact(pool.available() - 71).unwrap();
    let mut w = AacAdtsWriter::new(&pool, source(), raw.clone(), params(), 8191).unwrap();
    assert!(matches!(
        w.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    let charged = pool.used();
    assert!(charged > baseline);
    assert!(matches!(
        w.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(BuildStep::Yield)
    ));
    assert_eq!(pool.used(), charged);
    assert!(matches!(
        w.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(pool.used(), charged);
    drop(blocker);
    let out = match w
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        BuildStep::Ready(out) => out,
        _ => panic!("completed copy must not repeat on freeze retry"),
    };
    assert_eq!(out.payload_bytes().unwrap().as_ref(), [0xa5; 64]);
    drop(out);
    assert_eq!(pool.used(), baseline);
    let mut blocker = BudgetVec::<u8>::new(&pool);
    blocker.try_reserve_exact(pool.available() - 1).unwrap();
    let mut w = AacAdtsWriter::new(&pool, source(), raw.clone(), params(), 8191).unwrap();
    assert!(matches!(
        w.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    let out = match w
        .step(&mut WorkBudget::new(1000), &AtomicBool::new(false))
        .unwrap()
    {
        BuildStep::Ready(out) => out,
        _ => panic!(),
    };
    assert_eq!(out.payload_bytes().unwrap().as_ref(), raw.as_ref());
}

#[test]
fn parser_writer_zero_work_cancel_and_fatal_errors_are_sticky_and_release_scratch() {
    let pool = Budget::new(100_000).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &literal(false, None, 3, 2, 0, 0, &[0; 64])).unwrap();
    let baseline = pool.used();
    let mut p = AacAdtsParser::new(&pool, source(), raw.clone(), 8191).unwrap();
    assert!(matches!(
        p.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(BuildStep::Yield)
    ));
    assert_eq!(pool.used(), baseline);
    assert!(matches!(
        p.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        p.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
    let payload = Bytes::copy_from_slice(&pool, &[0; 64]).unwrap();
    let baseline = pool.used();
    let mut w = AacAdtsWriter::new(&pool, source(), payload.clone(), params(), 8191).unwrap();
    assert!(matches!(
        w.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(BuildStep::Yield)
    ));
    assert!(pool.used() > baseline);
    assert!(matches!(
        w.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), baseline);
    assert!(matches!(
        w.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
    let mut malformed = literal(false, None, 3, 2, 0, 0, &[0]);
    malformed[0] = 0;
    let mut p = AacAdtsParser::new(
        &pool,
        source(),
        Bytes::copy_from_slice(&pool, &malformed).unwrap(),
        8191,
    )
    .unwrap();
    let e = match p.step(&mut WorkBudget::new(100), &AtomicBool::new(false)) {
        Err(e) => e,
        _ => panic!(),
    };
    assert!(
        matches!(p.step(&mut WorkBudget::new(0),&AtomicBool::new(false)),Err(other) if other==e)
    );
}

#[test]
fn original_asc_core_parameters_do_not_substitute_render_rate_or_inferred_stereo() {
    let pool = Budget::new(100_000).unwrap();
    for (raw, core_index, channel) in [([0x2b, 0x09, 0x88, 0], 6, 1), ([0xeb, 0x09, 0x88, 0], 6, 1)]
    {
        let mut p = AacAscParser::new(
            &pool,
            source(),
            Bytes::copy_from_slice(&pool, &raw).unwrap(),
            AacConfigLimits {
                maximum_record_bytes: 100,
                maximum_pce_elements: 70,
                maximum_comment_bytes: 255,
            },
        )
        .unwrap();
        let asc = match p
            .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(o) => o,
            _ => panic!(),
        };
        let params = AacAdtsParameters::from_asc(asc.facts()).unwrap();
        assert_eq!(
            (params.frequency_index, params.channel_configuration),
            (core_index, channel)
        );
        let o = write(
            &pool,
            Bytes::copy_from_slice(&pool, &[0x80]).unwrap(),
            params,
            1,
        )
        .unwrap();
        assert_eq!(o.constructed_header().coded_frequency_hz(), Some(24000));
        assert_eq!(o.constructed_header().coded_channel_count(), Some(1));
    }
}

#[test]
fn bounds_foreign_pool_absolute_offset_and_parameter_widths_are_checked() {
    let a = Budget::new(100_000).unwrap();
    let b = Budget::new(100_000).unwrap();
    let raw = Bytes::copy_from_slice(&a, &[0; 10]).unwrap();
    assert!(matches!(
        AacAdtsWriter::new(&b, source(), raw.clone(), params(), 8191),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        AacAdtsParser::new(&b, source(), raw.clone(), 8191),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        AacAdtsWriter::new(
            &a,
            AacSource {
                offset: u64::MAX,
                ..source()
            },
            raw.clone(),
            params(),
            8191
        ),
        Err(Error::Overflow)
    ));
    assert!(matches!(
        AacAdtsWriter::new(&a, source(), raw.clone(), params(), 16),
        Err(Error::Limit { .. })
    ));
    for p in [
        AacAdtsParameters {
            frequency_index: 13,
            ..params()
        },
        AacAdtsParameters {
            channel_configuration: 8,
            ..params()
        },
        AacAdtsParameters {
            buffer_fullness: 2048,
            ..params()
        },
    ] {
        assert!(matches!(
            AacAdtsWriter::new(&a, source(), raw.clone(), p, 8191),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn retained_parser_payload_and_writer_output_drop_their_last_charge_across_threads() {
    let pool = Budget::new(100_000).unwrap();
    let baseline = pool.used();
    let frame = parse(
        &pool,
        Bytes::copy_from_slice(
            &pool,
            &literal(false, Some(0xffff), 3, 2, 0, 0, &[0x83, 0, 0xff]),
        )
        .unwrap(),
    )
    .unwrap();
    let payload = frame.payload_bytes().clone();
    drop(frame);
    assert_eq!(payload.as_ref(), [0x83, 0, 0xff]);
    assert!(pool.used() > baseline);
    std::thread::spawn(move || drop(payload)).join().unwrap();
    assert_eq!(pool.used(), baseline);
    let output = write(
        &pool,
        Bytes::copy_from_slice(&pool, &[0x83, 0, 0xff]).unwrap(),
        params(),
        1,
    )
    .unwrap();
    let payload = output.payload_bytes().unwrap();
    drop(output);
    assert!(pool.used() > baseline);
    std::thread::spawn(move || drop(payload)).join().unwrap();
    assert_eq!(pool.used(), baseline);
}
