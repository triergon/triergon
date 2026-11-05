use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, TimeBase, WorkBudget};
use triergon_subtitles::dvd::{
    HdFragmentIntegrity, HdFragmentLimits, HdFragmentOutput, HdFragmentStamp, HdFragmentState,
    HdFragmentStatus, HdPacketClock, HdPhysicalPacket, HdReset, HdSpan,
};
macro_rules! case {
    ($s:literal) => {
        include_bytes!(concat!("hd-fragment-fixtures/", $s, ".bin")).as_slice()
    };
}
fn source(data: &[u8], b: &Budget) -> Vec<HdPhysicalPacket> {
    let v3 = &data[..8] == b"SMDVC003";
    let n = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;
    let mut at = 12 + n;
    let count = u32::from_be_bytes(data[at..at + 4].try_into().unwrap());
    let num = u32::from_be_bytes(data[at + 4..at + 8].try_into().unwrap());
    let den = u32::from_be_bytes(data[at + 8..at + 12].try_into().unwrap());
    at += 12;
    let tb = TimeBase::new(u64::from(num), u64::from(den)).unwrap();
    let mut out = Vec::new();
    for order in 0..count {
        let reset = if v3 {
            let r = data[at];
            at += 1;
            r
        } else {
            0
        };
        let known = data[at] != 0;
        let raw = i64::from_be_bytes(data[at + 1..at + 9].try_into().unwrap());
        let n = u32::from_be_bytes(data[at + 9..at + 13].try_into().unwrap()) as usize;
        at += 13;
        out.push(HdPhysicalPacket {
            bytes: Bytes::copy_from_slice(b, &data[at..at + n]).unwrap(),
            clock: HdPacketClock {
                known,
                raw,
                timebase: tb,
            },
            order: u64::from(order),
            reset: match reset {
                0 => HdReset::Keep,
                1 => HdReset::Flush,
                _ => HdReset::Reopen,
            },
        });
        at += n;
    }
    assert_eq!(at, data.len());
    out
}
fn begin(
    state: HdFragmentState,
    input: HdPhysicalPacket,
) -> triergon_subtitles::dvd::HdFragmentJob {
    let mut slot = Some(state);
    HdFragmentState::start(&mut slot, input).unwrap()
}
fn one(state: HdFragmentState, p: HdPhysicalPacket, q: u64) -> (HdFragmentState, HdFragmentOutput) {
    let before = state.work_spent();
    let mut j = begin(state, p);
    let c = AtomicBool::new(false);
    let mut total = 0;
    for _ in 0..1000000 {
        let mut w = WorkBudget::new(q);
        let status = j.poll(&mut w, &c).unwrap();
        assert!(w.spent() <= q);
        total += w.spent();
        if status == HdFragmentStatus::Ready {
            assert_eq!(j.work_spent() - before, total as usize);
            return j.try_finish(&c).unwrap();
        }
    }
    panic!("bounded packet job stalled")
}
fn seq(data: &[u8], b: &Budget, q: u64) -> (HdFragmentState, Vec<HdFragmentOutput>) {
    let mut s = HdFragmentState::new(b, HdFragmentLimits::default()).unwrap();
    let mut out = Vec::new();
    for p in source(data, b) {
        let (next, r) = one(s, p, q);
        s = next;
        out.push(r);
    }
    (s, out)
}
#[test]
fn empty_state_and_zero_work_allocate_nothing() {
    let b = Budget::new(100000).unwrap();
    let baseline = b.used();
    let state = HdFragmentState::new(&b, HdFragmentLimits::default()).unwrap();
    assert_eq!(baseline, Budget::control_storage_bytes());
    assert_eq!(b.used(), baseline);
    let p = source(case!("hd-split-10"), &b).remove(0);
    let before = b.used();
    let mut job = begin(state, p);
    assert_eq!(b.used(), before);
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        HdFragmentStatus::Yield
    );
    assert_eq!(job.work_spent(), 0);
    assert_eq!(b.used(), before);
}
#[test]
fn legal_control_header_fragments_reassemble_with_exact_history_and_final_clock() {
    for data in [
        case!("hd-split-10"),
        case!("hd-split-14"),
        case!("four-wide-split-10"),
    ] {
        for q in 1..=32 {
            let b = Budget::new(100000).unwrap();
            let (s, out) = seq(data, &b, q);
            assert!(s.pending().is_none());
            assert_eq!(out[0].integrity, HdFragmentIntegrity::Pending);
            assert_eq!(out[0].projected_returned, 0);
            let full = out[1].supplied_source.as_ref().unwrap();
            assert_eq!(out[1].projected_returned, full.len());
            assert!(full.len() > out[1].physical.as_ref().unwrap().bytes.len());
            assert!(out[1].frame.as_ref().unwrap().source().picture().is_some());
            let stamps = out[1].history.stamps().collect::<Vec<_>>();
            assert_eq!(stamps.len(), 2);
            for (stamp, part) in stamps.iter().zip(&out) {
                assert_eq!(
                    stamp.raw(full).unwrap(),
                    part.physical.as_ref().unwrap().bytes.as_ref()
                );
                assert_eq!(stamp.clock, part.physical.as_ref().unwrap().clock);
            }
            assert_eq!(out[1].physical.as_ref().unwrap().clock.raw, 135000);
        }
    }
}
#[test]
fn successful_common_packet_keeps_whole_source_storage_zero_copy() {
    let b = Budget::new(100000).unwrap();
    let input = source(case!("alpha-after-flush"), &b).remove(0);
    let held = input.bytes.clone();
    let (_, out) = one(
        HdFragmentState::new(&b, HdFragmentLimits::default()).unwrap(),
        input,
        1,
    );
    let raw = out.supplied_source.as_ref().unwrap();
    assert!(raw.shares_storage_with(&held));
    assert_eq!(out.history.len(), 1);
    assert_eq!(out.integrity, HdFragmentIntegrity::Exact);
    assert_eq!(raw.retained_storage_bytes(), held.retained_storage_bytes());
}
#[test]
fn opaque_physical_prefixes_remain_observable_without_fabricating_a_frame() {
    let b = Budget::new(100000).unwrap();
    let (_, out) = seq(case!("hd-split-4"), &b, 1);
    assert_eq!(
        out[0].integrity,
        HdFragmentIntegrity::PhysicalPrefixConsumed
    );
    assert_eq!(out[0].projected_returned, 4);
    assert!(out[0].frame.is_none());
    assert_eq!(out[0].physical.as_ref().unwrap().bytes.len(), 4);
}
#[test]
fn flush_discards_pending_source_evidence_and_keeps_alpha_while_reopen_resets_it() {
    let b = Budget::new(1000000).unwrap();
    let (_, a) = seq(case!("fragment-flush-before-tail"), &b, 1);
    let evidence = a[1].reset_discarded.as_ref().unwrap();
    assert_eq!(
        evidence.bytes.as_ref(),
        a[0].physical.as_ref().unwrap().bytes.as_ref()
    );
    assert_eq!(evidence.history.len(), 1);
    let (_, a) = seq(case!("alpha-after-flush"), &b, 1);
    let f = a[1].frame.as_ref().unwrap();
    assert_eq!(f.picture_state().alpha(1).unwrap().value, 1);
    assert!(f.source().source_facts().contrast.is_none());
    let (_, a) = seq(case!("alpha-after-reopen"), &b, 1);
    let f = a[1].frame.as_ref().unwrap();
    assert_eq!(f.picture_state().alpha(1).unwrap().value, 0);
    assert!(f.source().source_facts().contrast.is_none());
}
#[test]
fn all_original_packet_clock_knownness_and_signed_ticks_survive_reassembly() {
    for data in [
        case!("fragment-known-clock-changes"),
        case!("fragment-first-unknown-last-known"),
        case!("fragment-first-known-last-unknown"),
        case!("fragment-first-negative-last-positive"),
        case!("fragment-known-sentinel-last-negative"),
    ] {
        let b = Budget::new(100000).unwrap();
        let expected = source(data, &b);
        let (_, out) = seq(data, &b, 3);
        let stamps = out[1].history.stamps().collect::<Vec<_>>();
        for (stamp, p) in stamps.iter().zip(&expected) {
            assert_eq!(stamp.clock, p.clock);
        }
        assert_eq!(out[1].physical.as_ref().unwrap().clock, expected[1].clock);
        assert!(out[1].frame.as_ref().unwrap().source().picture().is_some());
    }
}
#[test]
fn eos_retains_incomplete_source_and_rejects_later_physical_input() {
    let b = Budget::new(100000).unwrap();
    let p = source(case!("hd-split-10"), &b).remove(0);
    let bytes = p.bytes.clone();
    let (state, _) = one(
        HdFragmentState::new(&b, HdFragmentLimits::default()).unwrap(),
        p,
        1,
    );
    let mut job = state.end();
    let c = AtomicBool::new(false);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != HdFragmentStatus::Ready {}
    let (state, out) = job.try_finish(&c).unwrap();
    assert_eq!(
        out.incomplete_at_end.as_ref().unwrap().bytes.as_ref(),
        bytes.as_ref()
    );
    assert!(out.physical.is_none());
    let p = source(case!("hd-split-10"), &b).remove(0);
    let mut slot = Some(state);
    let e = HdFragmentState::start(&mut slot, p).unwrap_err();
    assert_eq!(e.error, Error::Invalid("HD physical input after end"));
    assert_eq!(e.input.bytes.as_ref(), bytes.as_ref());
}
#[test]
fn retryable_allocation_pressure_never_loses_pending_or_incoming_bytes() {
    let b = Budget::new(100000).unwrap();
    let mut p = source(case!("hd-split-10"), &b);
    let second = p.pop().unwrap();
    let first = p.pop().unwrap();
    let expected = [first.bytes.as_ref(), second.bytes.as_ref()].concat();
    let (state, _) = one(
        HdFragmentState::new(&b, HdFragmentLimits::default()).unwrap(),
        first,
        1,
    );
    let mut job = begin(state, second);
    let c = AtomicBool::new(false);
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(100000), &c),
        Err(Error::Backpressure { .. })
    ));
    let used = b.used();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap(),
        HdFragmentStatus::Yield
    );
    assert_eq!(b.used(), used);
    drop(blocker);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != HdFragmentStatus::Ready {}
    let (_, out) = job.try_finish(&c).unwrap();
    assert_eq!(out.supplied_source.as_ref().unwrap().as_ref(), expected);
}
#[test]
fn fragment_and_input_limits_return_original_state_and_owned_incoming_input() {
    let b = Budget::new(100000).unwrap();
    let p = source(case!("hd-split-10"), &b);
    let full = [p[0].bytes.as_ref(), p[1].bytes.as_ref()].concat();
    let clock = p[0].clock;
    let limits = HdFragmentLimits {
        max_fragments: 2,
        ..HdFragmentLimits::default()
    };
    let mut state = HdFragmentState::new(&b, limits).unwrap();
    for (order, range) in [(0, 0..10), (1, 10..12)] {
        let (next, _) = one(
            state,
            HdPhysicalPacket {
                bytes: Bytes::copy_from_slice(&b, &full[range]).unwrap(),
                clock,
                order,
                reset: HdReset::Keep,
            },
            1,
        );
        state = next;
    }
    let mut slot = Some(state);
    let e = HdFragmentState::start(
        &mut slot,
        HdPhysicalPacket {
            bytes: Bytes::copy_from_slice(&b, &full[12..]).unwrap(),
            clock,
            order: 2,
            reset: HdReset::Keep,
        },
    )
    .unwrap_err();
    assert!(matches!(
        e.error,
        Error::Limit {
            requested: 3,
            maximum: 2
        }
    ));
    assert_eq!(slot.as_ref().unwrap().pending().unwrap(), &full[..12]);
    assert_eq!(e.input.bytes.as_ref(), &full[12..]);
    let limits = HdFragmentLimits {
        max_input: 10,
        ..HdFragmentLimits::default()
    };
    let (state, _) = one(HdFragmentState::new(&b, limits).unwrap(), p[0].clone(), 1);
    let mut slot = Some(state);
    let e = HdFragmentState::start(&mut slot, p[1].clone()).unwrap_err();
    assert!(matches!(e.error, Error::Limit { maximum: 10, .. }));
    assert_eq!(
        slot.as_ref().unwrap().pending().unwrap(),
        p[0].bytes.as_ref()
    );
}
#[test]
fn foreign_pool_and_resource_errors_are_not_native_recovery() {
    let b = Budget::new(100000).unwrap();
    let other = Budget::new(100000).unwrap();
    let p = source(case!("hd-split-10"), &other).remove(0);
    let mut slot = Some(HdFragmentState::new(&b, HdFragmentLimits::default()).unwrap());
    let e = HdFragmentState::start(&mut slot, p).unwrap_err();
    assert_eq!(e.error, Error::ForeignPool);
    assert!(slot.as_ref().unwrap().pending().is_none());
    assert!(e.input.bytes.belongs_to(&other));
    let limits = HdFragmentLimits {
        max_work: 0,
        ..HdFragmentLimits::default()
    };
    let mut j = begin(
        HdFragmentState::new(&b, limits).unwrap(),
        source(case!("hd-split-10"), &b).remove(0),
    );
    let c = AtomicBool::new(false);
    assert!(matches!(
        j.poll(&mut WorkBudget::new(1), &c),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(j.try_finish(&c), Err(Error::Limit { .. })));
}
#[test]
fn cancellation_during_paid_copy_is_sticky_and_retained_outputs_stay_charged() {
    let b = Budget::new(100000).unwrap();
    let baseline = b.used();
    let mut p = source(case!("hd-split-10"), &b);
    let second = p.pop().unwrap();
    let first = p.pop().unwrap();
    let charge = first.bytes.retained_storage_bytes();
    let (state, held) = one(
        HdFragmentState::new(&b, HdFragmentLimits::default()).unwrap(),
        first,
        1,
    );
    let mut job = begin(state, second);
    let c = AtomicBool::new(false);
    assert_eq!(
        job.poll(&mut WorkBudget::new(20), &c).unwrap(),
        HdFragmentStatus::Yield
    );
    let spent = job.work_spent();
    assert!(spent > 0);
    c.store(true, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(job.work_spent(), spent);
    c.store(false, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut WorkBudget::new(100000), &c).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(b.used(), baseline + charge);
    drop(held);
    assert_eq!(b.used(), baseline);
}
#[test]
fn mutable_public_stamp_access_remains_checked() {
    let stamp = HdFragmentStamp {
        order: 0,
        clock: HdPacketClock {
            known: false,
            raw: -123,
            timebase: TimeBase::new(1, 90000).unwrap(),
        },
        range: HdSpan {
            start: usize::MAX,
            end: 0,
        },
    };
    assert!(std::panic::catch_unwind(|| stamp.raw(&[1, 2, 3])).is_ok());
    assert_eq!(stamp.raw(&[1, 2, 3]), Err(Error::Truncated));
}

#[test]
fn prefix_projection_preserves_declared_extent_and_missing_end_source_error() {
    for data in [case!("classic-split-35"), case!("hd-split-1071")] {
        let b = Budget::new(100000).unwrap();
        let (_, out) = seq(data, &b, 1);
        let first = &out[0];
        assert!(matches!(
            first.integrity,
            HdFragmentIntegrity::Truncated { .. }
        ));
        assert_eq!(first.source_error(), Some(Error::Truncated));
        let source = first.supplied_source.as_ref().unwrap();
        assert_eq!(
            source.as_ref(),
            first.physical.as_ref().unwrap().bytes.as_ref()
        );
        assert!(matches!(
            triergon_subtitles::dvd::HdPacket::parse(source),
            Err(Error::Truncated)
        ));
        let picture = first.frame.as_ref().unwrap().source().picture().unwrap();
        assert!(picture.facts().ended_at_input);
        assert_eq!(picture.packet().as_ref(), source.as_ref());
    }
}
#[test]
fn invalid_continuation_headers_consume_without_becoming_pending_source() {
    for data in [
        case!("classic-split-2"),
        case!("classic-split-3"),
        case!("classic-split-6"),
        case!("classic-split-10"),
        case!("classic-split-18"),
        case!("hd-split-536"),
    ] {
        let b = Budget::new(100000).unwrap();
        let (state, out) = seq(data, &b, 1);
        assert!(state.pending().is_none());
        for p in out {
            assert_eq!(
                p.projected_returned,
                p.physical.as_ref().unwrap().bytes.len()
            );
            assert!(
                p.frame
                    .as_ref()
                    .and_then(|p| p.source().picture())
                    .is_none()
            );
        }
    }
}
#[test]
fn native_rectangle_forced_aggregation_keeps_selected_local_source_flag_false() {
    for data in [
        case!("eligible-2-first-forced"),
        case!("eligible-3-first-forced"),
        case!("middle-forced-last-ordinary"),
    ] {
        let b = Budget::new(100000).unwrap();
        let (_, out) = seq(data, &b, 1);
        let picture = out[0].frame.as_ref().unwrap().source().picture().unwrap();
        assert!(!picture.facts().forced);
        assert!(picture.facts().menu);
        assert!(picture.native_forced());
    }
}
#[test]
fn paid_freeze_pressure_restores_assembled_buffer_and_does_not_allocate_on_zero_work() {
    let b = Budget::new(100000).unwrap();
    let mut p = source(case!("hd-split-10"), &b);
    let second = p.pop().unwrap();
    let first = p.pop().unwrap();
    let total = first.bytes.len() + second.bytes.len();
    let expected = [first.bytes.as_ref(), second.bytes.as_ref()].concat();
    let (state, _) = one(
        HdFragmentState::new(&b, HdFragmentLimits::default()).unwrap(),
        first,
        1,
    );
    let before = state.work_spent();
    let until = before + 1 + 1 + total + 10;
    let mut job = begin(state, second);
    let c = AtomicBool::new(false);
    while job.work_spent() < until {
        assert_eq!(
            job.poll(&mut WorkBudget::new(1), &c).unwrap(),
            HdFragmentStatus::Yield
        );
    }
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(1), &c),
        Err(Error::Backpressure { .. })
    ));
    let spent = job.work_spent();
    let used = b.used();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &c).unwrap(),
        HdFragmentStatus::Yield
    );
    assert_eq!(b.used(), used);
    assert_eq!(job.work_spent(), spent);
    drop(blocker);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != HdFragmentStatus::Ready {}
    let (_, out) = job.try_finish(&c).unwrap();
    assert_eq!(out.supplied_source.as_ref().unwrap().as_ref(), expected);
    assert!(out.frame.as_ref().unwrap().source().picture().is_some());
}

#[test]
fn busy_state_slot_rejects_owned_input_without_losing_the_active_job() {
    let b = Budget::new(100000).unwrap();
    let mut p = source(case!("hd-split-10"), &b);
    let second = p.pop().unwrap();
    let first = p.pop().unwrap();
    let mut slot = Some(HdFragmentState::new(&b, HdFragmentLimits::default()).unwrap());
    let mut job = HdFragmentState::start(&mut slot, first).unwrap();
    assert!(slot.is_none());
    let rejected = HdFragmentState::start(&mut slot, second).unwrap_err();
    assert_eq!(
        rejected.error,
        Error::Invalid("HD fragment state is already in use")
    );
    assert!(slot.is_none());
    let c = AtomicBool::new(false);
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != HdFragmentStatus::Ready {}
    let (state, _) = job.try_finish(&c).unwrap();
    slot = Some(state);
    let mut job = HdFragmentState::start(&mut slot, rejected.input).unwrap();
    while job.poll(&mut WorkBudget::new(1), &c).unwrap() != HdFragmentStatus::Ready {}
    let (state, out) = job.try_finish(&c).unwrap();
    assert!(state.pending().is_none());
    assert!(out.frame.as_ref().unwrap().source().picture().is_some());
}
