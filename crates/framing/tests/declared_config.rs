//! Original layout declarations, not ISO avcC/hvcC conformance vectors.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    BuildStep, Codec, ConfigInspection, DeclaredConfigArray, DeclaredConfigLimits,
    DeclaredConfigNal, DeclaredConfigParser, DeclaredConfiguration, LengthSize,
};

fn limits() -> DeclaredConfigLimits {
    DeclaredConfigLimits {
        maximum_record_bytes: 4096,
        maximum_arrays: 8,
        maximum_nals: 16,
        maximum_nal_bytes: 256,
    }
}

fn drive(
    parser: &mut DeclaredConfigParser<'_>,
    units: u64,
) -> Result<DeclaredConfiguration, Error> {
    let cancel = AtomicBool::new(false);
    for _ in 0..4096 {
        match parser.step(&mut WorkBudget::new(units), &cancel)? {
            BuildStep::Yield => {}
            BuildStep::Ready(value) => return Ok(value),
            BuildStep::Complete => panic!("missing configuration"),
        }
    }
    panic!("no bounded progress");
}

fn parse(
    pool: &Budget,
    codec: Codec,
    raw: &[u8],
    arrays: &[DeclaredConfigArray<'_>],
) -> Result<DeclaredConfiguration, Error> {
    let mut parser = DeclaredConfigParser::new(
        pool,
        codec,
        SourceId(91),
        Bytes::copy_from_slice(pool, raw)?,
        LengthSize::new(4)?,
        arrays,
        limits(),
    )?;
    drive(&mut parser, 1)
}

#[test]
fn declarations_preserve_record_array_order_and_unknown_types() {
    let pool = Budget::new(65536).unwrap();
    let first = [DeclaredConfigNal {
        length_field: 2..4,
        payload: 4..6,
    }];
    let second = [DeclaredConfigNal {
        length_field: 7..9,
        payload: 9..10,
    }];
    let arrays = [
        DeclaredConfigArray {
            raw_range: 1..6,
            nal_unit_type: 7,
            completeness: Some(false),
            nals: &first,
        },
        DeclaredConfigArray {
            raw_range: 6..10,
            nal_unit_type: 31,
            completeness: None,
            nals: &second,
        },
    ];
    let raw = [0xaa, 0xf1, 0, 2, 0x67, 0xe3, 0xbe, 0, 1, 0x1f, 0xcc];
    let value = parse(&pool, Codec::Avc, &raw, &arrays).unwrap();
    assert_eq!(value.source_bytes().as_ref(), raw);
    assert_eq!(value.source_id(), SourceId(91));
    assert_eq!(value.inspection(), ConfigInspection::DeclaredOpaque);
    assert_eq!(value.sample_length_size().bytes(), 4);
    assert_eq!(value.arrays().len(), 2);
    assert_eq!(value.arrays()[0].source_bytes().as_ref(), &raw[1..6]);
    assert_eq!(value.arrays()[0].declared_completeness(), Some(false));
    assert_eq!(value.arrays()[1].declared_nal_unit_type(), 31);
    assert_eq!(value.arrays()[0].nal_indices(), 0..1);
    assert_eq!(value.arrays()[1].nal_indices(), 1..2);
    assert_eq!(value.nals()[0].source_bytes().as_ref(), [0x67, 0xe3]);
    assert_eq!(value.nals()[1].source_bytes().as_ref(), [0x1f]);
    assert!(!value.nals()[0].array_type_mismatch());
    assert!(
        value.nals()[0]
            .source_bytes()
            .shares_storage_with(value.source_bytes())
    );
}

#[test]
fn all_declared_big_endian_widths_are_checked_without_an_iso_width_claim() {
    for width in 1..=4 {
        let pool = Budget::new(65536).unwrap();
        let mut raw = vec![0; width];
        raw[width - 1] = 2;
        raw.extend([0x68, 0x80]);
        let nals = [DeclaredConfigNal {
            length_field: 0..width,
            payload: width..width + 2,
        }];
        let arrays = [DeclaredConfigArray {
            raw_range: 0..raw.len(),
            nal_unit_type: 8,
            completeness: Some(true),
            nals: &nals,
        }];
        let output = parse(&pool, Codec::Avc, &raw, &arrays).unwrap();
        assert_eq!(
            output.nals()[0].length_field_bytes().as_ref(),
            &raw[..width]
        );
        assert_eq!(output.nals()[0].source_range(), width..width + 2);
    }
}

#[test]
fn hevc_header_and_declared_type_disagreement_remain_distinct() {
    let pool = Budget::new(65536).unwrap();
    let nals = [DeclaredConfigNal {
        length_field: 0..2,
        payload: 2..5,
    }];
    let arrays = [DeclaredConfigArray {
        raw_range: 0..5,
        nal_unit_type: 33,
        completeness: Some(true),
        nals: &nals,
    }];
    let output = parse(&pool, Codec::Hevc, &[0, 3, 0x40, 9, 0x80], &arrays).unwrap();
    assert_eq!(output.nals()[0].header().nal_unit_type(), 32);
    assert!(output.nals()[0].array_type_mismatch());
    assert_eq!(output.nals()[0].source_bytes().as_ref(), [0x40, 9, 0x80]);
}

#[test]
fn empty_arrays_and_opaque_suffix_are_preserved() {
    let pool = Budget::new(65536).unwrap();
    let arrays = [DeclaredConfigArray {
        raw_range: 1..2,
        nal_unit_type: 34,
        completeness: None,
        nals: &[],
    }];
    let output = parse(&pool, Codec::Hevc, &[0xaa, 0xbb, 0xcc], &arrays).unwrap();
    assert!(output.nals().is_empty());
    assert_eq!(output.arrays()[0].nal_indices(), 0..0);
    assert_eq!(output.source_bytes().as_ref(), [0xaa, 0xbb, 0xcc]);
}

#[test]
fn declared_length_mismatch_is_sticky_and_releases_owned_storage() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let nals = [DeclaredConfigNal {
        length_field: 0..2,
        payload: 2..3,
    }];
    let arrays = [DeclaredConfigArray {
        raw_range: 0..3,
        nal_unit_type: 7,
        completeness: None,
        nals: &nals,
    }];
    let mut parser = DeclaredConfigParser::new(
        &pool,
        Codec::Avc,
        SourceId(1),
        Bytes::copy_from_slice(&pool, &[0, 2, 0x67]).unwrap(),
        LengthSize::new(4).unwrap(),
        &arrays,
        limits(),
    )
    .unwrap();
    let error = drive(&mut parser, 1).err().unwrap();
    assert_eq!(error, Error::Invalid("declared NAL length mismatch"));
    assert_eq!(
        parser
            .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
            .err(),
        Some(error)
    );
    assert_eq!(pool.used(), baseline);
}

#[test]
fn malformed_ranges_never_index_outside_record() {
    let pool = Budget::new(65536).unwrap();
    for (field, payload) in [
        (0..0, 0..1),
        (0..5, 5..6),
        (0..1, 2..3),
        (std::ops::Range { start: 2, end: 1 }, 1..2),
        (0..1, 1..4),
        (0..1, 1..1),
    ] {
        let nals = [DeclaredConfigNal {
            length_field: field,
            payload,
        }];
        let arrays = [DeclaredConfigArray {
            raw_range: 0..3,
            nal_unit_type: 7,
            completeness: None,
            nals: &nals,
        }];
        assert!(parse(&pool, Codec::Avc, &[1, 0x67, 0], &arrays).is_err());
    }
}

#[test]
fn overlapping_arrays_and_nals_are_rejected() {
    let pool = Budget::new(65536).unwrap();
    let arrays = [
        DeclaredConfigArray {
            raw_range: 0..2,
            nal_unit_type: 7,
            completeness: None,
            nals: &[],
        },
        DeclaredConfigArray {
            raw_range: 1..3,
            nal_unit_type: 7,
            completeness: None,
            nals: &[],
        },
    ];
    assert!(parse(&pool, Codec::Avc, &[1, 0x67, 0], &arrays).is_err());
    let nals = [
        DeclaredConfigNal {
            length_field: 0..1,
            payload: 1..2,
        },
        DeclaredConfigNal {
            length_field: 0..1,
            payload: 1..2,
        },
    ];
    let arrays = [DeclaredConfigArray {
        raw_range: 0..2,
        nal_unit_type: 7,
        completeness: None,
        nals: &nals,
    }];
    assert!(parse(&pool, Codec::Avc, &[1, 0x67], &arrays).is_err());
}

#[test]
fn fixed_nal_header_errors_are_not_hidden_by_declarations() {
    let pool = Budget::new(65536).unwrap();
    let nals = [DeclaredConfigNal {
        length_field: 0..1,
        payload: 1..3,
    }];
    let arrays = [DeclaredConfigArray {
        raw_range: 0..3,
        nal_unit_type: 32,
        completeness: None,
        nals: &nals,
    }];
    for raw in [[2, 0xc0, 1], [2, 0x40, 0]] {
        assert!(parse(&pool, Codec::Hevc, &raw, &arrays).is_err());
    }
}

#[test]
fn zero_work_has_no_allocation_then_single_unit_work_finishes_once() {
    let pool = Budget::new(65536).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[1, 0x67]).unwrap();
    let before = pool.used();
    let nals = [DeclaredConfigNal {
        length_field: 0..1,
        payload: 1..2,
    }];
    let arrays = [DeclaredConfigArray {
        raw_range: 0..2,
        nal_unit_type: 7,
        completeness: None,
        nals: &nals,
    }];
    let mut parser = DeclaredConfigParser::new(
        &pool,
        Codec::Avc,
        SourceId(2),
        source,
        LengthSize::new(1).unwrap(),
        &arrays,
        limits(),
    )
    .unwrap();
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Yield
    ));
    assert_eq!(pool.used(), before);
    let output = drive(&mut parser, 1).unwrap();
    assert_eq!(output.nals().len(), 1);
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Complete
    ));
}

#[test]
fn backpressure_retries_without_losing_the_record() {
    let pool = Budget::new(65536).unwrap();
    let nals = [DeclaredConfigNal {
        length_field: 0..1,
        payload: 1..2,
    }];
    let arrays = [DeclaredConfigArray {
        raw_range: 0..2,
        nal_unit_type: 7,
        completeness: None,
        nals: &nals,
    }];
    let source = Bytes::copy_from_slice(&pool, &[1, 0x67]).unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut parser = DeclaredConfigParser::new(
        &pool,
        Codec::Avc,
        SourceId(2),
        source,
        LengthSize::new(1).unwrap(),
        &arrays,
        limits(),
    )
    .unwrap();
    assert!(matches!(
        drive(&mut parser, 1),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    assert_eq!(
        drive(&mut parser, 1).unwrap().source_bytes().as_ref(),
        [1, 0x67]
    );
}

#[test]
fn cancellation_is_sticky_and_releases_to_constructor_baseline() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let source = Bytes::copy_from_slice(&pool, &[1, 0x67]).unwrap();
    let nals = [DeclaredConfigNal {
        length_field: 0..1,
        payload: 1..2,
    }];
    let arrays = [DeclaredConfigArray {
        raw_range: 0..2,
        nal_unit_type: 7,
        completeness: None,
        nals: &nals,
    }];
    let mut parser = DeclaredConfigParser::new(
        &pool,
        Codec::Avc,
        SourceId(3),
        source,
        LengthSize::new(4).unwrap(),
        &arrays,
        limits(),
    )
    .unwrap();
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(6), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Yield
    ));
    assert!(pool.used() > baseline);
    let cancel = AtomicBool::new(true);
    assert_eq!(
        parser.step(&mut WorkBudget::new(1), &cancel).err(),
        Some(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        parser.step(&mut WorkBudget::new(1), &cancel).err(),
        Some(Error::Cancelled)
    );
    assert_eq!(pool.used(), baseline);
}

#[test]
fn retained_nal_owns_source_after_all_declarations_and_parser_are_dropped() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let retained = {
        let nals = [DeclaredConfigNal {
            length_field: 1..2,
            payload: 2..3,
        }];
        let arrays = [DeclaredConfigArray {
            raw_range: 1..3,
            nal_unit_type: 7,
            completeness: None,
            nals: &nals,
        }];
        let result = parse(&pool, Codec::Avc, &[0xee, 1, 0x67, 0xdd], &arrays).unwrap();
        result.nals()[0].source_bytes().clone()
    };
    assert_eq!(retained.as_ref(), [0x67]);
    assert!(pool.used() > baseline);
    drop(retained);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn foreign_pool_and_all_descriptor_policy_limits_are_explicit() {
    let pool = Budget::new(65536).unwrap();
    let foreign = Budget::new(65536).unwrap();
    assert!(matches!(
        DeclaredConfigParser::new(
            &pool,
            Codec::Avc,
            SourceId(1),
            Bytes::copy_from_slice(&foreign, &[1]).unwrap(),
            LengthSize::new(4).unwrap(),
            &[],
            limits()
        ),
        Err(Error::ForeignPool)
    ));
    let nals = [DeclaredConfigNal {
        length_field: 0..1,
        payload: 1..3,
    }];
    let arrays = [DeclaredConfigArray {
        raw_range: 0..3,
        nal_unit_type: 7,
        completeness: None,
        nals: &nals,
    }];
    for restricted in [
        DeclaredConfigLimits {
            maximum_record_bytes: 2,
            ..limits()
        },
        DeclaredConfigLimits {
            maximum_arrays: 0,
            ..limits()
        },
        DeclaredConfigLimits {
            maximum_nals: 0,
            ..limits()
        },
        DeclaredConfigLimits {
            maximum_nal_bytes: 1,
            ..limits()
        },
    ] {
        let result = DeclaredConfigParser::new(
            &pool,
            Codec::Avc,
            SourceId(1),
            Bytes::copy_from_slice(&pool, &[2, 0x67, 0x80]).unwrap(),
            LengthSize::new(4).unwrap(),
            &arrays,
            restricted,
        );
        match result {
            Err(_) => {}
            Ok(mut parser) => assert!(drive(&mut parser, 1).is_err()),
        }
    }
}

#[test]
fn duplicate_nal_bytes_are_retained_in_physical_order_for_every_work_quantum() {
    let pool = Budget::new(65536).unwrap();
    let nals = [
        DeclaredConfigNal {
            length_field: 0..1,
            payload: 1..3,
        },
        DeclaredConfigNal {
            length_field: 3..4,
            payload: 4..6,
        },
    ];
    let arrays = [DeclaredConfigArray {
        raw_range: 0..6,
        nal_unit_type: 7,
        completeness: None,
        nals: &nals,
    }];
    for units in [1, 2, 3, 7, 64] {
        let raw = Bytes::copy_from_slice(&pool, &[2, 0x67, 0x80, 2, 0x67, 0x80]).unwrap();
        let mut parser = DeclaredConfigParser::new(
            &pool,
            Codec::Avc,
            SourceId(6),
            raw,
            LengthSize::new(4).unwrap(),
            &arrays,
            limits(),
        )
        .unwrap();
        let value = drive(&mut parser, units).unwrap();
        assert_eq!(value.nals().len(), 2);
        assert_eq!(value.nals()[0].source_bytes().as_ref(), [0x67, 0x80]);
        assert_eq!(value.nals()[1].source_bytes().as_ref(), [0x67, 0x80]);
        assert_eq!(value.nals()[0].source_range(), 1..3);
        assert_eq!(value.nals()[1].source_range(), 4..6);
    }
}
