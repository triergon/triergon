mod common;
use common::{child, roots};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BoxView, BuildStatus, Limits, Mov9RunKind, Mov9RunRecoveryBuilder, Mov9RunRecoveryTable,
    PinnedMovD32RegularRecoveryV1, SampleStep, SampleTableBuilder, TimingRunPolicy,
};
const STTS: &[u8] = include_bytes!("fixtures/bad-stts-count-mismatch.mov");
const CTTS: &[u8] = include_bytes!("fixtures/bad-ctts-count-mismatch.mov");
fn stbl(bytes: &[u8], pool: &Budget) -> BoxView {
    let top = roots(pool, bytes).unwrap();
    let moov = top.iter().find(|v| v.header().kind == *b"moov").unwrap();
    child(
        &child(
            &child(&child(moov, b"trak").unwrap(), b"mdia").unwrap(),
            b"minf",
        )
        .unwrap(),
        b"stbl",
    )
    .unwrap()
}
fn build(bytes: &[u8], pool: &Budget, quantum: u64) -> Result<Mov9RunRecoveryTable, Error> {
    let mut builder = Mov9RunRecoveryBuilder::new(
        stbl(bytes, pool),
        pool.clone(),
        Limits::default(),
        PinnedMovD32RegularRecoveryV1,
    )?;
    for _ in 0..100_000 {
        if builder.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))?
            == BuildStatus::Ready
        {
            return builder.finish();
        }
    }
    panic!("no bounded progress");
}
fn clock_prefix(table: &Mov9RunRecoveryTable) -> Vec<(u64, i128, i128)> {
    let mut cursor = table.source_prefix_cursor();
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(0)).unwrap(),
        SampleStep::Yield
    );
    let mut rows = Vec::new();
    loop {
        match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Sample(v) => {
                rows.push((v.ordinal, v.dts_ticks.unwrap(), v.cts_ticks.unwrap()))
            }
            SampleStep::Yield => {}
            SampleStep::End => break,
        }
    }
    rows
}
#[test]
fn strict_stts_and_ctts_errors_remain_exact() {
    let p = Budget::new(1 << 20).unwrap();
    for bytes in [STTS, CTTS] {
        assert_eq!(
            common::table(stbl(bytes, &p), &p, 1).unwrap_err(),
            Error::Invalid("time run exceeds sample count")
        );
    }
}
#[test]
fn bounded_timing_keeps_short_runs_and_invalid_sync_entries_strict() {
    let pool = Budget::new(1 << 20).unwrap();
    for (bytes, expected) in [
        (
            include_bytes!("fixtures/pinned-run-recovery/run57-04-r-front-stco-lastcount5.mov")
                .as_slice(),
            Error::Invalid("time table sample count mismatch"),
        ),
        (
            include_bytes!("fixtures/pinned-run-recovery/run57-05-r-front-stco-lastcount0.mov")
                .as_slice(),
            Error::Invalid("zero sample count in time run"),
        ),
        (
            include_bytes!("fixtures/bad-stss-out-of-range.mov").as_slice(),
            Error::Invalid("sync sample order/range"),
        ),
    ] {
        let mut builder = SampleTableBuilder::with_timing_policy(
            stbl(bytes, &pool),
            pool.clone(),
            Limits::default(),
            TimingRunPolicy::BoundedPrefix,
        )
        .unwrap();
        let error = (0..100_000)
            .find_map(|_| match builder.poll(&mut WorkBudget::new(1)) {
                Ok(BuildStatus::Yield) => None,
                Ok(BuildStatus::Ready) => panic!("invalid table admitted"),
                Err(error) => Some(error),
            })
            .expect("table validation stalled");
        assert_eq!(error, expected);
    }
}
#[test]
fn final_stts_positive_surplus_is_observed_not_rewritten() {
    let p = Budget::new(1 << 20).unwrap();
    for q in [1, 3, 4096] {
        let t = build(STTS, &p, q).unwrap();
        let d = t.coverage()[0].unwrap();
        assert_eq!(
            (
                d.kind,
                d.declared_samples,
                d.bounded_samples,
                d.surplus_samples
            ),
            (Mov9RunKind::Decode, 7, 6, 1)
        );
        assert_eq!(
            &STTS[d.count_field_start as usize..d.count_field_end as usize],
            &7u32.to_be_bytes()
        );
        assert_eq!(
            clock_prefix(&t),
            (0..6)
                .map(|i| (i + 1, i128::from(i) * 1000, i128::from(i) * 1000))
                .collect::<Vec<_>>()
        );
    }
}
#[test]
fn ctts_keeps_all_twelve_declared_occurrences_and_first_six_source_values() {
    let p = Budget::new(1 << 20).unwrap();
    let t = build(CTTS, &p, 1).unwrap();
    let d = t.coverage()[1].unwrap();
    assert_eq!(
        (
            d.kind,
            d.declared_samples,
            d.bounded_samples,
            d.surplus_samples
        ),
        (Mov9RunKind::Composition, 12, 6, 6)
    );
    assert_eq!(
        &CTTS[d.count_field_start as usize..d.count_field_end as usize],
        &7u32.to_be_bytes()
    );
    assert_eq!(
        clock_prefix(&t),
        (0..6)
            .map(|i| (i + 1, i128::from(i) * 1000, i128::from(i) * 1000 + 1))
            .collect::<Vec<_>>()
    );
}
#[test]
fn run_controls_cover_large_final_counts_and_vfr() {
    let p = Budget::new(1 << 20).unwrap();
    for bytes in [
        include_bytes!("fixtures/pinned-run-recovery/run57-00-r-front-stco-unchanged.mov")
            .as_slice(),
        include_bytes!("fixtures/pinned-run-recovery/run57-01-r-front-stco-lastcount7.mov"),
        include_bytes!("fixtures/pinned-run-recovery/run57-02-r-front-stco-lastcount8.mov"),
        include_bytes!("fixtures/pinned-run-recovery/run57-03-r-front-stco-lastcount12.mov"),
        include_bytes!("fixtures/pinned-run-recovery/run57-06-r-vfr-unchanged.mov"),
        include_bytes!("fixtures/pinned-run-recovery/run57-07-r-vfr-lastcount2.mov"),
        include_bytes!("fixtures/pinned-run-recovery/run57-08-r-vfr-lastcount8.mov"),
    ] {
        assert_eq!(clock_prefix(&build(bytes, &p, 1).unwrap()).len(), 6);
    }
}
#[test]
fn zero_and_short_run_counts_are_not_recovered() {
    let p = Budget::new(1 << 20).unwrap();
    for bytes in [
        include_bytes!("fixtures/pinned-run-recovery/run57-04-r-front-stco-lastcount5.mov")
            .as_slice(),
        include_bytes!("fixtures/pinned-run-recovery/run57-05-r-front-stco-lastcount0.mov"),
    ] {
        assert!(build(bytes, &p, 1).is_err());
    }
}
#[test]
fn ctts_unused_trailing_zero_is_still_rejected() {
    let mut bytes = CTTS.to_vec();
    let at = bytes.windows(4).position(|b| b == b"ctts").unwrap();
    bytes[at + 20..at + 24].copy_from_slice(&0u32.to_be_bytes());
    let p = Budget::new(1 << 20).unwrap();
    assert_eq!(
        build(&bytes, &p, 1).unwrap_err(),
        Error::Invalid("zero sample count in time run")
    );
}
#[test]
fn nonfinal_decode_surplus_is_not_recovered() {
    let mut bytes = include_bytes!("fixtures/r-vfr.mov").to_vec();
    let at = bytes.windows(4).position(|b| b == b"stts").unwrap();
    bytes[at + 12..at + 16].copy_from_slice(&7u32.to_be_bytes());
    let p = Budget::new(1 << 20).unwrap();
    assert_eq!(
        build(&bytes, &p, 1).unwrap_err(),
        Error::Invalid("time run exceeds sample count")
    );
}
#[test]
fn unrelated_description_and_zero_sync_values_stay_errors() {
    let p = Budget::new(1 << 20).unwrap();
    for bytes in [
        include_bytes!("fixtures/bad-stsc-description-zero.mov").as_slice(),
        include_bytes!("fixtures/pinned-field-recovery/stss-0-4.mov"),
    ] {
        assert!(build(bytes, &p, 1).is_err());
    }
}
#[test]
fn zero_work_yields_and_cancellation_is_sticky() {
    let p = Budget::new(1 << 20).unwrap();
    let mut b = Mov9RunRecoveryBuilder::new(
        stbl(STTS, &p),
        p.clone(),
        Limits::default(),
        PinnedMovD32RegularRecoveryV1,
    )
    .unwrap();
    let c = AtomicBool::new(false);
    assert_eq!(
        b.poll(&mut WorkBudget::new(0), &c).unwrap(),
        BuildStatus::Yield
    );
    c.store(true, Ordering::Relaxed);
    assert_eq!(b.poll(&mut WorkBudget::new(1), &c), Err(Error::Cancelled));
    c.store(false, Ordering::Relaxed);
    assert_eq!(b.poll(&mut WorkBudget::new(1), &c), Err(Error::Cancelled));
    assert!(matches!(b.finish(), Err(Error::Cancelled)));
}
#[test]
fn raw_owner_survives_input_drop_and_releases_to_initial_pool_control() {
    let p = Budget::new(1 << 20).unwrap();
    let initial = p.used();
    {
        let input = STTS.to_vec();
        let t = build(&input, &p, 1).unwrap();
        let raw = t.raw().clone();
        drop(input);
        drop(t);
        assert!(raw.bytes().belongs_to(&p));
        assert_eq!(raw.header().kind, *b"stbl");
        assert!(p.used() > initial);
    }
    assert_eq!(p.used(), initial);
}
#[test]
fn wrong_pool_and_record_limit_remain_enforced() {
    let p = Budget::new(1 << 20).unwrap();
    let other = Budget::new(1 << 20).unwrap();
    assert!(
        Mov9RunRecoveryBuilder::new(
            stbl(STTS, &p),
            other,
            Limits::default(),
            PinnedMovD32RegularRecoveryV1
        )
        .is_err()
    );
    let mut b = Mov9RunRecoveryBuilder::new(
        stbl(STTS, &p),
        p.clone(),
        Limits {
            max_records: 0,
            ..Limits::default()
        },
        PinnedMovD32RegularRecoveryV1,
    )
    .unwrap();
    assert!(
        b.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .is_err()
    );
}
#[test]
fn budget_backpressure_retains_builder_and_recovers_without_rewriting() {
    let p = Budget::new(1 << 20).unwrap();
    let raw = stbl(STTS, &p);
    let held = p.reserve(p.available()).unwrap();
    let mut b = Mov9RunRecoveryBuilder::new(
        raw,
        p.clone(),
        Limits::default(),
        PinnedMovD32RegularRecoveryV1,
    )
    .unwrap();
    assert!(matches!(
        b.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(held);
    while b
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != BuildStatus::Ready
    {}
    assert_eq!(
        b.finish().unwrap().coverage()[0].unwrap().declared_samples,
        7
    );
}
