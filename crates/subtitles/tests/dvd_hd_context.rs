use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{
    HdContextJob, HdContextOrigin, HdContextOutput, HdLimits, HdReceiverContext, HdReceiverStatus,
    HdSpan, HdStatus,
};
macro_rules! case {
    ($name:literal) => {
        include_bytes!(concat!("hd-context-fixtures/", $name, ".bin")).as_slice()
    };
}
fn packets(data: &[u8]) -> Vec<&[u8]> {
    assert_eq!(&data[..8], b"SMDVC002");
    let private = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;
    let mut at = 12 + private;
    let count = u32::from_be_bytes(data[at..at + 4].try_into().unwrap());
    at += 12;
    let mut output = Vec::new();
    for _ in 0..count {
        let n = u32::from_be_bytes(data[at + 9..at + 13].try_into().unwrap()) as usize;
        at += 13;
        output.push(&data[at..at + n]);
        at += n;
    }
    assert_eq!(at, data.len());
    output
}
fn one(raw: &[u8], state: &HdReceiverContext, b: &Budget, q: u64) -> HdContextOutput {
    let c = AtomicBool::new(false);
    let mut j = HdContextJob::new(
        Bytes::copy_from_slice(b, raw).unwrap(),
        b,
        HdLimits::default(),
        state,
    )
    .unwrap();
    j.finish_input();
    let mut spent = 0;
    for _ in 0..100000 {
        let mut work = WorkBudget::new(q);
        let status = j.poll(&mut work, &c).unwrap();
        assert!(work.spent() <= q);
        spent += work.spent();
        if status == HdReceiverStatus::Ready {
            assert_eq!(j.work_spent() as u64, spent);
            return j.try_finish(&c).unwrap();
        }
    }
    panic!("bounded context source did not finish")
}
fn sequence(data: &[u8], b: &Budget, q: u64) -> Vec<HdContextOutput> {
    let mut state = HdReceiverContext::new(b);
    let mut output = Vec::new();
    for raw in packets(data) {
        let result = one(raw, &state, b, q);
        state = result.next_state().clone();
        output.push(result);
    }
    output
}
#[test]
fn empty_context_allocates_nothing_and_reset_values_have_no_source() {
    let b = Budget::new(100000).unwrap();
    let base = b.used();
    let mut s = HdReceiverContext::new(&b);
    assert_eq!(b.used(), base);
    assert_eq!(base, Budget::control_storage_bytes());
    assert_eq!(s.selectors().unwrap(), [0; 4]);
    for i in 0..=255 {
        let value = s.alpha(i).unwrap();
        assert_eq!(value.value, 0);
        assert!(matches!(value.origin, HdContextOrigin::ReceiverReset));
        assert!(value.origin.raw().unwrap().is_none());
    }
    s.reset();
    assert_eq!(b.used(), base);
    assert!(s.retained_sources().iter().all(Option::is_none));
}
#[test]
fn inherited_alpha_is_observable_without_inventing_current_source_contrast() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("eight-contrast-persists"), &b, 1);
    let second = &out[1];
    assert!(second.source().source_facts().contrast.is_none());
    assert!(second.source().picture().unwrap().contrast().is_none());
    for i in 0..=255 {
        let a = second.picture_state().alpha(i).unwrap();
        assert_eq!(a.value, i);
        match a.origin {
            HdContextOrigin::Contrast { owner, span } => {
                assert!(owner.shares_storage_with(out[0].source().packet()));
                assert_eq!(span.get(owner).unwrap()[usize::from(i)], 255 - i);
            }
            _ => panic!("inherited source origin absent"),
        }
    }
}
#[test]
fn local_yuv_changes_while_alpha_owner_stays_on_prior_packet() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("eight-new-yuv-inherited-alpha"), &b, 3);
    assert_ne!(
        out[0].source().picture().unwrap().ycrcb(),
        out[1].source().picture().unwrap().ycrcb()
    );
    assert_eq!(out[1].picture_state().alpha(200).unwrap().value, 200);
    assert!(out[1].source().source_facts().contrast.is_none());
}
#[test]
fn packed_override_persists_only_for_first_four_entries() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("packed-alpha-persists"), &b, 1);
    let state = out[1].picture_state();
    for (i, v) in [2, 4, 8, 15].into_iter().enumerate() {
        let a = state.alpha(i as u8).unwrap();
        assert_eq!(a.value, v);
        assert!(matches!(a.origin, HdContextOrigin::Packed { .. }));
    }
    for i in 4..=255 {
        assert_eq!(state.alpha(i).unwrap().value, i);
    }
    assert!(out[1].source().source_facts().alpha4_word.is_none());
}
#[test]
fn full_contrast_replaces_earlier_packed_override_for_later_packets() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("full-contrast-clears-inherited-packed"), &b, 5);
    for i in 0..=255 {
        let a = out[2].picture_state().alpha(i).unwrap();
        assert_eq!(a.value, 255 - ((3 * u16::from(i) + 17) & 255) as u8);
        match a.origin {
            HdContextOrigin::Contrast { owner, .. } => {
                assert!(owner.shares_storage_with(out[1].source().packet()));
            }
            _ => panic!("full contrast did not replace packed"),
        }
    }
}
#[test]
fn four_entry_selectors_and_alpha_keep_original_owner_and_unknown_source_rgb() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("four-selectors-alpha-persist"), &b, 1);
    let second = &out[1];
    let f = second.source().source_facts();
    assert!(f.palette4_word.is_none() && f.alpha4_word.is_none());
    assert_eq!(second.picture_state().selectors().unwrap(), [0, 1, 2, 3]);
    let p = second.picture_state().four_palette(None).unwrap();
    assert_eq!(p.effective_alpha, [2, 4, 8, 15]);
    assert!(p.source_rgb.is_none());
    match second.picture_state().selectors_origin() {
        HdContextOrigin::Packed { owner, .. } => {
            assert!(owner.shares_storage_with(out[0].source().packet()));
        }
        _ => panic!("selector source missing"),
    }
    let source = [0x123456; 16];
    assert_eq!(
        second
            .picture_state()
            .four_palette(Some(&source))
            .unwrap()
            .source_rgb,
        Some(source)
    );
}
#[test]
fn four_to_eight_carries_raw_nibbles_without_inventing_other_alpha_entries() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("four-to-eight-state"), &b, 1);
    let state = out[1].picture_state();
    assert_eq!(state.alpha(0).unwrap().value, 2);
    assert_eq!(state.alpha(3).unwrap().value, 15);
    for i in 4..=255 {
        let a = state.alpha(i).unwrap();
        assert_eq!(a.value, 0);
        assert!(matches!(a.origin, HdContextOrigin::ReceiverReset));
    }
}
#[test]
fn eight_to_four_preserves_inherited_effective_alpha() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("eight-to-four-state"), &b, 1);
    let p = out[1].picture_state().four_palette(None).unwrap();
    assert_eq!(p.effective_alpha, [0, 1, 2, 3]);
    assert_eq!(p.argb.map(|v| v >> 24), [0, 17, 34, 51]);
}
#[test]
fn rejected_rle_retains_error_and_commits_only_the_parsed_context_prefix() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("rejected-rle-still-updates-alpha"), &b, 1);
    assert_eq!(
        out[0].source().source_error(),
        Some(Error::Invalid("HD run exceeds source row"))
    );
    assert!(out[0].source().picture().is_none());
    assert_eq!(out[1].picture_state().alpha(0).unwrap().value, 238);
    assert_eq!(out[1].picture_state().alpha(1).unwrap().value, 235);
    match out[1].picture_state().alpha(99).unwrap().origin {
        HdContextOrigin::Contrast { owner, .. } => {
            assert!(owner.shares_storage_with(out[0].source().packet()));
        }
        _ => panic!("rejected prefix source absent"),
    }
}
#[test]
fn missing_local_yuv_is_never_supplied_from_a_previous_packet() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("missing-yuv-still-updates-alpha"), &b, 1);
    let picture = out[1].source().picture().unwrap();
    assert!(picture.ycrcb().is_none());
    assert!(matches!(
        out[1].picture_state().palette_job(picture),
        Err(Error::Unsupported("HD source YCrCb absent"))
    ));
    assert_eq!(out[2].picture_state().alpha(0).unwrap().value, 238);
}
#[test]
fn later_palette_control_updates_next_context_without_repainting_selected_image() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("late-alpha-persists-next-packet"), &b, 1);
    assert_eq!(out[0].picture_state().alpha(1).unwrap().value, 1);
    assert_eq!(out[0].next_state().alpha(1).unwrap().value, 235);
    assert_ne!(
        out[0].source().source_facts().contrast,
        out[0].source().parsed_context_facts().contrast
    );
    assert_eq!(out[1].picture_state().alpha(1).unwrap().value, 235);
}
#[test]
fn all_predeclared_sequences_are_work_quantum_invariant() {
    let inputs = [
        case!("eight-contrast-persists"),
        case!("eight-new-yuv-inherited-alpha"),
        case!("packed-alpha-persists"),
        case!("four-selectors-alpha-persist"),
        case!("full-contrast-clears-inherited-packed"),
        case!("rejected-rle-still-updates-alpha"),
        case!("missing-yuv-still-updates-alpha"),
        case!("four-to-eight-state"),
        case!("eight-to-four-state"),
        case!("late-alpha-persists-next-packet"),
    ];
    for data in inputs {
        let b = Budget::new(1000000).unwrap();
        let expected = sequence(data, &b, 1);
        for q in 2..=32 {
            let actual = sequence(data, &b, q);
            for (a, e) in actual.iter().zip(&expected) {
                assert_eq!(a.source().source_error(), e.source().source_error());
                assert_eq!(a.source().source_facts(), e.source().source_facts());
                assert_eq!(
                    a.source().picture().map(|p| p.bitmap().pixels().as_ref()),
                    e.source().picture().map(|p| p.bitmap().pixels().as_ref())
                );
                for i in 0..=255 {
                    assert_eq!(
                        a.picture_state().alpha(i).unwrap().value,
                        e.picture_state().alpha(i).unwrap().value
                    );
                    assert_eq!(
                        a.next_state().alpha(i).unwrap().value,
                        e.next_state().alpha(i).unwrap().value
                    );
                }
            }
        }
    }
}
#[test]
fn zero_work_pressure_retry_and_context_commit_are_bounded() {
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let raw = packets(case!("eight-contrast-persists"))[0];
    let initial = HdReceiverContext::new(&b);
    let packet = Bytes::copy_from_slice(&b, raw).unwrap();
    let before = b.used();
    let mut job = HdContextJob::new(packet, &b, HdLimits::default(), &initial).unwrap();
    assert_eq!(b.used(), before);
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap(),
        HdReceiverStatus::Yield
    );
    assert_eq!(b.used(), before);
    assert_eq!(job.work_spent(), 0);
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(100000), &c),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    job.finish_input();
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != HdReceiverStatus::Ready {}
    assert_eq!(initial.alpha(1).unwrap().value, 0);
    let result = job.try_finish(&c).unwrap();
    assert_eq!(result.next_state().alpha(1).unwrap().value, 1);
}
#[test]
fn cancellation_is_sticky_and_does_not_mutate_or_uncharge_external_state() {
    let b = Budget::new(100000).unwrap();
    let raw = packets(case!("eight-contrast-persists"))[0];
    let first = one(raw, &HdReceiverContext::new(&b), &b, 1);
    let state = first.next_state().clone();
    drop(first);
    let retained = b.used();
    let c = AtomicBool::new(true);
    let mut job = HdContextJob::new(
        Bytes::copy_from_slice(&b, raw).unwrap(),
        &b,
        HdLimits::default(),
        &state,
    )
    .unwrap();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap_err(),
        Error::Cancelled
    );
    c.store(false, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut WorkBudget::new(100000), &c).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(b.used(), retained);
    assert_eq!(state.alpha(99).unwrap().value, 99);
}
#[test]
fn retained_cache_sources_keep_full_charge_after_outputs_and_jobs_drop() {
    let b = Budget::new(100000).unwrap();
    let baseline = b.used();
    let raw = packets(case!("eight-contrast-persists"))[0];
    let output = one(raw, &HdReceiverContext::new(&b), &b, 1);
    let charge = output.source().packet().retained_storage_bytes();
    let mut state = output.next_state().clone();
    drop(output);
    assert_eq!(b.used(), baseline + charge);
    let owner = match state.alpha(1).unwrap().origin {
        HdContextOrigin::Contrast { owner, .. } => owner.clone(),
        _ => panic!("source missing"),
    };
    state.reset();
    assert_eq!(b.used(), baseline + charge);
    assert_eq!(owner.as_ref(), raw);
    drop(owner);
    assert_eq!(b.used(), baseline);
}
#[test]
fn a_context_can_retain_three_distinct_sources_and_replacement_releases_old_one() {
    let b = Budget::new(100000).unwrap();
    let a = packets(case!("eight-contrast-persists"))[0];
    let p = packets(case!("four-selectors-alpha-persist"))[0];
    let c = packets(case!("eight-to-four-state"))[1];
    let first = one(a, &HdReceiverContext::new(&b), &b, 1);
    let second = one(p, first.next_state(), &b, 1);
    let third = one(c, second.next_state(), &b, 1);
    let state = third.next_state();
    let slots = state.retained_sources();
    assert!(slots.iter().all(Option::is_some));
    assert!(!slots[0].unwrap().shares_storage_with(slots[1].unwrap()));
    assert!(!slots[1].unwrap().shares_storage_with(slots[2].unwrap()));
    assert!(!slots[0].unwrap().shares_storage_with(slots[2].unwrap()));
    let replacement = one(a, state, &b, 1);
    assert!(replacement.next_state().retained_sources()[1].is_none());
}
#[test]
fn foreign_pool_and_resource_limits_never_become_source_recovery() {
    let b = Budget::new(100000).unwrap();
    let other = Budget::new(100000).unwrap();
    let state = HdReceiverContext::new(&b);
    let raw = packets(case!("eight-contrast-persists"))[0];
    assert!(matches!(
        HdContextJob::new(
            Bytes::copy_from_slice(&other, raw).unwrap(),
            &other,
            HdLimits::default(),
            &state
        ),
        Err(Error::ForeignPool)
    ));
    let limits = HdLimits {
        max_work: 1,
        ..HdLimits::default()
    };
    let mut job =
        HdContextJob::new(Bytes::copy_from_slice(&b, raw).unwrap(), &b, limits, &state).unwrap();
    job.finish_input();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(100000), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(
        job.try_finish(&AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
}
#[test]
fn palette_has_paid_entries_zero_work_and_sticky_cancellation() {
    let b = Budget::new(100000).unwrap();
    let out = sequence(case!("eight-contrast-persists"), &b, 1);
    let picture = out[1].source().picture().unwrap();
    let mut job = out[1].picture_state().palette_job(picture).unwrap();
    let c = AtomicBool::new(false);
    let before = b.used();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap(),
        HdStatus::Yield
    );
    assert_eq!(job.entries_done(), 0);
    assert_eq!(b.used(), before);
    assert_eq!(
        job.poll(&mut WorkBudget::new(3), &c).unwrap(),
        HdStatus::Yield
    );
    assert_eq!(job.entries_done(), 3);
    c.store(true, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap_err(),
        Error::Cancelled
    );
    c.store(false, Ordering::Relaxed);
    assert_eq!(job.try_finish(&c).unwrap_err(), Error::Cancelled);
    let mut job = out[1].picture_state().palette_job(picture).unwrap();
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != HdStatus::Ready {}
    let words = job.try_finish(&c).unwrap();
    assert_eq!(words[200] >> 24, 200);
    drop(out);
    assert_eq!(words[99] >> 24, 99);
}
#[test]
fn public_context_origin_metadata_access_is_checked() {
    let b = Budget::new(100000).unwrap();
    let owner = Bytes::copy_from_slice(&b, &[1, 2, 3]).unwrap();
    let origin = HdContextOrigin::Packed {
        owner: &owner,
        span: HdSpan {
            start: usize::MAX,
            end: 0,
        },
    };
    assert!(std::panic::catch_unwind(|| origin.raw()).is_ok());
    assert_eq!(origin.raw(), Err(Error::Truncated));
}
#[test]
fn resource_failure_after_parsing_alpha_cannot_commit_partial_receiver_state() {
    let b = Budget::new(100000).unwrap();
    let raws = packets(case!("full-contrast-clears-inherited-packed"));
    let first = one(raws[0], &HdReceiverContext::new(&b), &b, 1);
    let state = first.next_state().clone();
    drop(first);
    let retained = b.used();
    let limits = HdLimits {
        max_pixels: 1,
        ..HdLimits::default()
    };
    let c = AtomicBool::new(false);
    let mut job = HdContextJob::new(
        Bytes::copy_from_slice(&b, raws[1]).unwrap(),
        &b,
        limits,
        &state,
    )
    .unwrap();
    job.finish_input();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(100000), &c),
        Err(Error::Limit {
            requested: 8,
            maximum: 1
        })
    ));
    assert!(matches!(job.try_finish(&c), Err(Error::Limit { .. })));
    assert_eq!(state.alpha(1).unwrap().value, 4);
    assert_eq!(b.used(), retained);
}
#[test]
fn cancellation_after_ready_drops_private_outputs_but_preserves_callers_state() {
    let b = Budget::new(100000).unwrap();
    let raws = packets(case!("eight-contrast-persists"));
    let first = one(raws[0], &HdReceiverContext::new(&b), &b, 1);
    let state = first.next_state().clone();
    drop(first);
    let retained = b.used();
    let c = AtomicBool::new(false);
    let mut job = HdContextJob::new(
        Bytes::copy_from_slice(&b, raws[1]).unwrap(),
        &b,
        HdLimits::default(),
        &state,
    )
    .unwrap();
    job.finish_input();
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != HdReceiverStatus::Ready {}
    assert!(b.used() > retained);
    c.store(true, Ordering::Relaxed);
    assert_eq!(job.try_finish(&c).unwrap_err(), Error::Cancelled);
    assert_eq!(b.used(), retained);
    assert_eq!(state.alpha(199).unwrap().value, 199);
}
