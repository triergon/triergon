use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{HdLimits, HdPolicy, HdSpuJob, HdStatus};
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
