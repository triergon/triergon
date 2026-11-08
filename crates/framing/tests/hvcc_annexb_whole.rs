#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    BuildStep, HvcAnnexBOutput, HvcAnnexBWriter, HvcConfigLimits, HvcConfigParser, HvcConfigSource,
    HvcConfiguration,
};
fn raw() -> Vec<u8> {
    vec![
        1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xf0, 0, 0xfc, 0xfc, 0xf8, 0xf8, 0, 0, 3, 3, 0xa0,
        0, 2, 0, 3, 0x40, 1, 0x81, 0, 3, 0x40, 1, 0x81, 55, 0, 1, 0, 4, 0x6e, 9, 0, 0xff, 0x61, 0,
        1, 0, 2, 0x44, 1, 0xa5, 0xff,
    ]
}
fn expected() -> Vec<u8> {
    vec![
        0, 0, 0, 1, 0x40, 1, 0x81, 0, 0, 0, 1, 0x40, 1, 0x81, 0, 0, 0, 1, 0x6e, 9, 0, 0xff, 0, 0,
        0, 1, 0x44, 1,
    ]
}
fn config(pool: &Budget, bytes: Bytes) -> HvcConfiguration {
    let mut p = HvcConfigParser::new(
        pool,
        HvcConfigSource {
            source: SourceId(13),
            generation: 27,
            offset: 71,
        },
        bytes,
        HvcConfigLimits {
            maximum_record_bytes: 1_000_000,
            maximum_arrays: 255,
            maximum_nals: 65535,
            maximum_nal_bytes: 65535,
        },
    )
    .unwrap();
    loop {
        match p
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(o) => return o,
            BuildStep::Yield => {}
            BuildStep::Complete => panic!(),
        }
    }
}
fn finish(w: &mut HvcAnnexBWriter, work: u64) -> Result<HvcAnnexBOutput, Error> {
    for _ in 0..10000 {
        match w.step(&mut WorkBudget::new(work), &AtomicBool::new(false))? {
            BuildStep::Ready(o) => return Ok(o),
            BuildStep::Yield => {}
            BuildStep::Complete => panic!(),
        }
    }
    panic!("bounded whole conversion did not finish")
}

#[test]
fn whole_ordered_conversion_preserves_every_array_duplicate_unknown_mismatch_and_suffix() {
    let pool = Budget::new(100_000).unwrap();
    let bytes = raw();
    for work in [1, 2, 7, 4096] {
        let c = config(&pool, Bytes::copy_from_slice(&pool, &bytes).unwrap());
        let mut w = HvcAnnexBWriter::new(&pool, c, 1000).unwrap();
        let o = finish(&mut w, work).unwrap();
        assert_eq!(o.bytes().as_ref(), expected());
        assert_eq!(o.configuration().source_bytes().as_ref(), bytes);
        assert_eq!(o.configuration().arrays().len(), 3);
        assert_eq!(o.configuration().nals().len(), 4);
        assert_eq!(o.configuration().opaque_suffix(), [0xa5, 0xff]);
        assert_ne!(o.configuration().syntax_issues() & 0xc0, 0);
        assert_eq!(o.configuration().source().offset, 71);
        assert!(matches!(
            w.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
            Ok(BuildStep::Complete)
        ));
    }
}

#[test]
fn zero_arrays_and_empty_array_remain_distinct_even_with_empty_annexb() {
    let pool = Budget::new(100_000).unwrap();
    for array in [false, true] {
        let mut bytes = raw()[..23].to_vec();
        bytes[22] = u8::from(array);
        if array {
            bytes.extend_from_slice(&[0xa0, 0, 0]);
        }
        let c = config(&pool, Bytes::copy_from_slice(&pool, &bytes).unwrap());
        let mut w = HvcAnnexBWriter::new(&pool, c, 0).unwrap();
        let o = finish(&mut w, 1).unwrap();
        assert!(o.bytes().is_empty());
        assert_eq!(o.configuration().arrays().len(), usize::from(array));
        assert_eq!(o.configuration().source_bytes().as_ref(), bytes);
    }
}

#[test]
fn output_bound_failure_cannot_truncate_the_final_nal() {
    let pool = Budget::new(100_000).unwrap();
    let c = config(&pool, Bytes::copy_from_slice(&pool, &raw()).unwrap());
    let mut w = HvcAnnexBWriter::new(&pool, c, expected().len() - 1).unwrap();
    assert!(matches!(finish(&mut w, 1), Err(Error::Limit { .. })));
    assert!(matches!(
        w.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn reserve_and_freeze_retries_do_not_repeat_complete_conversion() {
    let pool = Budget::new(10_000).unwrap();
    let c = config(&pool, Bytes::copy_from_slice(&pool, &raw()).unwrap());
    let mut retained = BudgetVec::<u8>::new(&pool);
    retained
        .try_reserve_exact(pool.available() - expected().len())
        .unwrap();
    let mut w = HvcAnnexBWriter::new(&pool, c, 1000).unwrap();
    assert!(matches!(
        finish(&mut w, 1000),
        Err(Error::Backpressure { .. })
    ));
    let used = pool.used();
    assert!(matches!(
        w.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(BuildStep::Yield)
    ));
    assert_eq!(pool.used(), used);
    assert!(matches!(
        w.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(pool.used(), used);
    drop(retained);
    let BuildStep::Ready(o) = w
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("copied NAL bytes must survive freeze retry")
    };
    assert_eq!(o.bytes().as_ref(), expected());
    assert_eq!(o.configuration().source_bytes().as_ref(), raw());
}

#[test]
fn every_selected_cancel_boundary_releases_descriptors_and_output_capacity() {
    for n in [0, 1, 3, 5, 8, 15, 30] {
        let pool = Budget::new(100_000).unwrap();
        let bytes = Bytes::copy_from_slice(&pool, &raw()).unwrap();
        let baseline = pool.used();
        let c = config(&pool, bytes.clone());
        let mut w = HvcAnnexBWriter::new(&pool, c, 1000).unwrap();
        assert!(matches!(
            w.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(BuildStep::Yield)
        ));
        for _ in 0..n {
            assert!(matches!(
                w.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
                Ok(BuildStep::Yield)
            ));
        }
        assert!(matches!(
            w.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert_eq!(pool.used(), baseline);
        assert!(matches!(
            w.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
            Err(Error::Cancelled)
        ));
    }
}

#[test]
fn intrinsically_impossible_retained_source_plus_output_is_limit() {
    let bytes = raw();
    // The record, its descriptor table and one byte: no room for the output.
    let roomy = Budget::new(100_000).unwrap();
    let table = config(&roomy, Bytes::copy_from_slice(&roomy, &bytes).unwrap())
        .descriptor_storage_bytes()
        .unwrap();
    let size =
        Budget::control_storage_bytes() + Bytes::owner_overhead_bytes() + bytes.len() + table + 1;
    let pool = Budget::new(size).unwrap();
    let c = config(&pool, Bytes::copy_from_slice(&pool, &bytes).unwrap());
    let mut w = HvcAnnexBWriter::new(&pool, c, 1000).unwrap();
    assert!(matches!(finish(&mut w, 1), Err(Error::Limit { .. })));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn a_foreign_pool_does_not_acquire_the_configuration() {
    let pool = Budget::new(100_000).unwrap();
    let other = Budget::new(100_000).unwrap();
    let c = config(&pool, Bytes::copy_from_slice(&pool, &raw()).unwrap());
    assert!(matches!(
        HvcAnnexBWriter::new(&other, c, 1000),
        Err(Error::ForeignPool)
    ));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn both_source_configuration_and_output_survive_the_job_then_release_last_owners() {
    let pool = Budget::new(100_000).unwrap();
    let baseline = pool.used();
    let c = config(&pool, Bytes::copy_from_slice(&pool, &raw()).unwrap());
    let mut w = HvcAnnexBWriter::new(&pool, c, 1000).unwrap();
    let o = finish(&mut w, 1).unwrap();
    drop(w);
    let source = o.configuration().source_bytes().clone();
    let result = o.bytes().clone();
    drop(o);
    assert_eq!(source.as_ref(), raw());
    assert_eq!(result.as_ref(), expected());
    std::thread::spawn(move || drop((source, result)))
        .join()
        .unwrap();
    assert_eq!(pool.used(), baseline);
}
