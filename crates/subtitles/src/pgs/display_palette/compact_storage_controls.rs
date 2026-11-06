//! Independent bounded-storage regressions. These use the real builder and pool.
use super::*;
use crate::pgs::{Envelope, PacketClock};
use std::sync::atomic::AtomicBool;
use triergon_core::{TimeBase, WorkBudget};

fn stamp(sequence: u64) -> SegmentStamp {
    SegmentStamp {
        packet_sequence: sequence,
        packet_clock: PacketClock {
            pts: Some(-900 + sequence as i64),
            dts: None,
            timebase: TimeBase::new(1, 90_000).unwrap(),
        },
        sup_clock: None,
        packet_offset: 17,
        envelope: Envelope::Packet,
    }
}

fn finish(
    budget: &Budget,
    entries: &[[u8; 5]],
    previous: Option<PaletteSnapshot>,
    preserve: bool,
    sequence: u64,
    quantum: u64,
) -> Result<PaletteSnapshot> {
    let mut bytes = vec![3, sequence as u8];
    for entry in entries {
        bytes.extend_from_slice(entry);
    }
    let mut job = PaletteBuild::new(
        budget,
        Bytes::copy_from_slice(budget, &bytes)?,
        sequence,
        stamp(sequence),
        ColourMatrix::for_canvas(if sequence.is_multiple_of(2) {
            480
        } else {
            1080
        }),
        previous,
        preserve,
    )?;
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    for _ in 0..20_000 {
        if job.step(&mut DisplayWork {
            caller: &mut WorkBudget::new(quantum),
            spent: &mut spent,
            maximum: 1_000_000,
            cancel: &cancel,
        })? {
            return job.take();
        }
    }
    Err(Error::Invalid("compact palette progress bound"))
}

#[test]
fn empty_palette_fits_pool_for_words_without_absent_source_storage() {
    let budget = Budget::new(2048).unwrap();
    let baseline = budget.used();
    let palette = finish(&budget, &[], None, false, 0, 1).unwrap();
    for index in 0..=255 {
        assert_eq!(palette.source(index), None);
        assert_eq!(palette.receiver_argb(index), 0);
        assert_eq!(
            palette.receiver_origin(index),
            ColourOrigin::ReceiverZeroInitialization
        );
    }
    drop(palette);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn sparse_palette_uses_tight_pool_without_allocating_256_source_owners() {
    let budget = Budget::new(4096).unwrap();
    let baseline = budget.used();
    let palette = finish(
        &budget,
        &[
            [0, 16, 128, 128, 255],
            [63, 31, 91, 171, 7],
            [255, 235, 128, 128, 0],
        ],
        None,
        false,
        1,
        1,
    )
    .unwrap();
    for index in 0..=255 {
        assert_eq!(
            palette.source(index).is_some(),
            [0, 63, 255].contains(&index)
        );
    }
    assert_eq!(palette.source(63).unwrap().stamp, stamp(1));
    drop(palette);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn full256_palette_fits_bounded_pool_and_keeps_every_literal_entry() {
    let budget = Budget::new(6144).unwrap();
    let baseline = budget.used();
    let entries: Vec<_> = (0..=255u8).map(|i| [i, i, i ^ 0x5a, i ^ 0xa5, i]).collect();
    let palette = finish(&budget, &entries, None, false, 0, 7).unwrap();
    for index in 0..=255 {
        let source = palette.source(index).unwrap();
        assert_eq!(
            [source.y, source.cr, source.cb, source.alpha],
            entries[index as usize][1..]
        );
        assert_eq!(source.stamp, stamp(0));
        assert_eq!(palette.receiver_origin(index), ColourOrigin::ExplicitSource);
        assert_eq!(
            palette.receiver_argb(index),
            ColourMatrix::for_canvas(480).argb(index, index ^ 0x5a, index ^ 0xa5, index)
        );
    }
    drop(palette);
    assert_eq!(budget.used(), baseline);
}

fn owner_invariants(palette: &PaletteSnapshot) {
    let mut counts = [0u16; 256];
    for cell in palette.0.colours.as_slice() {
        if let Some(id) = cell.provenance {
            assert!(id.get() <= 256);
            counts[usize::from(id.get() - 1)] += 1;
        }
    }
    assert!(palette.0.other_owners.len() <= 255);
    for (slot, count) in counts.into_iter().enumerate() {
        let owner = if slot == 0 {
            palette.0.first_owner.as_ref()
        } else {
            palette
                .0
                .other_owners
                .as_slice()
                .get(slot - 1)
                .and_then(Option::as_ref)
        };
        assert_eq!(owner.map_or(0, |o| o.entries), count);
        assert_eq!(owner.is_some(), count != 0);
    }
}

#[test]
fn compact_cells_have_no_per_colour_destructor_or_shared_owner() {
    assert!(!std::mem::needs_drop::<StoredSourceColour>());
    assert!(size_of::<StoredSourceColour>() <= 8);
}

#[test]
fn full_palette_retains_one_actual_provenance_owner() {
    let budget = Budget::new(1 << 20).unwrap();
    let entries: Vec<_> = (0..=255).map(|i| [i, i, 128, 128, i]).collect();
    let palette = finish(&budget, &entries, None, false, 0, 4096).unwrap();
    assert_eq!(
        Arc::strong_count(palette.0.source_provenance(0).unwrap()),
        1
    );
    assert!(palette.0.other_owners.is_empty());
    assert_eq!(palette.0.first_owner.as_ref().unwrap().entries, 256);
    owner_invariants(&palette);
}

#[test]
fn empty_palette_has_no_source_column_or_provenance_spill_capacity() {
    let budget = Budget::new(1 << 20).unwrap();
    let palette = finish(&budget, &[], None, false, 0, 1).unwrap();
    assert_eq!(palette.0.colours.capacity(), 0);
    assert_eq!(palette.0.other_owners.capacity(), 0);
    assert!(palette.0.first_owner.is_none());
    assert_eq!(palette.receiver_words().len(), 256);
    owner_invariants(&palette);
}

#[test]
fn partial_carry_clones_distinct_owner_once_and_preserves_clock_and_matrix() {
    let budget = Budget::new(1 << 20).unwrap();
    let entries: Vec<_> = (0..=255).map(|i| [i, i, 128, 128, i]).collect();
    let a = finish(&budget, &entries, None, false, 0, 7).unwrap();
    let b = finish(
        &budget,
        &[[7, 235, 93, 171, 0]],
        Some(a.clone()),
        true,
        1,
        1,
    )
    .unwrap();
    assert!(Arc::ptr_eq(
        a.0.source_provenance(8).unwrap(),
        b.0.source_provenance(8).unwrap()
    ));
    assert_eq!(Arc::strong_count(a.0.source_provenance(8).unwrap()), 2);
    for i in 0..=255 {
        if i != 7 {
            assert_eq!(b.source(i), a.source(i));
        }
    }
    assert_eq!(b.source(7).unwrap().stamp, stamp(1));
    assert_eq!(
        b.source(7).unwrap().receiver_matrix,
        ColourMatrix::for_canvas(1080)
    );
    assert_eq!(
        b.source(8).unwrap().receiver_matrix,
        ColourMatrix::for_canvas(480)
    );
    owner_invariants(&a);
    owner_invariants(&b);
}

#[test]
fn duplicate_records_retain_last_literal_components_without_owner_fanout() {
    let budget = Budget::new(1 << 20).unwrap();
    let entries: Vec<_> = (0..=255)
        .map(|i| [17, i, i ^ 0x55, i ^ 0xaa, 255 - i])
        .collect();
    let palette = finish(&budget, &entries, None, false, 2, 3).unwrap();
    let source = palette.source(17).unwrap();
    assert_eq!(
        [source.y, source.cr, source.cb, source.alpha],
        [255, 170, 85, 0]
    );
    assert_eq!(palette.0.first_owner.as_ref().unwrap().entries, 1);
    assert_eq!(
        Arc::strong_count(palette.0.source_provenance(17).unwrap()),
        1
    );
    owner_invariants(&palette);
}

#[test]
fn every_owner_index_including_256_is_live_and_reusable() {
    let budget = Budget::new(1 << 20).unwrap();
    let mut palette = finish(&budget, &[], None, false, 0, 4096).unwrap();
    for i in 0..=255u8 {
        palette = finish(
            &budget,
            &[[i, i, 128, 128, i]],
            Some(palette),
            true,
            u64::from(i) + 1,
            4096,
        )
        .unwrap();
        owner_invariants(&palette);
    }
    assert_eq!(palette.0.other_owners.len(), 255);
    assert_eq!(
        palette.0.colours.as_slice()[255].provenance.unwrap().get(),
        256
    );
    let held = palette.clone();
    palette = finish(
        &budget,
        &[[255, 16, 93, 171, 7]],
        Some(palette),
        true,
        300,
        1,
    )
    .unwrap();
    assert_eq!(palette.0.other_owners.len(), 255);
    assert_eq!(palette.source(255).unwrap().stamp, stamp(300));
    assert_eq!(held.source(255).unwrap().stamp, stamp(256));
    owner_invariants(&held);
    owner_invariants(&palette);
}

#[test]
fn full_replacement_releases_unused_live_owners_without_losing_held_generation() {
    let budget = Budget::new(1 << 20).unwrap();
    let baseline = budget.used();
    let mut palette = finish(&budget, &[], None, false, 0, 4096).unwrap();
    for i in 0..=255u8 {
        palette = finish(
            &budget,
            &[[i, i, 128, 128, i]],
            Some(palette),
            true,
            u64::from(i) + 1,
            4096,
        )
        .unwrap();
    }
    let held = palette.clone();
    let entries: Vec<_> = (0..=255).rev().map(|i| [i, 235, 93, 171, i]).collect();
    let b = finish(&budget, &entries, Some(palette), true, 900, 1).unwrap();
    owner_invariants(&b);
    assert_eq!(
        usize::from(b.0.first_owner.is_some())
            + b.0
                .other_owners
                .as_slice()
                .iter()
                .filter(|p| p.is_some())
                .count(),
        1
    );
    for i in 0..=255 {
        assert_eq!(b.source(i).unwrap().stamp, stamp(900));
        assert_eq!(held.source(i).unwrap().stamp, stamp(u64::from(i) + 1));
    }
    drop(held);
    assert_eq!(Arc::strong_count(b.0.source_provenance(0).unwrap()), 1);
    drop(b);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn epoch_carry_clears_source_and_owners_but_preserves_receiver_words_and_origin() {
    let budget = Budget::new(1 << 20).unwrap();
    let a = finish(&budget, &[[63, 235, 93, 171, 7]], None, false, 0, 1).unwrap();
    let b = finish(&budget, &[], Some(a.clone()), false, 1, 1).unwrap();
    assert_eq!(b.0.colours.capacity(), 0);
    assert!(b.0.first_owner.is_none());
    for i in 0..=255 {
        assert_eq!(b.source(i), None);
        assert_eq!(b.receiver_argb(i), a.receiver_argb(i));
        assert_eq!(
            b.receiver_origin(i),
            if i == 63 {
                ColourOrigin::PreviousEpochSlot
            } else {
                ColourOrigin::ReceiverZeroInitialization
            }
        );
    }
    owner_invariants(&b);
}

#[test]
fn carried_receiver_word_does_not_become_source_when_one_new_entry_is_written() {
    let budget = Budget::new(1 << 20).unwrap();
    let a = finish(&budget, &[[63, 235, 93, 171, 7]], None, false, 0, 1).unwrap();
    let b = finish(
        &budget,
        &[[64, 16, 128, 128, 255]],
        Some(a.clone()),
        false,
        1,
        1,
    )
    .unwrap();
    assert_eq!(b.source(63), None);
    assert_eq!(b.receiver_argb(63), a.receiver_argb(63));
    assert_eq!(b.receiver_origin(63), ColourOrigin::PreviousEpochSlot);
    assert_eq!(b.source(64).unwrap().stamp, stamp(1));
    owner_invariants(&b);
}

#[test]
fn empty_partial_update_keeps_owners_shared_without_new_provenance() {
    let budget = Budget::new(1 << 20).unwrap();
    let a = finish(&budget, &[[7, 16, 128, 128, 255]], None, false, 0, 1).unwrap();
    let b = finish(&budget, &[], Some(a.clone()), true, 1, 1).unwrap();
    assert_eq!(b.source(7), a.source(7));
    assert!(Arc::ptr_eq(
        a.0.source_provenance(7).unwrap(),
        b.0.source_provenance(7).unwrap()
    ));
    assert_eq!(Arc::strong_count(a.0.source_provenance(7).unwrap()), 2);
    owner_invariants(&b);
}

#[test]
fn all_small_quanta_and_bulk_boundary_quanta_keep_full_literal_palette() {
    let budget = Budget::new(1 << 20).unwrap();
    let entries: Vec<_> = (0..=255)
        .map(|i| [i, i, i ^ 31, i ^ 127, 255 - i])
        .collect();
    let expected = finish(&budget, &entries, None, false, 7, 4096).unwrap();
    for q in [1, 2, 3, 7, 31, 63, 64, 65, 255, 256, 257] {
        let p = finish(&budget, &entries, None, false, 7, q).unwrap();
        for i in 0..=255 {
            assert_eq!(p.source(i), expected.source(i));
            assert_eq!(p.receiver_argb(i), expected.receiver_argb(i));
        }
        owner_invariants(&p);
    }
}

fn one_entry_job(budget: &Budget) -> PaletteBuild {
    PaletteBuild::new(
        budget,
        Bytes::copy_from_slice(budget, &[3, 1, 7, 16, 128, 128, 255]).unwrap(),
        1,
        stamp(1),
        ColourMatrix::for_canvas(480),
        None,
        false,
    )
    .unwrap()
}

#[test]
fn funded_pressure_retry_with_zero_work_allocates_nothing_and_keeps_state() {
    let budget = Budget::new(65536).unwrap();
    let mut job = one_entry_job(&budget);
    let pressure = budget.reserve(budget.available()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    assert!(matches!(
        job.step(&mut DisplayWork {
            caller: &mut WorkBudget::new(1),
            spent: &mut spent,
            maximum: 10000,
            cancel: &cancel
        }),
        Err(Error::Backpressure { .. })
    ));
    let paid = spent;
    drop(pressure);
    let before = budget.used();
    assert!(
        !job.step(&mut DisplayWork {
            caller: &mut WorkBudget::new(0),
            spent: &mut spent,
            maximum: 10000,
            cancel: &cancel
        })
        .unwrap()
    );
    assert_eq!(
        (spent, budget.used(), job.colours.capacity()),
        (paid, before, 0)
    );
}

#[test]
fn cancellation_precedes_zero_work_retry_and_does_not_touch_held_palette() {
    let budget = Budget::new(65536).unwrap();
    let held = finish(&budget, &[[7, 16, 128, 128, 255]], None, false, 0, 1).unwrap();
    let original = held.source(7);
    let mut job = one_entry_job(&budget);
    let before = budget.used();
    let cancel = AtomicBool::new(true);
    let mut spent = 0;
    assert_eq!(
        job.step(&mut DisplayWork {
            caller: &mut WorkBudget::new(0),
            spent: &mut spent,
            maximum: 10000,
            cancel: &cancel
        }),
        Err(Error::Cancelled)
    );
    assert_eq!((spent, budget.used()), (0, before));
    assert_eq!(held.source(7), original);
}

#[test]
fn malformed_palette_is_refused_before_any_owner_allocation_under_pressure() {
    let budget = Budget::new(65536).unwrap();
    let raw = Bytes::copy_from_slice(&budget, &[3, 1, 7]).unwrap();
    let pressure = budget.reserve(budget.available()).unwrap();
    let before = budget.used();
    assert!(matches!(
        PaletteBuild::new(
            &budget,
            raw,
            0,
            stamp(0),
            ColourMatrix::for_canvas(480),
            None,
            false
        ),
        Err(Error::Invalid("PGS partial palette entry"))
    ));
    drop(pressure);
    assert!(budget.used() < before);
}

#[test]
fn foreign_previous_snapshot_remains_foreign_even_for_empty_source() {
    let a = Budget::new(65536).unwrap();
    let b = Budget::new(65536).unwrap();
    let held = finish(&a, &[], None, false, 0, 1).unwrap();
    assert!(matches!(
        PaletteBuild::new(
            &b,
            Bytes::copy_from_slice(&b, &[3, 1]).unwrap(),
            0,
            stamp(0),
            ColourMatrix::for_canvas(480),
            Some(held),
            true
        ),
        Err(Error::ForeignPool)
    ));
}

#[test]
fn spill_growth_pressure_preserves_old_colour_counts_and_funded_zero_work_retry() {
    let budget = Budget::new(65536).unwrap();
    let baseline = budget.used();
    let held = finish(
        &budget,
        &[[7, 16, 128, 128, 255], [8, 31, 93, 171, 7]],
        None,
        false,
        0,
        1,
    )
    .unwrap();
    let old7 = held.source(7);
    let old8 = held.source(8);
    let raw = Bytes::copy_from_slice(&budget, &[3, 1, 7, 235, 93, 171, 0]).unwrap();
    let mut job = PaletteBuild::new(
        &budget,
        raw,
        1,
        stamp(1),
        ColourMatrix::for_canvas(1080),
        Some(held.clone()),
        true,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut spent = 0;
    for _ in 0..1000 {
        if matches!(job.phase, Phase::Entries) {
            break;
        }
        assert!(
            !job.step(&mut DisplayWork {
                caller: &mut WorkBudget::new(1),
                spent: &mut spent,
                maximum: 10000,
                cancel: &cancel
            })
            .unwrap()
        );
    }
    assert!(matches!(job.phase, Phase::Entries));
    assert_eq!(job.first_owner.as_ref().unwrap().entries, 2);
    let original = job.colours.as_slice()[7].provenance;
    let pressure = budget.reserve(budget.available()).unwrap();
    let before = budget.used();
    assert!(matches!(
        job.step(&mut DisplayWork {
            caller: &mut WorkBudget::new(64),
            spent: &mut spent,
            maximum: 10000,
            cancel: &cancel
        }),
        Err(Error::Backpressure { .. })
    ));
    let paid = spent;
    assert_eq!(job.first_owner.as_ref().unwrap().entries, 2);
    assert_eq!(job.colours.as_slice()[7].provenance, original);
    assert_eq!(job.entry, 0);
    assert!(job.incoming_owner.is_none());
    assert_eq!(budget.used(), before);
    drop(pressure);
    assert!(
        !job.step(&mut DisplayWork {
            caller: &mut WorkBudget::new(0),
            spent: &mut spent,
            maximum: 10000,
            cancel: &cancel
        })
        .unwrap()
    );
    assert_eq!(spent, paid);
    assert_eq!(job.first_owner.as_ref().unwrap().entries, 2);
    let mut ready = false;
    for _ in 0..1000 {
        if job
            .step(&mut DisplayWork {
                caller: &mut WorkBudget::new(1),
                spent: &mut spent,
                maximum: 10000,
                cancel: &cancel,
            })
            .unwrap()
        {
            ready = true;
            break;
        }
    }
    assert!(ready);
    let result = job.take().unwrap();
    assert_eq!(held.source(7), old7);
    assert_eq!(held.source(8), old8);
    assert_eq!(result.source(8), old8);
    assert_eq!(result.source(7).unwrap().stamp, stamp(1));
    owner_invariants(&held);
    owner_invariants(&result);
    drop(result);
    drop(held);
    drop(job);
    assert_eq!(budget.used(), baseline);
}
