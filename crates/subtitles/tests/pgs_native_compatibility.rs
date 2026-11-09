//! Public API guards of the PGS receiver. The subtitle worker's `pgs-` modes
//! fold every recorded case under `native-compatibility-gold/` and check it
//! against the recorded transcript.
#![forbid(unsafe_code)]
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, TimeBase, WorkBudget};
use triergon_subtitles::compat::pgs::{
    IndexedRecoveryDecoder, ReceiverDecoder, ReceiverEof, ReceiverPacket,
};
use triergon_subtitles::pgs::{DisplayConfig, DisplayLimits, PacketClock, RecoveryStatus};

fn segment(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    out.extend_from_slice(body);
}

fn pcs(width: u16, height: u16, state: u8, ids: &[u16]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    body.extend_from_slice(&[0x10, 0, 1, state, 0, 0, ids.len() as u8]);
    for id in ids {
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(&[0, 0, 0, 3, 0, 0]);
    }
    let mut out = Vec::new();
    segment(&mut out, 0x16, &body);
    out
}

fn picture() -> Vec<u8> {
    let mut out = pcs(16, 1, 0x80, &[7]);
    segment(
        &mut out,
        0x14,
        &[0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 128],
    );
    segment(
        &mut out,
        0x15,
        &[0, 7, 0, 0xc0, 0, 0, 8, 0, 2, 0, 1, 1, 2, 0, 0],
    );
    segment(&mut out, 0x80, &[]);
    out
}

fn clock() -> PacketClock {
    PacketClock {
        pts: Some(90_000),
        dts: Some(89_000),
        timebase: TimeBase::new(1, 90_000).unwrap(),
    }
}

fn decoder(b: &Budget, forced: bool) -> ReceiverDecoder {
    ReceiverDecoder::new(
        b,
        DisplayLimits::default(),
        DisplayConfig {
            receiver_forced_only: forced,
        },
    )
    .unwrap()
}

fn push(d: &mut ReceiverDecoder, b: &Budget, raw: &[u8]) {
    let mut owned = Some(Bytes::copy_from_slice(b, raw).unwrap());
    d.push(&mut owned, clock(), &AtomicBool::new(false))
        .unwrap();
    assert!(owned.is_none());
}

fn ready(d: &mut ReceiverDecoder, quantum: u64) -> ReceiverPacket {
    let cancel = AtomicBool::new(false);
    for _ in 0..1_000_000 {
        let mut work = WorkBudget::new(quantum);
        let status = d.poll(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        if status == RecoveryStatus::OutputReady {
            return d.take_output(&cancel).unwrap();
        }
        assert_eq!(status, RecoveryStatus::Yield);
    }
    panic!("native compatibility did not finish bounded input");
}

fn assert_picture(out: &ReceiverPacket, forced: bool) {
    assert_eq!(out.context_canvas, [16, 1]);
    assert_eq!(out.clock, clock());
    let event = out.receiver_output().unwrap();
    assert_eq!(event.pts_microseconds, Some(1_000_000));
    assert_eq!(event.rectangles().len(), 1);
    let r = &event.rectangles()[0];
    assert_eq!(
        (r.x, r.y, r.width, r.height, r.stride, r.forced),
        (3, 0, 2, 1, 2, false)
    );
    assert_eq!(r.pixels().as_ref(), &[1, 2]);
    if forced {
        assert_eq!(r.palette(), &[0; 256]);
    } else {
        assert_eq!((r.palette()[1], r.palette()[2]), (0xffff_ffff, 0x8000_0000));
    }
}

#[test]
fn native_application_output_has_literal_pixels_palette_geometry_and_raw_clock() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let mut d = decoder(&b, false);
    assert_eq!(
        b.used(),
        baseline,
        "constructing native compatibility needs no strict Source allocation"
    );
    let raw = picture();
    push(&mut d, &b, &raw);
    let out = ready(&mut d, 1);
    assert_picture(&out, false);
    assert_eq!(out.raw_packet().as_ref(), raw);
    drop(d);
    assert_picture(&out, false);
    drop(out);
    assert_eq!(b.used(), baseline);
}

#[test]
fn native_constructor_preserves_finite_policy_caps_without_strict_source_setup() {
    let b = Budget::new(4096).unwrap();
    let baseline = b.used();
    for limits in [
        DisplayLimits {
            max_objects: 65,
            ..DisplayLimits::default()
        },
        DisplayLimits {
            max_palettes: 9,
            ..DisplayLimits::default()
        },
        DisplayLimits {
            max_references: 256,
            ..DisplayLimits::default()
        },
        DisplayLimits {
            max_windows: 256,
            ..DisplayLimits::default()
        },
        DisplayLimits {
            max_segments: 4097,
            ..DisplayLimits::default()
        },
    ] {
        assert!(matches!(
            ReceiverDecoder::new(&b, limits, DisplayConfig::default()),
            Err(Error::Limit { .. })
        ));
        assert_eq!(b.used(), baseline);
    }
}

#[test]
fn duplicate_display_keeps_later_hidden_caches_and_final_canvas_for_next_packet() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b, false);
    let mut raw = pcs(16, 1, 0x80, &[]);
    segment(&mut raw, 0x80, &[]);
    let mut later = picture();
    later[10] = 0; // Second PCS is a normal update, not a new epoch.
    raw.extend_from_slice(&later);
    raw.extend_from_slice(&pcs(32, 2, 0, &[7]));
    push(&mut d, &b, &raw);
    let clear = ready(&mut d, 1);
    assert!(clear.receiver_output().unwrap().rectangles().is_empty());
    assert_eq!(clear.context_canvas, [32, 2]);
    let mut next = pcs(32, 2, 0, &[7]);
    segment(&mut next, 0x80, &[]);
    push(&mut d, &b, &next);
    let restored = ready(&mut d, 7);
    let r = &restored.receiver_output().unwrap().rectangles()[0];
    assert_eq!(r.pixels().as_ref(), &[1, 2]);
    assert_eq!((r.palette()[1], r.palette()[2]), (0xffff_ffff, 0x8000_0000));
    assert_eq!(restored.context_canvas, [32, 2]);
    assert_eq!(clear.raw_packet().as_ref(), raw);
}

#[test]
fn full_api_still_retains_both_source_sets_and_additional_receiver_event() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let c = AtomicBool::new(false);
    let mut full =
        IndexedRecoveryDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default())
            .unwrap();
    let mut raw = pcs(16, 1, 0x80, &[]);
    segment(&mut raw, 0x80, &[]);
    let mut later = picture();
    later[10] = 0;
    raw.extend_from_slice(&later);
    let mut owned = Some(Bytes::copy_from_slice(&b, &raw).unwrap());
    full.push(&mut owned, clock(), &c).unwrap();
    let mut result = None;
    for _ in 0..1_000_000 {
        if full.poll(&mut WorkBudget::new(7), &c).unwrap() == RecoveryStatus::OutputReady {
            result = Some(full.take_output(&c).unwrap());
            break;
        }
    }
    let out = result.expect("full Source+Receiver operation must finish");
    assert_eq!(out.source_error(), None);
    assert_eq!(out.source_sets().len(), 2);
    assert!(out.source_sets()[0].is_clear());
    assert_eq!(out.source_sets()[1].objects()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(out.receiver_sets().len(), 2);
    assert_eq!(out.omitted_receiver_events(), 1);
    assert!(out.receiver_output().unwrap().rectangles().is_empty());
}

#[test]
fn small_work_and_zero_work_keep_one_native_operation_and_exact_owned_result() {
    let mut totals = Vec::new();
    for quantum in [1, 7, 4096] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let mut d = decoder(&b, false);
        push(&mut d, &b, &picture());
        let before = b.used();
        let mut zero = WorkBudget::new(0);
        assert_eq!(
            d.poll(&mut zero, &AtomicBool::new(false)).unwrap(),
            RecoveryStatus::Yield
        );
        assert_eq!((zero.spent(), d.work_spent(), b.used()), (0, 0, before));
        assert_picture(&ready(&mut d, quantum), false);
        totals.push(d.work_spent());
    }
    assert!(totals[0] > 0);
    assert_eq!(totals, vec![totals[0]; 3]);
}

#[test]
fn funded_pressure_retries_do_not_repeat_work_or_lose_the_packet_owner() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut clean = decoder(&b, false);
    push(&mut clean, &b, &picture());
    let expected = ready(&mut clean, 4096);
    let expected_work = clean.work_spent();
    drop(expected);
    drop(clean);
    let mut d = decoder(&b, false);
    push(&mut d, &b, &picture());
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(
        d.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    let paid = d.work_spent();
    assert!(paid > 0);
    for _ in 0..3 {
        let mut retry = WorkBudget::new(4096);
        assert!(matches!(
            d.poll(&mut retry, &AtomicBool::new(false)),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(
            (d.work_spent(), retry.spent(), b.used()),
            (paid, 0, b.limit())
        );
    }
    drop(blocker);
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        RecoveryStatus::Yield
    );
    assert_eq!(d.work_spent(), paid);
    let out = ready(&mut d, 1);
    assert_picture(&out, false);
    assert_eq!(out.raw_packet().as_ref(), picture());
    assert_eq!(d.work_spent(), expected_work);
}

#[test]
fn rejected_foreign_busy_and_oversized_input_preserve_the_exact_caller_owner() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let foreign = Budget::new(4096).unwrap();
    let c = AtomicBool::new(false);
    let mut d = decoder(&b, false);
    let held = Bytes::copy_from_slice(&foreign, &picture()).unwrap();
    let mut owned = Some(held.clone());
    assert_eq!(d.push(&mut owned, clock(), &c), Err(Error::ForeignPool));
    assert!(owned.as_ref().unwrap().shares_storage_with(&held));
    push(&mut d, &b, &picture());
    let held = Bytes::copy_from_slice(&b, &picture()).unwrap();
    let mut owned = Some(held.clone());
    assert!(matches!(
        d.push(&mut owned, clock(), &c),
        Err(Error::Invalid(_))
    ));
    assert!(owned.as_ref().unwrap().shares_storage_with(&held));
    drop(ready(&mut d, 4096));
    let mut limited = ReceiverDecoder::new(
        &b,
        DisplayLimits {
            max_packet_bytes: 1,
            ..DisplayLimits::default()
        },
        DisplayConfig::default(),
    )
    .unwrap();
    assert!(matches!(
        limited.push(&mut owned, clock(), &c),
        Err(Error::Limit { .. })
    ));
    assert!(owned.as_ref().unwrap().shares_storage_with(&held));
}

#[test]
fn cancellation_during_funding_is_sticky_releases_private_graph_and_reset_recovers() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let c = AtomicBool::new(false);
    let mut d = decoder(&b, false);
    push(&mut d, &b, &picture());
    assert_eq!(
        d.poll(&mut WorkBudget::new(1), &c).unwrap(),
        RecoveryStatus::Yield
    );
    assert_eq!(d.work_spent(), 1);
    c.store(true, Ordering::Relaxed);
    assert_eq!(d.poll(&mut WorkBudget::new(0), &c), Err(Error::Cancelled));
    assert_eq!(b.used(), baseline);
    c.store(false, Ordering::Relaxed);
    assert_eq!(
        d.poll(&mut WorkBudget::new(4096), &c),
        Err(Error::Cancelled)
    );
    d.reset(&c).unwrap();
    assert_eq!(d.work_spent(), 0);
    push(&mut d, &b, &picture());
    assert_picture(&ready(&mut d, 7), false);
}

#[test]
fn cancelled_push_preserves_input_and_cancelled_take_discards_unpublished_output() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let c = AtomicBool::new(true);
    let mut d = decoder(&b, false);
    let held = Bytes::copy_from_slice(&b, &picture()).unwrap();
    let mut owned = Some(held.clone());
    assert_eq!(d.push(&mut owned, clock(), &c), Err(Error::Cancelled));
    assert!(owned.as_ref().unwrap().shares_storage_with(&held));
    c.store(false, Ordering::Relaxed);
    d.reset(&c).unwrap();
    d.push(&mut owned, clock(), &c).unwrap();
    let mut ready = false;
    for _ in 0..1_000_000 {
        if d.poll(&mut WorkBudget::new(4096), &c).unwrap() == RecoveryStatus::OutputReady {
            ready = true;
            break;
        }
    }
    assert!(
        ready,
        "native output must become ready before cancelled take"
    );
    c.store(true, Ordering::Relaxed);
    assert!(matches!(d.take_output(&c), Err(Error::Cancelled)));
    drop(held);
    assert_eq!(b.used(), baseline);
}

#[test]
fn cumulative_work_limit_poison_cannot_be_bypassed_by_small_quanta_or_reset_retries() {
    for quantum in [1, 4096] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let baseline = b.used();
        let mut d = ReceiverDecoder::new(
            &b,
            DisplayLimits {
                max_work: 1,
                ..DisplayLimits::default()
            },
            DisplayConfig::default(),
        )
        .unwrap();
        push(&mut d, &b, &picture());
        let mut error = None;
        for _ in 0..4 {
            match d.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false)) {
                Err(value) => {
                    error = Some(value);
                    break;
                }
                Ok(status) => assert_eq!(status, RecoveryStatus::Yield),
            }
        }
        let error = error.expect("finite native work ceiling must refuse input");
        assert!(matches!(error, Error::Limit { .. }));
        assert_eq!((d.work_spent(), b.used()), (1, baseline));
        assert_eq!(
            d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Err(error)
        );
        d.reset(&AtomicBool::new(false)).unwrap();
        assert_eq!((d.work_spent(), b.used()), (0, baseline));
    }
}

#[test]
fn retained_native_outputs_survive_reset_and_decoder_drop_with_all_charges() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let mut d = decoder(&b, true);
    let mut outputs = Vec::new();
    for _ in 0..3 {
        push(&mut d, &b, &picture());
        outputs.push(ready(&mut d, 7));
        d.reset(&AtomicBool::new(false)).unwrap();
    }
    drop(d);
    assert!(b.used() > baseline);
    for out in &outputs {
        assert_picture(out, true);
        assert_eq!(out.raw_packet().as_ref(), picture());
    }
    let alias = outputs[0].receiver_output().unwrap().rectangles()[0]
        .pixels()
        .clone();
    drop(outputs);
    assert_eq!(alias.as_ref(), &[1, 2]);
    assert!(b.used() > baseline);
    drop(alias);
    assert_eq!(b.used(), baseline);
}

#[test]
fn native_eof_does_not_validate_pending_strict_source_state_or_fabricate_output() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let c = AtomicBool::new(false);
    let mut d = decoder(&b, false);
    let mut raw = picture();
    raw.extend_from_slice(&pcs(16, 1, 0, &[7])); // Deliberately no later END.
    push(&mut d, &b, &raw);
    assert_picture(&ready(&mut d, 1), false);
    d.finish_input(&c).unwrap();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &c).unwrap(),
        RecoveryStatus::Yield
    );
    let before = d.work_spent();
    let mut final_work = WorkBudget::new(1);
    assert_eq!(d.poll(&mut final_work, &c).unwrap(), RecoveryStatus::Yield);
    assert_eq!(final_work.spent(), 1);
    assert_eq!(d.work_spent(), before + 1);
    let mut zero = WorkBudget::new(0);
    assert_eq!(d.poll(&mut zero, &c).unwrap(), RecoveryStatus::Ended);
    assert_eq!(zero.spent(), 0);
    assert_eq!(d.work_spent(), before + 1);
    assert_eq!(d.eof_report(), Some(ReceiverEof::NoDelayedSubtitle));
    assert!(d.take_output(&c).is_err());
    let mut next = Some(Bytes::copy_from_slice(&b, &picture()).unwrap());
    let held = next.as_ref().unwrap().clone();
    assert!(d.push(&mut next, clock(), &c).is_err());
    assert!(next.as_ref().unwrap().shares_storage_with(&held));
}

#[test]
fn unknown_pts_retains_raw_dts_without_inventing_native_presentation_time() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = decoder(&b, false);
    let raw_clock = PacketClock {
        pts: None,
        ..clock()
    };
    let mut owned = Some(Bytes::copy_from_slice(&b, &picture()).unwrap());
    d.push(&mut owned, raw_clock, &AtomicBool::new(false))
        .unwrap();
    let out = ready(&mut d, 7);
    assert_eq!(out.clock, raw_clock);
    assert_eq!(out.receiver_output().unwrap().pts_microseconds, None);
}

// This is functional transcript reproduction, not a benchmark adapter. Recorded
// input/native expected bytes are read only by tests, never by production code.
