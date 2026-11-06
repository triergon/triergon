use super::*;
use crate::pgs::RasterLimits;
use triergon_core::TimeBase;

fn packet() -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, body) in [
        (
            0x16,
            &[
                0x10, 0, 0, 1, 0x10, 0, 1, 0x80, 0, 0, 1, 0, 7, 0, 0, 0, 0, 0, 0,
            ][..],
        ),
        (
            0x14,
            &[0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 128][..],
        ),
        (
            0x15,
            &[0, 7, 0, 0xc0, 0, 0, 10, 0x10, 0, 0, 1, 0, 0xd0, 0, 1, 0, 0][..],
        ),
        (0x80, &[][..]),
    ] {
        out.push(kind);
        out.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
        out.extend_from_slice(body);
    }
    out
}

fn compare(b: &Budget) -> IndexedRecoveryDecoder {
    let mut d =
        IndexedRecoveryDecoder::new(b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    let mut raw = Some(Bytes::copy_from_slice(b, &packet()).unwrap());
    d.push(
        &mut raw,
        PacketClock {
            pts: None,
            dts: None,
            timebase: TimeBase::new(1, 90_000).unwrap(),
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    for _ in 0..100_000 {
        if d.phase == Phase::ShareCompare {
            return d;
        }
        d.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap();
    }
    panic!("literal-compare progress bound");
}

#[test]
fn comparison_zero_work_never_reads_advances_allocates_or_recharges() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = compare(&b);
    let spent = d.spent;
    let used = b.used();
    for _ in 0..5 {
        assert_eq!(
            d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(RecoveryStatus::Yield)
        );
        assert_eq!(d.share_at, 0);
        assert_eq!(d.spent, spent);
        assert_eq!(b.used(), used);
    }
}

#[test]
fn comparison_one_unit_visits_exactly_one_actual_encoded_byte() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = compare(&b);
    let length = d.objects.as_slice()[d.slot].encoded.len();
    for at in 0..length {
        let spent = d.spent;
        let mut work = WorkBudget::new(1);
        d.poll(&mut work, &AtomicBool::new(false)).unwrap();
        assert_eq!(d.share_at, at + 1);
        assert_eq!(d.spent, spent + 1);
        assert_eq!(work.spent(), 1);
        assert!(d.rect_next.is_none());
    }
}

#[test]
fn cancellation_releases_unpublished_evidence_but_not_held_pixel_owner() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let mut d = compare(&b);
    let held = d.share_candidate.as_ref().unwrap().1.clone();
    let cancel = AtomicBool::new(true);
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    assert!(d.share_candidate.is_none());
    assert!(held.iter().all(|v| *v == 1));
    drop(d);
    assert!(b.used() > baseline);
    drop(held);
    assert_eq!(b.used(), baseline);
}

#[test]
fn dimension_mismatch_cannot_obtain_validated_source_evidence() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let d = compare(&b);
    let source = d.role.strict.as_ref().unwrap();
    assert!(source.validated_indices(7, 4096, 1).is_some());
    assert!(source.validated_indices(7, 4095, 1).is_none());
    assert!(source.validated_indices(7, 4096, 2).is_none());
    assert!(source.validated_indices(8, 4096, 1).is_none());
}

#[test]
fn receiver_grammar_completion_does_not_grant_strict_evidence() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let raw = Bytes::copy_from_slice(&b, &[0, 0xd0, 0, 1, 0, 0]).unwrap();
    let mut strict = RasterJob::new(raw.clone(), 4096, 1, &b, RasterLimits::default()).unwrap();
    let mut receiver =
        RasterJob::native_receiver(raw, 4096, 1, &b, RasterLimits::default()).unwrap();
    assert!(strict.strict_validated_encoded().is_none());
    for _ in 0..100_000 {
        if strict
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == RasterStatus::Ready
        {
            break;
        }
    }
    for _ in 0..100_000 {
        if receiver
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == RasterStatus::Ready
        {
            break;
        }
    }
    assert!(strict.strict_validated_encoded().is_some());
    assert!(receiver.strict_validated_encoded().is_none());
}

#[test]
fn first_actual_native_slots_and_source_list_do_not_reserve_four() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = compare(&b);
    for _ in 0..100_000 {
        if d.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap()
            == RecoveryStatus::OutputReady
        {
            break;
        }
    }
    let out = d.take_output(&AtomicBool::new(false)).unwrap();
    assert_eq!(d.palettes.len(), 1);
    assert_eq!(d.palettes.capacity(), 1);
    assert_eq!(d.objects.capacity(), 1);
    assert_eq!(out.source_sets.capacity(), 1);
    assert_eq!(out.source_sets()[0].segments.capacity(), 4);
}
