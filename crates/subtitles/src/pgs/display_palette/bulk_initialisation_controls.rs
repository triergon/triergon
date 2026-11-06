//! Funded bulk initialisation, retry and retained-source regressions.
use super::*;
use crate::pgs::{Envelope, PacketClock};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{TimeBase, WorkBudget};

fn job(
    budget: &Budget,
    entries: &[[u8; 5]],
    previous: Option<PaletteSnapshot>,
    preserve: bool,
    sequence: u64,
) -> PaletteBuild {
    let mut payload = vec![3, sequence as u8];
    for entry in entries {
        payload.extend_from_slice(entry);
    }
    PaletteBuild::new(
        budget,
        Bytes::copy_from_slice(budget, &payload).unwrap(),
        sequence + 7,
        SegmentStamp {
            packet_sequence: sequence,
            packet_clock: PacketClock {
                pts: Some(sequence as i64 * 900),
                dts: None,
                timebase: TimeBase::new(1, 90_000).unwrap(),
            },
            sup_clock: None,
            packet_offset: 13,
            envelope: Envelope::Packet,
        },
        ColourMatrix::for_canvas(if sequence == 0 { 480 } else { 1080 }),
        previous,
        preserve,
    )
    .unwrap()
}

fn step(
    job: &mut PaletteBuild,
    spent: &mut usize,
    units: u64,
    maximum: usize,
    cancel: &AtomicBool,
) -> Result<bool> {
    let mut caller = WorkBudget::new(units);
    let before = *spent;
    let result = job.step(&mut DisplayWork {
        caller: &mut caller,
        spent: &mut *spent,
        maximum,
        cancel,
    });
    assert_eq!(*spent - before, usize::try_from(caller.spent()).unwrap());
    result
}

fn finish(mut job: PaletteBuild, mut spent: usize, quantum: u64) -> (PaletteSnapshot, usize) {
    let cancel = AtomicBool::new(false);
    for _ in 0..20_000 {
        if step(&mut job, &mut spent, quantum, 1_000_000, &cancel).unwrap() {
            return (job.take().unwrap(), spent);
        }
    }
    panic!("bounded bulk palette progress");
}

fn reserve_columns(job: &mut PaletteBuild, spent: &mut usize, cancel: &AtomicBool) {
    assert!(!step(job, spent, 4, 1_000_000, cancel).unwrap());
    assert!(matches!(job.phase, Phase::Initialise));
    assert_eq!(*spent, 4);
    assert_eq!(job.credit, 0);
    assert!(job.words.capacity() >= 256 && job.origins.capacity() >= 256);
}

fn no_cells(job: &PaletteBuild) {
    assert_eq!(
        (job.colours.len(), job.words.len(), job.origins.len()),
        (0, 0, 0)
    );
    assert!(job.first_owner.is_none());
    assert!(job.other_owners.is_empty());
}

#[test]
fn fresh_and_source_palettes_keep_exact_work_for_small_and_large_quanta() {
    for quantum in [1, 2, 3, 7, 63, 64, 255, 256, 4096] {
        let budget = Budget::new(65536).unwrap();
        let baseline = budget.used();
        let (empty, empty_work) = finish(job(&budget, &[], None, false, 0), 0, quantum);
        assert_eq!(empty_work, 4 + 256 + 1);
        assert_eq!(empty.receiver_words(), &[0; 256]);
        for index in 0..=255 {
            assert_eq!(empty.source(index), None);
            assert_eq!(
                empty.receiver_origin(index),
                ColourOrigin::ReceiverZeroInitialization
            );
        }
        drop(empty);
        let (source, source_work) = finish(
            job(&budget, &[[7, 16, 128, 128, 255]], None, false, 0),
            0,
            quantum,
        );
        assert_eq!(source_work, 4 + 256 + 8 + 1);
        assert_eq!(source.source(7).unwrap().palette_version, 0);
        assert_eq!(source.receiver_origin(7), ColourOrigin::ExplicitSource);
        drop(source);
        assert_eq!(budget.used(), baseline);
    }
}

#[test]
fn no_column_is_written_until_all256_cell_units_are_funded() {
    let budget = Budget::new(65536).unwrap();
    let mut job = job(&budget, &[[7, 16, 128, 128, 255]], None, false, 0);
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    reserve_columns(&mut job, &mut spent, &cancel);
    let reserved = budget.used();
    assert!(!step(&mut job, &mut spent, 255, 10000, &cancel).unwrap());
    no_cells(&job);
    assert_eq!((spent, job.credit, budget.used()), (259, 255, reserved));
    assert!(!step(&mut job, &mut spent, 0, 10000, &cancel).unwrap());
    no_cells(&job);
    assert_eq!((spent, job.credit, budget.used()), (259, 255, reserved));
    assert!(!step(&mut job, &mut spent, 1, 10000, &cancel).unwrap());
    assert_eq!(
        (job.colours.len(), job.words.len(), job.origins.len()),
        (256, 256, 256)
    );
    assert_eq!((spent, budget.used()), (260, reserved));
    assert!(matches!(job.phase, Phase::Provenance));
    assert!(matches!(
        job.take(),
        Err(Error::Invalid("PGS palette not ready"))
    ));
}

#[test]
fn pressure_before_columns_retains_paid_allocation_and_zero_work_stays_inert() {
    let budget = Budget::new(65536).unwrap();
    let mut job = job(&budget, &[[7, 16, 128, 128, 255]], None, false, 0);
    let pressure = budget.reserve(budget.available()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    assert!(matches!(
        step(&mut job, &mut spent, 1, 10000, &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!((spent, job.credit, job.colours.capacity()), (1, 1, 0));
    drop(pressure);
    let before = budget.used();
    assert!(!step(&mut job, &mut spent, 0, 10000, &cancel).unwrap());
    assert_eq!((spent, job.credit, budget.used()), (1, 1, before));
    assert_eq!(job.colours.capacity(), 0);
    assert!(!step(&mut job, &mut spent, 3, 10000, &cancel).unwrap());
    assert_eq!(spent, 4);
    assert!(matches!(job.phase, Phase::Initialise));
    no_cells(&job);
    let (result, total) = finish(job, spent, 7);
    assert_eq!(total, 269);
    assert_eq!(result.source(7).unwrap().y, 16);
}

#[test]
fn fully_funded_copy_needs_no_pool_room_and_owner_pressure_keeps_credit() {
    let budget = Budget::new(65536).unwrap();
    let baseline = budget.used();
    let mut job = job(&budget, &[], None, false, 0);
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    reserve_columns(&mut job, &mut spent, &cancel);
    let pressure = budget.reserve(budget.available()).unwrap();
    let full = budget.used();
    assert!(!step(&mut job, &mut spent, 256, 10000, &cancel).unwrap());
    assert_eq!(
        (job.words.len(), job.origins.len(), budget.used()),
        (256, 256, full)
    );
    assert!(job.colours.is_empty());
    assert!(matches!(job.phase, Phase::Owner));
    assert!(matches!(
        step(&mut job, &mut spent, 1, 10000, &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!((spent, job.credit), (261, 1));
    drop(pressure);
    let before = budget.used();
    assert!(!step(&mut job, &mut spent, 0, 10000, &cancel).unwrap());
    assert_eq!((spent, job.credit, budget.used()), (261, 1, before));
    assert!(step(&mut job, &mut spent, 1, 10000, &cancel).unwrap());
    assert_eq!(spent, 261);
    drop(job.take().unwrap());
    drop(job);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn cancellation_after_partial_funding_precedes_zero_work_and_keeps_held_source() {
    let budget = Budget::new(65536).unwrap();
    let (held, _) = finish(
        job(&budget, &[[7, 16, 128, 128, 255]], None, false, 0),
        0,
        4096,
    );
    let original = held.source(7);
    let mut job = job(&budget, &[], Some(held.clone()), true, 1);
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    reserve_columns(&mut job, &mut spent, &cancel);
    assert!(!step(&mut job, &mut spent, 127, 10000, &cancel).unwrap());
    let before = budget.used();
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        step(&mut job, &mut spent, 0, 10000, &cancel),
        Err(Error::Cancelled)
    );
    no_cells(&job);
    assert_eq!((spent, job.credit, budget.used()), (131, 127, before));
    assert_eq!(held.source(7), original);
    assert_eq!(Arc::strong_count(held.0.source_provenance(7).unwrap()), 1);
}

#[test]
fn exact_work_limit_keeps_partial_credit_and_never_commits_unfunded_cells() {
    let budget = Budget::new(65536).unwrap();
    let mut job = job(&budget, &[], None, false, 0);
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    reserve_columns(&mut job, &mut spent, &cancel);
    assert!(!step(&mut job, &mut spent, 255, 259, &cancel).unwrap());
    assert_eq!(
        step(&mut job, &mut spent, 1, 259, &cancel),
        Err(Error::Limit {
            requested: 260,
            maximum: 259
        })
    );
    no_cells(&job);
    assert_eq!((spent, job.credit), (259, 255));
    assert!(!step(&mut job, &mut spent, 1, 260, &cancel).unwrap());
    assert_eq!((spent, job.words.len()), (260, 256));
    assert_eq!(
        step(&mut job, &mut spent, 1, 260, &cancel),
        Err(Error::Limit {
            requested: 261,
            maximum: 260
        })
    );
    assert!(step(&mut job, &mut spent, 1, 261, &cancel).unwrap());
    assert_eq!(spent, 261);
}

#[test]
fn partial_and_epoch_carries_keep_all_literal_source_and_receiver_facts() {
    let budget = Budget::new(65536).unwrap();
    let (held, _) = finish(
        job(
            &budget,
            &[[7, 16, 128, 128, 255], [8, 235, 93, 171, 0]],
            None,
            false,
            0,
        ),
        0,
        4096,
    );
    for quantum in [1, 3, 7, 64, 255, 4096] {
        for preserve in [false, true] {
            let (copy, work) = finish(
                job(&budget, &[], Some(held.clone()), preserve, 1),
                0,
                quantum,
            );
            assert_eq!(work, 4 + 256 + 2);
            assert_eq!(copy.receiver_words(), held.receiver_words());
            for index in 0..=255 {
                assert_eq!(
                    copy.source(index),
                    if preserve { held.source(index) } else { None }
                );
                let expected = if preserve {
                    held.receiver_origin(index)
                } else if held.receiver_origin(index) == ColourOrigin::ReceiverZeroInitialization {
                    ColourOrigin::ReceiverZeroInitialization
                } else {
                    ColourOrigin::PreviousEpochSlot
                };
                assert_eq!(copy.receiver_origin(index), expected);
            }
        }
    }
}

#[test]
fn distinct_owners_are_cloned_once_only_after_all_cells_are_funded() {
    let budget = Budget::new(65536).unwrap();
    let (a, _) = finish(
        job(
            &budget,
            &[[7, 16, 128, 128, 255], [8, 31, 93, 171, 7]],
            None,
            false,
            0,
        ),
        0,
        4096,
    );
    let (b, _) = finish(
        job(&budget, &[[7, 235, 128, 128, 0]], Some(a.clone()), true, 1),
        0,
        7,
    );
    let counts = [7, 8].map(|index| Arc::strong_count(b.0.source_provenance(index).unwrap()));
    let mut job = job(&budget, &[], Some(b.clone()), true, 2);
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    reserve_columns(&mut job, &mut spent, &cancel);
    assert!(!step(&mut job, &mut spent, 255, 10000, &cancel).unwrap());
    no_cells(&job);
    for (index, count) in [7, 8].into_iter().zip(counts) {
        assert_eq!(
            Arc::strong_count(b.0.source_provenance(index).unwrap()),
            count
        );
    }
    assert!(!step(&mut job, &mut spent, 1, 10000, &cancel).unwrap());
    assert_eq!(job.other_owners.len(), b.0.other_owners.len());
    for (index, count) in [7, 8].into_iter().zip(counts) {
        assert_eq!(
            Arc::strong_count(b.0.source_provenance(index).unwrap()),
            count + 1
        );
    }
    let (copy, total) = finish(job, spent, 3);
    assert_eq!(total, 4 + 256 + 3);
    assert_eq!(copy.source(7), b.source(7));
    assert_eq!(copy.source(8), b.source(8));
}

#[test]
fn dropping_partial_bulk_job_releases_only_private_capacity_and_keeps_snapshot() {
    let budget = Budget::new(65536).unwrap();
    let baseline = budget.used();
    let (held, _) = finish(
        job(&budget, &[[7, 16, 128, 128, 255]], None, false, 0),
        0,
        4096,
    );
    let held_used = budget.used();
    let original = held.source(7);
    let mut job = job(&budget, &[], Some(held.clone()), true, 1);
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    reserve_columns(&mut job, &mut spent, &cancel);
    assert!(!step(&mut job, &mut spent, 255, 10000, &cancel).unwrap());
    drop(job);
    assert_eq!(budget.used(), held_used);
    assert_eq!(held.source(7), original);
    drop(held);
    assert_eq!(budget.used(), baseline);
}
