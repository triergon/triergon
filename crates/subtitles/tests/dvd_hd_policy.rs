use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{HdAlphaOrigin, HdLimits, HdPolicy, HdSpuJob, HdStatus};
fn decode(raw: &[u8], policy: HdPolicy, q: u64) -> triergon_subtitles::dvd::HdSubpicture {
    let b = Budget::new(1024 * 1024).unwrap();
    let c = AtomicBool::new(false);
    let mut j = HdSpuJob::with_policy(
        Bytes::copy_from_slice(&b, raw).unwrap(),
        &b,
        HdLimits::default(),
        policy,
    )
    .unwrap();
    for _ in 0..100000 {
        let mut work = WorkBudget::new(q);
        if j.poll(&mut work, &c).unwrap() == HdStatus::Ready {
            return j.try_finish(&c).unwrap();
        }
        assert!(work.spent() <= q);
    }
    panic!("bounded source case did not finish")
}
#[test]
fn four_entry_rle_under_wide_headers_preserves_exact_source_plane_and_words() {
    let raw = include_bytes!("hd-source-fixtures/hd-header-two-bit-pixels.spu");
    for q in 1..=32 {
        let p = decode(raw, HdPolicy::Strict, q);
        assert!(!p.facts().eight_bit);
        assert_eq!(p.bitmap().pixels().as_ref(), [0, 1, 2, 3, 3, 2, 1, 0]);
        assert_eq!(p.facts().palette4_word, Some(0x3210));
        assert_eq!(p.facts().alpha4_word, Some(0xffff));
        assert_eq!(
            p.receiver_four_palette(None).unwrap().argb,
            [0xff000000, 0xff545454, 0xffa9a9a9, 0xfffefefe]
        );
    }
}
#[test]
fn source_alpha_nibbles_and_contrast_bytes_have_explicit_order_and_origin() {
    let p = decode(
        include_bytes!("hd-policy-fixtures/four-84-then-04.spu"),
        HdPolicy::Strict,
        1,
    );
    let palette = p.receiver_four_palette(None).unwrap();
    assert_eq!(palette.effective_alpha, [2, 4, 8, 15]);
    assert_eq!(palette.argb.map(|v| v >> 24), [34, 68, 136, 255]);
    assert!(
        palette
            .alpha_origins
            .iter()
            .all(|v| matches!(v, HdAlphaOrigin::Packed { word: 0xf842, .. }))
    );
    assert!(palette.source_rgb.is_none());
    let p = decode(
        include_bytes!("hd-policy-fixtures/four-04-then-84.spu"),
        HdPolicy::Strict,
        1,
    );
    let palette = p.receiver_four_palette(None).unwrap();
    assert_eq!(palette.effective_alpha, [0, 16, 30, 255]);
    assert_eq!(palette.argb.map(|v| v >> 24), [0, 16, 254, 239]);
    assert!(
        palette
            .alpha_origins
            .iter()
            .all(|v| matches!(v, HdAlphaOrigin::Contrast { .. }))
    );
}
#[test]
fn eight_entry_overrides_use_raw_nibbles_and_later_contrast_resets_all_entries() {
    let c = AtomicBool::new(false);
    for (raw, alpha) in [
        (
            include_bytes!("hd-policy-fixtures/eight-84-then-04.spu").as_slice(),
            [2, 4, 8, 15],
        ),
        (
            include_bytes!("hd-policy-fixtures/eight-04-then-84.spu").as_slice(),
            [0, 1, 2, 3],
        ),
    ] {
        let p = decode(raw, HdPolicy::Strict, 1);
        let mut job = p.receiver_palette_job().unwrap();
        while job.poll(&mut WorkBudget::new(1), &c).unwrap() != HdStatus::Ready {}
        let words = job.try_finish(&c).unwrap();
        assert_eq!(
            words[..4].iter().map(|v| v >> 24).collect::<Vec<_>>(),
            alpha
        );
        assert_eq!(words[255] >> 24, 255);
        assert!(p.contrast().is_some());
    }
}
#[test]
fn absent_contrast_is_never_fabricated_into_source_facts() {
    let p = decode(
        include_bytes!("hd-policy-fixtures/eight-missing-contrast-forced.spu"),
        HdPolicy::Strict,
        1,
    );
    assert!(p.contrast().is_none());
    assert!(p.facts().contrast.is_none());
    assert!(matches!(p.palette_job(), Err(Error::Unsupported(_))));
    let c = AtomicBool::new(false);
    let mut receiver = p.receiver_palette_job().unwrap();
    while receiver.poll(&mut WorkBudget::new(7), &c).unwrap() != HdStatus::Ready {}
    let words = receiver.try_finish(&c).unwrap();
    assert!(words.iter().all(|v| v >> 24 == 0));
    assert!(words.iter().any(|v| v & 0xffffff != 0));
}
#[test]
fn forced_date_and_ordinary_start_date_remain_separate() {
    let p = decode(
        include_bytes!("hd-policy-fixtures/forced-date-17.spu"),
        HdPolicy::Strict,
        1,
    );
    assert_eq!(p.facts().forced_date.unwrap().0, 17);
    assert!(p.facts().start_date.is_none());
    assert!(p.facts().forced);
    assert!(p.facts().menu);
    let p = decode(
        include_bytes!("hd-policy-fixtures/ordinary-date-17.spu"),
        HdPolicy::Strict,
        1,
    );
    assert!(p.facts().forced_date.is_none());
    assert_eq!(p.facts().start_date.unwrap().0, 17);
    assert!(!p.facts().forced);
}
#[test]
fn native_opaque_prefix_retains_exact_tail_and_original_bytes() {
    let raw = include_bytes!("hd-source-fixtures/hd8-unknown-07-after-fields.spu");
    let p = decode(raw, HdPolicy::Receiver, 1);
    let span = p.facts().last_opaque_tail.unwrap();
    assert_eq!(span.get(p.packet()).unwrap()[0], 7);
    assert_eq!(p.packet().as_ref(), raw);
    assert_eq!(p.bitmap().pixels().len(), 36);
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = HdSpuJob::with_policy(
        Bytes::copy_from_slice(&b, raw).unwrap(),
        &b,
        HdLimits::default(),
        HdPolicy::Strict,
    )
    .unwrap();
    assert_eq!(
        j.poll(&mut WorkBudget::new(100000), &c).unwrap_err(),
        Error::Unsupported("HD opaque control")
    );
}
#[test]
fn native_stop_at_bad_link_does_not_invent_a_repaired_source_offset() {
    let raw = include_bytes!("hd-policy-fixtures/invalid-next-link-after-fields.spu");
    let p = decode(raw, HdPolicy::Receiver, 1);
    let span = p.facts().stopped_at_link.unwrap();
    assert_eq!(span.get(p.packet()).unwrap(), [255; 4]);
    assert!(p.facts().stop_date.is_none());
    assert_eq!(p.packet().as_ref(), raw);
}
#[test]
fn four_entry_pressure_zero_work_freeze_cancel_and_retained_output() {
    let raw = include_bytes!("hd-source-fixtures/hd-header-two-bit-pixels.spu");
    let b = Budget::new(100000).unwrap();
    let c = AtomicBool::new(false);
    let mut j = HdSpuJob::with_policy(
        Bytes::copy_from_slice(&b, raw).unwrap(),
        &b,
        HdLimits::default(),
        HdPolicy::Strict,
    )
    .unwrap();
    let before = b.used();
    assert_eq!(
        j.poll(&mut WorkBudget::new(0), &c).unwrap(),
        HdStatus::Yield
    );
    assert_eq!(b.used(), before);
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(
        j.poll(&mut WorkBudget::new(100000), &c),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    while j.poll(&mut WorkBudget::new(1), &c).unwrap() != HdStatus::Ready {}
    let spent = j.work_spent();
    let blocker = b.reserve(b.available()).unwrap();
    assert!(matches!(j.try_finish(&c), Err(Error::Backpressure { .. })));
    drop(blocker);
    let p = j.try_finish(&c).unwrap();
    assert_eq!(j.work_spent(), spent);
    let held = p.bitmap().pixels().clone();
    drop(p);
    c.store(true, Ordering::Relaxed);
    assert_eq!(
        j.poll(&mut WorkBudget::new(1), &c).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(held.as_ref(), [0, 1, 2, 3, 3, 2, 1, 0]);
}
#[test]
fn public_metadata_mutation_raw_getter_never_panics_and_keeps_valid_source_access() {
    use triergon_subtitles::dvd::{HdPacket, HdSpan};
    let raw = [0, 10, 0, 4, 0, 0, 0, 4, 0xff, 0];
    let original = HdPacket::parse(&raw)
        .unwrap()
        .controls(1, 1)
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(original.raw().unwrap(), &raw[4..]);
    for span in [
        HdSpan {
            start: usize::MAX,
            end: 0,
        },
        HdSpan {
            start: 0,
            end: usize::MAX,
        },
        HdSpan { start: 8, end: 4 },
    ] {
        let mut control = original;
        control.span = span;
        let result = std::panic::catch_unwind(|| control.raw());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Err(Error::Truncated));
    }
}
