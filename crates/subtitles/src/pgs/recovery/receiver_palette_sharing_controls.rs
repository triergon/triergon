use super::*;
use triergon_core::TimeBase;

fn segment(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    out.extend_from_slice(body);
}

fn packet(state: u8, entries: &[u8], rle: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    segment(
        &mut out,
        0x16,
        &[
            0, 16, 0, 1, 0x10, 0, 1, state, 0, 0, 1, 0, 7, 0, 0, 0, 0, 0, 0,
        ],
    );
    let mut palette = vec![0, 0];
    palette.extend_from_slice(entries);
    segment(&mut out, 0x14, &palette);
    if let Some(rle) = rle {
        let length = u32::try_from(rle.len() + 4).unwrap().to_be_bytes();
        let mut object = vec![0, 7, 0, 0xc0, length[1], length[2], length[3], 0, 1, 0, 1];
        object.extend_from_slice(rle);
        segment(&mut out, 0x15, &object);
    }
    segment(&mut out, 0x80, &[]);
    out
}

fn clock() -> PacketClock {
    PacketClock {
        pts: Some(123),
        dts: None,
        timebase: TimeBase::new(1, 90_000).unwrap(),
    }
}

fn push(d: &mut IndexedRecoveryDecoder, b: &Budget, bytes: &[u8]) {
    let mut raw = Some(Bytes::copy_from_slice(b, bytes).unwrap());
    d.push(&mut raw, clock(), &AtomicBool::new(false)).unwrap();
    assert!(raw.is_none());
}

fn finish(d: &mut IndexedRecoveryDecoder, quantum: u64) -> IndexedRecoveredPacket {
    for _ in 0..100_000 {
        if d.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
            .unwrap()
            == RecoveryStatus::OutputReady
        {
            return d.take_output(&AtomicBool::new(false)).unwrap();
        }
    }
    panic!("bounded receiver palette completion");
}

fn store(b: &Budget, malformed: bool) -> IndexedRecoveryDecoder {
    let mut d =
        IndexedRecoveryDecoder::new(b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    let good = [1, 0, 0];
    let short = [1, 0, 0, 42];
    push(
        &mut d,
        b,
        &packet(
            0x80,
            &[1, 235, 128, 128, 255],
            Some(if malformed { &short[..] } else { &good[..] }),
        ),
    );
    for _ in 0..100_000 {
        if d.phase == Phase::StoreRect {
            return d;
        }
        d.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap();
    }
    panic!("bounded first receiver rectangle");
}

#[test]
fn complete_literal_palette_reuses_the_actual_source_owner_without_an_inline_kibibyte() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = store(&b, false);
    let out = finish(&mut d, 1);
    let source = out.source_sets()[0].objects()[0].palette();
    let rect = &out.receiver_sets()[0].rectangles()[0];
    assert!(std::ptr::eq(
        rect.palette().as_ptr(),
        source.receiver_words().as_ptr()
    ));
    assert_eq!(rect.palette().as_slice(), source.receiver_words());
    assert!(size_of::<RecoveredRect>() < 256);
    assert_eq!(&rect.pixels()[..], &[1]);
}

#[test]
fn retained_palette_is_immutable_after_later_pds_reset_and_producer_drop() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let mut d = store(&b, false);
    let first = finish(&mut d, 7);
    let white = first.receiver_sets()[0].rectangles()[0].palette()[1];
    push(&mut d, &b, &packet(0, &[1, 16, 128, 128, 255], None));
    let second = finish(&mut d, 31);
    let rect = &second.receiver_sets()[0].rectangles()[0];
    assert_ne!(rect.palette()[1], white);
    assert!(std::ptr::eq(
        rect.palette().as_ptr(),
        second.source_sets()[0].objects()[0]
            .palette()
            .receiver_words()
            .as_ptr()
    ));
    d.reset(&AtomicBool::new(false)).unwrap();
    drop(d);
    assert_eq!(first.receiver_sets()[0].rectangles()[0].palette()[1], white);
    assert_eq!(
        first.source_sets()[0].objects()[0]
            .palette()
            .source(1)
            .unwrap()
            .y,
        235
    );
    assert!(b.used() > baseline);
    drop(second);
    drop(first);
    assert_eq!(b.used(), baseline);
}

#[test]
fn unused_word255_difference_prevents_sharing_with_a_later_source_snapshot() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = IndexedRecoveryDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default())
        .unwrap();
    let mut raw = packet(0x80, &[1, 235, 128, 128, 255], Some(&[1, 0, 0]));
    raw.extend(packet(0, &[255, 235, 128, 128, 255], None));
    push(&mut d, &b, &raw);
    let out = finish(&mut d, 19);
    assert_eq!(out.receiver_sets().len(), 2);
    assert_eq!(out.source_sets().len(), 2);
    let first = &out.receiver_sets()[0].rectangles()[0];
    let last = out.source_sets()[1].objects()[0].palette();
    assert_eq!(first.palette()[255], 0);
    assert_ne!(last.receiver_argb(255), 0);
    assert!(!std::ptr::eq(
        first.palette().as_ptr(),
        last.receiver_words().as_ptr()
    ));
    assert_eq!(first.palette()[1], last.receiver_argb(1));
    assert!(std::ptr::eq(
        out.receiver_sets()[1].rectangles()[0].palette().as_ptr(),
        last.receiver_words().as_ptr()
    ));
}

#[test]
fn equal_epoch_carried_words_never_promote_missing_source_colours() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = store(&b, false);
    let first = finish(&mut d, 11);
    let white = first.receiver_sets()[0].rectangles()[0].palette()[1];
    push(&mut d, &b, &packet(0x80, &[], Some(&[1, 0, 0])));
    let out = finish(&mut d, 1);
    let source = out.source_sets()[0].objects()[0].palette();
    let rect = &out.receiver_sets()[0].rectangles()[0];
    assert!(source.source(1).is_none());
    assert!(source.source(255).is_none());
    assert_eq!(
        source.receiver_origin(1),
        crate::compat::pgs::ColourOrigin::PreviousEpochSlot
    );
    assert_eq!(rect.palette()[1], white);
    assert!(std::ptr::eq(
        rect.palette().as_ptr(),
        source.receiver_words().as_ptr()
    ));
}

#[test]
fn malformed_strict_rle_keeps_literal_receiver_words_and_an_independent_source_refusal() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = store(&b, true);
    let out = finish(&mut d, 1);
    assert!(out.source_error().is_some());
    assert!(out.source_sets().is_empty());
    let rect = &out.receiver_sets()[0].rectangles()[0];
    assert_eq!(
        rect.palette()[1],
        ColourMatrix::Sd601.argb(235, 128, 128, 255)
    );
    assert_eq!(rect.palette()[255], 0);
    assert_eq!(
        out.raw_packet().as_ref(),
        packet(0x80, &[1, 235, 128, 128, 255], Some(&[1, 0, 0, 42])).as_slice()
    );
}

#[test]
fn forced_only_zero_projection_cannot_alias_a_nonzero_source_palette() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = IndexedRecoveryDecoder::new(
        &b,
        DisplayLimits::default(),
        DisplayConfig {
            receiver_forced_only: true,
        },
    )
    .unwrap();
    push(
        &mut d,
        &b,
        &packet(0x80, &[1, 235, 128, 128, 255], Some(&[1, 0, 0])),
    );
    let out = finish(&mut d, 13);
    let source = out.source_sets()[0].objects()[0].palette();
    let rect = &out.receiver_sets()[0].rectangles()[0];
    assert!(rect.palette().iter().all(|word| *word == 0));
    assert_ne!(source.receiver_argb(1), 0);
    assert!(!std::ptr::eq(
        rect.palette().as_ptr(),
        source.receiver_words().as_ptr()
    ));
    assert!(source.source(1).is_some());
}

#[test]
fn zero_work_and_one_funded_store_preserve_the_same_work_quantum_under_pressure() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d = store(&b, false);
    let held = b.reserve(b.available()).unwrap();
    let spent = d.spent;
    let used = b.used();
    for _ in 0..5 {
        assert_eq!(
            d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(RecoveryStatus::Yield)
        );
        assert_eq!(d.spent, spent);
        assert_eq!(b.used(), used);
        assert_eq!(d.rect_index, 0);
        assert!(d.rect_next.is_some());
    }
    let mut work = WorkBudget::new(1);
    assert_eq!(
        d.poll(&mut work, &AtomicBool::new(false)),
        Ok(RecoveryStatus::Yield)
    );
    assert_eq!(work.spent(), 1);
    assert_eq!(d.spent, spent + 1);
    assert_eq!(d.rect_index, 1);
    assert_eq!(b.used(), used);
    drop(held);
    let out = finish(&mut d, 1);
    assert_eq!(out.source_error(), None);
}

#[test]
fn fallback_pressure_retry_preserves_paid_credit_and_its_actual_retained_charge() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let mut d = store(&b, true);
    let held = b.reserve(b.available()).unwrap();
    let spent = d.spent;
    assert!(matches!(
        d.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Backpressure {
            requested: 1024,
            ..
        })
    ));
    assert_eq!(d.spent, spent + 1);
    assert_eq!(d.credit, 1);
    assert!(d.rect_next.is_some());
    let used = b.used();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(RecoveryStatus::Yield)
    );
    assert_eq!(d.spent, spent + 1);
    assert_eq!(b.used(), used);
    drop(held);
    let out = finish(&mut d, 1);
    let words = *out.receiver_sets()[0].rectangles()[0].palette();
    drop(d);
    assert!(b.used() >= baseline + 1024);
    assert_eq!(out.receiver_sets()[0].rectangles()[0].palette(), &words);
    drop(out);
    assert_eq!(b.used(), baseline);
}

#[test]
fn foreign_packet_and_cancel_preserve_caller_bytes_and_held_palette_owners() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let foreign = Budget::new(4096).unwrap();
    let mut d = IndexedRecoveryDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default())
        .unwrap();
    let bytes = packet(0x80, &[1, 235, 128, 128, 255], Some(&[1, 0, 0]));
    let mut raw = Some(Bytes::copy_from_slice(&foreign, &bytes).unwrap());
    assert_eq!(
        d.push(&mut raw, clock(), &AtomicBool::new(false)),
        Err(Error::ForeignPool)
    );
    assert_eq!(&raw.as_ref().unwrap()[..], bytes.as_slice());
    assert_eq!(d.spent, 0);
    drop(d);
    let baseline = b.used();
    let mut d = store(&b, false);
    let palette = d.role.source_sets.as_ref().unwrap().as_slice()[0].objects()[0]
        .palette()
        .clone();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    assert!(d.rect_next.is_none());
    drop(d);
    assert_eq!(palette.source(1).unwrap().y, 235);
    assert!(b.used() > baseline);
    drop(palette);
    assert_eq!(b.used(), baseline);
}
