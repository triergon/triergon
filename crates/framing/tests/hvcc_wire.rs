#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    BuildStep, HvcConfigArray, HvcConfigLimits, HvcConfigNal, HvcConfigParser, HvcConfigSource,
    HvcConfiguration, LengthSize, inspect_hvc_config_prefix,
};
fn limits() -> HvcConfigLimits {
    HvcConfigLimits {
        maximum_record_bytes: 1_000_000,
        maximum_arrays: 255,
        maximum_nals: 65535,
        maximum_nal_bytes: 65535,
    }
}
fn source() -> HvcConfigSource {
    HvcConfigSource {
        source: SourceId(342),
        generation: 9,
        offset: 37,
    }
}
fn prefix(width: u8, arrays: u8) -> Vec<u8> {
    vec![
        1,
        0xa5,
        0x81,
        0x23,
        0x45,
        0x67,
        0x92,
        0x34,
        0x56,
        0x78,
        0x9a,
        0xbc,
        123,
        0xfa,
        0xbc,
        0xfd,
        0xfe,
        0xfa,
        0xfb,
        0x12,
        0x34,
        0xb4 | (width - 1),
        arrays,
    ]
}
fn record(groups: &[(u8, bool, Vec<Vec<u8>>)], suffix: &[u8]) -> Vec<u8> {
    let mut out = prefix(4, groups.len() as u8);
    for (kind, complete, nals) in groups {
        out.push(*kind | if *complete { 128 } else { 0 });
        out.extend_from_slice(&(nals.len() as u16).to_be_bytes());
        for n in nals {
            out.extend_from_slice(&(n.len() as u16).to_be_bytes());
            out.extend_from_slice(n);
        }
    }
    out.extend_from_slice(suffix);
    out
}
fn nal(kind: u8, tag: u8) -> Vec<u8> {
    vec![kind << 1, 1, 0, 0, 3, tag]
}
fn example() -> Vec<u8> {
    record(
        &[
            (33, true, vec![nal(33, 9), nal(33, 9)]),
            (55, false, vec![]),
            (32, false, vec![nal(32, 7)]),
            (33, false, vec![nal(34, 6)]),
        ],
        &[],
    )
}
fn run(
    pool: &Budget,
    data: Bytes,
    l: HvcConfigLimits,
    work: u64,
) -> Result<HvcConfiguration, Error> {
    let mut p = HvcConfigParser::new(pool, source(), data, l)?;
    for _ in 0..2_000_000 {
        match p.step(&mut WorkBudget::new(work), &AtomicBool::new(false))? {
            BuildStep::Ready(out) => {
                assert!(matches!(
                    p.step(&mut WorkBudget::new(0), &AtomicBool::new(false))?,
                    BuildStep::Complete
                ));
                return Ok(out);
            }
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("completed without publication"),
        }
    }
    panic!("bounded configuration did not finish")
}
fn parse(data: &[u8]) -> Result<HvcConfiguration, Error> {
    let pool = Budget::new(50_000_000).unwrap();
    let b = Bytes::copy_from_slice(&pool, data).unwrap();
    run(&pool, b, limits(), 1)
}
#[test]
fn complete_prefix_fields_are_literal_full_width_source_facts() {
    let bytes = prefix(4, 0);
    let h = inspect_hvc_config_prefix(&bytes).unwrap();
    assert_eq!(h.raw_prefix().as_slice(), bytes);
    assert_eq!(
        (
            h.version(),
            h.profile_space(),
            h.tier_flag(),
            h.profile_idc()
        ),
        (1, 2, true, 5)
    );
    assert_eq!(h.profile_compatibility_flags(), 0x81234567);
    assert_eq!(h.constraint_indicator_flags(), 0x923456789abc);
    assert_eq!(h.level_idc(), 123);
    assert_eq!(
        (
            h.min_spatial_segmentation_idc(),
            h.parallelism_type(),
            h.chroma_format_idc()
        ),
        (0xabc, 1, 2)
    );
    assert_eq!(
        (
            h.bit_depth_luma_minus8(),
            h.bit_depth_chroma_minus8(),
            h.average_frame_rate()
        ),
        (2, 3, 0x1234)
    );
    assert_eq!(
        (
            h.constant_frame_rate(),
            h.num_temporal_layers(),
            h.temporal_id_nested(),
            h.length_size_minus_one(),
            h.array_count()
        ),
        (2, 6, true, 3, 0)
    );
    assert_eq!(h.syntax_issues(), 0);
}
#[test]
fn every_temporal_flag_combination_is_independent_of_length_and_nesting() {
    for rate in 0..4 {
        for layers in 0..8 {
            for nested in [false, true] {
                for code in 0..4 {
                    let mut b = prefix(4, 0);
                    b[21] = (rate << 6) | (layers << 3) | (u8::from(nested) << 2) | code;
                    let h = inspect_hvc_config_prefix(&b).unwrap();
                    assert_eq!(
                        (
                            h.constant_frame_rate(),
                            h.num_temporal_layers(),
                            h.temporal_id_nested(),
                            h.length_size_minus_one()
                        ),
                        (rate, layers, nested, code)
                    );
                }
            }
        }
    }
}
#[test]
fn empty_arrays_and_empty_configuration_are_distinct_and_owned() {
    let a = parse(&prefix(1, 0)).unwrap();
    assert_eq!(a.length_size(), LengthSize::new(1).unwrap());
    assert!(a.arrays().is_empty());
    assert!(a.nals().is_empty());
    let b = record(&[(32, true, vec![]), (55, false, vec![])], &[]);
    let o = parse(&b).unwrap();
    assert_eq!(o.arrays().len(), 2);
    assert!(o.nals().is_empty());
    assert_eq!(o.source_bytes().as_ref(), b);
    assert_eq!(o.arrays()[0].nal_indices(), 0..0);
    assert_eq!(o.arrays()[1].declared_nal_type(), 55);
}
#[test]
fn all_arrays_nals_duplicates_unknown_types_order_and_emulation_bytes_survive() {
    let b = example();
    let o = parse(&b).unwrap();
    assert_eq!(o.source(), source());
    assert_eq!(o.source_bytes().as_ref(), b);
    assert_eq!(o.arrays().len(), 4);
    assert_eq!(o.nals().len(), 4);
    assert_eq!(
        o.nals()
            .iter()
            .map(HvcConfigNal::declared_nal_type)
            .collect::<Vec<_>>(),
        vec![33, 33, 32, 33]
    );
    assert_eq!(o.nals()[0].source_bytes().as_ref(), nal(33, 9));
    assert_eq!(o.nals()[1].source_bytes().as_ref(), nal(33, 9));
    assert_eq!(o.nals()[3].header().nal_unit_type(), 34);
    assert!(!o.nals()[3].type_matches_declaration());
    assert_ne!(o.syntax_issues() & 128, 0);
    for n in o.nals() {
        let range = n.source_range();
        assert_eq!(&b[range], n.source_bytes().as_ref());
        assert!(n.source_bytes().shares_storage_with(o.source_bytes()));
        assert_eq!(
            u16::from_be_bytes(n.length_field_bytes().as_ref().try_into().unwrap()) as usize,
            n.source_bytes().len()
        );
    }
    for a in o.arrays() {
        assert_eq!(
            &b[a.source_range().start..a.source_range().start + 3],
            a.source_header_bytes().as_ref()
        );
    }
}
#[test]
fn all_five_reserved_patterns_and_array_reserved_bit_are_retained_not_repaired() {
    for (at, mask, bit) in [
        (13, 0xf0, 0),
        (15, 0xfc, 1),
        (16, 0xfc, 2),
        (17, 0xf8, 3),
        (18, 0xf8, 4),
    ] {
        let mut b = record(&[(32, true, vec![nal(32, 2)])], &[]);
        b[at] &= !mask;
        let o = parse(&b).unwrap();
        assert_eq!(o.source_bytes().as_ref(), b);
        assert_ne!(o.syntax_issues() & (1 << bit), 0);
    }
    let mut b = record(&[(55, false, vec![])], &[]);
    b[23] |= 64;
    let o = parse(&b).unwrap();
    assert!(o.arrays()[0].reserved_bit());
    assert_ne!(o.syntax_issues() & 64, 0);
}
#[test]
fn one_two_four_widths_are_parsed_three_is_explicit_unsupported() {
    for (width, expected) in [
        (1, LengthSize::new(1).unwrap()),
        (2, LengthSize::new(2).unwrap()),
        (4, LengthSize::new(4).unwrap()),
    ] {
        let b = prefix(width, 0);
        assert_eq!(parse(&b).unwrap().length_size(), expected);
    }
    let b = prefix(3, 0);
    assert_eq!(
        inspect_hvc_config_prefix(&b)
            .unwrap()
            .length_size_minus_one(),
        2
    );
    assert!(matches!(parse(&b), Err(Error::Unsupported(_))));
}
#[test]
fn future_versions_are_unsupported_without_changing_declared_opaque() {
    for version in [0, 2, 255] {
        let mut b = prefix(4, 0);
        b[0] = version;
        assert!(matches!(parse(&b), Err(Error::Unsupported(_))));
    }
}
#[test]
fn every_structure_byte_truncation_is_an_actual_error() {
    let b = example();
    for end in 0..b.len() {
        assert!(
            matches!(parse(&b[..end]), Err(Error::Truncated)),
            "end={end}"
        );
    }
}
#[test]
fn complete_suffix_is_owned_opaque_with_exact_understood_end() {
    let suffix = [0xa5, 0, 0xff, 0x80];
    let base = record(&[(32, true, vec![nal(32, 3)])], &[]);
    let mut b = base.clone();
    b.extend_from_slice(&suffix);
    let o = parse(&b).unwrap();
    assert_eq!(o.understood_end(), base.len());
    assert_eq!(o.opaque_suffix().unwrap().as_ref(), suffix);
    assert_eq!(o.source_bytes().as_ref(), b);
}
#[test]
fn zero_length_short_forbidden_and_zero_temporal_nals_are_never_certified() {
    for n in [vec![], vec![64], vec![128, 1], vec![64, 0]] {
        let b = record(&[(32, true, vec![n])], &[]);
        assert!(parse(&b).is_err());
    }
}
#[test]
fn all255_arrays_and_all65535_nal_count_values_are_bounded_and_supported() {
    let groups: Vec<_> = (0..255)
        .map(|n| ((n % 64) as u8, n % 2 != 0, vec![]))
        .collect();
    let b = record(&groups, &[]);
    let o = parse(&b).unwrap();
    assert_eq!(o.arrays().len(), 255);
    let groups = [(32, true, vec![vec![64, 1]; 65535])];
    let b = record(&groups, &[]);
    let o = parse(&b).unwrap();
    assert_eq!(o.nals().len(), 65535);
    assert_eq!(o.arrays()[0].nal_indices(), 0..65535);
}
#[test]
fn record_array_nal_count_and_size_limits_reject_without_truncating_output() {
    let pool = Budget::new(500_000).unwrap();
    let b = example();
    for l in [
        HvcConfigLimits {
            maximum_record_bytes: b.len() - 1,
            ..limits()
        },
        HvcConfigLimits {
            maximum_arrays: 3,
            ..limits()
        },
        HvcConfigLimits {
            maximum_nals: 3,
            ..limits()
        },
        HvcConfigLimits {
            maximum_nal_bytes: 5,
            ..limits()
        },
    ] {
        let r = run(&pool, Bytes::copy_from_slice(&pool, &b).unwrap(), l, 31);
        assert!(matches!(r, Err(Error::Limit { .. })));
    }
}
#[test]
fn zero_work_no_allocation_and_atomic_cancel_are_sticky() {
    let pool = Budget::new(500_000).unwrap();
    let data = Bytes::copy_from_slice(&pool, &example()).unwrap();
    let baseline = pool.used();
    let mut p = HvcConfigParser::new(&pool, source(), data.clone(), limits()).unwrap();
    assert!(matches!(
        p.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(BuildStep::Yield)
    ));
    assert_eq!(pool.used(), baseline);
    assert!(matches!(
        p.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), baseline);
    assert!(matches!(
        p.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
}
#[test]
fn descriptor_backpressure_retries_the_same_complete_record() {
    let pool = Budget::new(10_000).unwrap();
    let raw = example();
    let data = Bytes::copy_from_slice(&pool, &raw).unwrap();
    let needed = 4 * size_of::<HvcConfigArray>() + 4 * size_of::<HvcConfigNal>();
    let mut blocker = BudgetVec::<u8>::new(&pool);
    blocker
        .try_reserve_exact(pool.available() - needed / 2)
        .unwrap();
    let mut p = HvcConfigParser::new(&pool, source(), data.clone(), limits()).unwrap();
    assert!(matches!(
        p.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    let o = loop {
        match p
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(o) => break o,
            BuildStep::Yield => {}
            BuildStep::Complete => panic!(),
        }
    };
    assert_eq!(o.source_bytes().as_ref(), raw);
    assert_eq!(o.nals().len(), 4);
}
#[test]
fn foreign_pool_and_absolute_source_overflow_are_explicit() {
    let a = Budget::new(100_000).unwrap();
    let b = Budget::new(100_000).unwrap();
    let data = Bytes::copy_from_slice(&a, &prefix(4, 0)).unwrap();
    assert!(matches!(
        HvcConfigParser::new(&b, source(), data.clone(), limits()),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        HvcConfigParser::new(
            &a,
            HvcConfigSource {
                offset: u64::MAX,
                ..source()
            },
            data,
            limits()
        ),
        Err(Error::Overflow)
    ));
}
#[test]
fn caller_retains_exact_charged_payload_after_parser_input_close_and_thread_release() {
    let pool = Budget::new(500_000).unwrap();
    let baseline = pool.used();
    let raw = example();
    let data = Bytes::copy_from_slice(&pool, &raw).unwrap();
    let o = run(&pool, data, limits(), 1).unwrap();
    let n = o.nals()[0].source_bytes().clone();
    drop(o);
    assert!(pool.used() > baseline);
    assert_eq!(n.as_ref(), nal(33, 9));
    std::thread::spawn(move || drop(n)).join().unwrap();
    assert_eq!(pool.used(), baseline);
}

#[test]
fn array_extents_cover_each_physical_header_length_and_payload_once() {
    let mut raw = example();
    raw.extend_from_slice(&[0x80, 0, 0]);
    let output = parse(&raw).unwrap();
    let mut reconstructed = raw[..23].to_vec();
    let mut position = 23;
    for (index, array) in output.arrays().iter().enumerate() {
        let range = array.source_range();
        assert_eq!(range.start, position);
        let mut end = range.start + 3;
        for nal in &output.nals()[array.nal_indices()] {
            assert_eq!(nal.array_index(), index);
            assert_eq!(nal.source_range().start, end + 2);
            end = nal.source_range().end;
        }
        assert_eq!(range.end, end);
        reconstructed.extend_from_slice(&raw[range.clone()]);
        position = range.end;
    }
    assert_eq!(position, output.understood_end());
    reconstructed.extend_from_slice(output.opaque_suffix().unwrap().as_ref());
    assert_eq!(reconstructed, raw);
}

#[test]
fn maximum_nal_extent_and_each_sample_work_partition_preserve_the_full_record() {
    let mut big = vec![0x5a; 65535];
    big[..2].copy_from_slice(&[64, 1]);
    let raw = record(&[(32, true, vec![big])], &[0xfe, 0xed]);
    for work in [1, 2, 3, 7, 256, 1000] {
        let pool = Budget::new(500_000).unwrap();
        let output = run(
            &pool,
            Bytes::copy_from_slice(&pool, &raw).unwrap(),
            limits(),
            work,
        )
        .unwrap();
        assert_eq!(output.nals()[0].source_bytes().len(), 65535);
        assert_eq!(output.source_bytes().as_ref(), raw);
        assert_eq!(output.opaque_suffix().unwrap().as_ref(), [0xfe, 0xed]);
    }
}

#[test]
fn zero_count_limits_allow_only_the_literal_empty_structure() {
    let pool = Budget::new(100_000).unwrap();
    let none = HvcConfigLimits {
        maximum_arrays: 0,
        maximum_nals: 0,
        ..limits()
    };
    let output = run(
        &pool,
        Bytes::copy_from_slice(&pool, &prefix(4, 0)).unwrap(),
        none,
        1,
    )
    .unwrap();
    assert!(output.arrays().is_empty());
    let raw = record(&[(32, true, vec![])], &[]);
    assert!(matches!(
        run(&pool, Bytes::copy_from_slice(&pool, &raw).unwrap(), none, 1),
        Err(Error::Limit { .. })
    ));
    let arrays = HvcConfigLimits {
        maximum_nals: 0,
        ..limits()
    };
    assert!(
        run(
            &pool,
            Bytes::copy_from_slice(&pool, &raw).unwrap(),
            arrays,
            1
        )
        .unwrap()
        .nals()
        .is_empty()
    );
    assert!(matches!(
        run(
            &pool,
            Bytes::copy_from_slice(&pool, &example()).unwrap(),
            arrays,
            1
        ),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn impossible_required_live_set_is_limit_and_drops_private_input() {
    let raw = example();
    let pool = Budget::new(
        Budget::control_storage_bytes() + raw.len() + Bytes::owner_overhead_bytes() + 1,
    )
    .unwrap();
    let baseline = pool.used();
    let mut parser = HvcConfigParser::new(
        &pool,
        source(),
        Bytes::copy_from_slice(&pool, &raw).unwrap(),
        limits(),
    )
    .unwrap();
    let Err(error @ Error::Limit { .. }) =
        parser.step(&mut WorkBudget::new(1000), &AtomicBool::new(false))
    else {
        panic!("an intrinsically impossible live-set must not wait for caller release")
    };
    assert_eq!(pool.used(), baseline);
    assert!(
        matches!(parser.step(&mut WorkBudget::new(0), &AtomicBool::new(false)), Err(e) if e == error)
    );
}

#[test]
fn each_reservation_retry_is_stable_and_late_cancel_releases_all_scratch() {
    let pool = Budget::new(20_000).unwrap();
    let data = Bytes::copy_from_slice(&pool, &example()).unwrap();
    let baseline = pool.used();
    let mut retained = BudgetVec::<u8>::new(&pool);
    retained.try_reserve_exact(pool.available() - 1).unwrap();
    let mut parser = HvcConfigParser::new(&pool, source(), data.clone(), limits()).unwrap();
    let first = parser.step(&mut WorkBudget::new(1000), &AtomicBool::new(false));
    let Err(error @ Error::Backpressure { .. }) = first else {
        panic!("reservation not exercised")
    };
    let charged = pool.used();
    for _ in 0..4 {
        assert!(
            matches!(parser.step(&mut WorkBudget::new(1), &AtomicBool::new(false)), Err(e) if e == error)
        );
        assert_eq!(pool.used(), charged);
    }
    drop(retained);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Ok(BuildStep::Yield)
    ));
    assert!(
        pool.used() > baseline,
        "real array descriptors were reserved"
    );
    assert!(matches!(
        parser.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), baseline);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
}

#[test]
fn complete_output_is_single_publication_even_after_cancel_and_zero_work() {
    let pool = Budget::new(100_000).unwrap();
    let mut parser = HvcConfigParser::new(
        &pool,
        source(),
        Bytes::copy_from_slice(&pool, &example()).unwrap(),
        limits(),
    )
    .unwrap();
    let BuildStep::Ready(output) = parser
        .step(&mut WorkBudget::new(1000), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("small fully bounded configuration must finish")
    };
    for _ in 0..3 {
        assert!(matches!(
            parser.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
            Ok(BuildStep::Complete)
        ));
    }
    assert_eq!(output.source_bytes().as_ref(), example());
}
