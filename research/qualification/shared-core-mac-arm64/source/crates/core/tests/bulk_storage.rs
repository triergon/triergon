use safe_media_core::{Budget, BudgetVec, Error};

#[test]
fn history_repetition_handles_overlap_and_retains_the_complete_charge() {
    let budget = Budget::new(4096).unwrap();
    let baseline = budget.used();
    let mut output = BudgetVec::new(&budget);
    output.try_extend_from_slice(b"abc").unwrap();
    output.try_extend_from_history(3, 31).unwrap();
    assert_eq!(output.as_slice(), b"abcabcabcabcabcabcabcabcabcabcabca");
    output.try_extend_from_history(1, 17).unwrap();
    assert_eq!(&output.as_slice()[34..], &[b'a'; 17]);
    let used = budget.used();
    assert_eq!(used, baseline + output.capacity());
    let bytes = output.freeze().unwrap();
    let tail = bytes.slice(34..51).unwrap();
    let charged = budget.used();
    drop(bytes);
    assert_eq!(tail.as_ref(), &[b'a'; 17]);
    assert_eq!(budget.used(), charged);
    drop(tail);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn rejected_history_growth_and_invalid_ranges_preserve_the_original_buffer() {
    let budget = Budget::new(256).unwrap();
    let mut output = BudgetVec::new(&budget);
    output.try_extend_from_slice(b"abc").unwrap();
    let capacity = output.capacity();
    let held = budget.reserve(budget.available()).unwrap();
    let used = budget.used();
    assert!(matches!(
        output.try_extend_from_history(3, 30),
        Err(Error::Backpressure { .. })
    ));
    for distance in [0, 4, usize::MAX] {
        assert!(matches!(
            output.try_extend_from_history(distance, 1),
            Err(Error::Invalid(_))
        ));
    }
    assert_eq!(
        output.try_extend_from_history(1, usize::MAX),
        Err(Error::Overflow)
    );
    output.try_extend_from_history(3, 0).unwrap();
    assert_eq!(output.as_slice(), b"abc");
    assert_eq!(output.capacity(), capacity);
    assert_eq!(budget.used(), used);
    drop(held);
    output.try_extend_from_history(3, 6).unwrap();
    assert_eq!(output.as_slice(), b"abcabcabc");
}

#[test]
fn initialized_resize_preserves_capacity_on_shrink_and_rolls_back_rejected_growth() {
    let budget = Budget::new(512).unwrap();
    let baseline = budget.used();
    let mut output = BudgetVec::new(&budget);
    output.try_resize(8, 7_u16).unwrap();
    assert_eq!(output.as_slice(), &[7; 8]);
    let capacity = output.capacity();
    let charged = budget.used();
    output.try_resize(2, 9).unwrap();
    assert_eq!(output.as_slice(), &[7, 7]);
    assert_eq!(output.capacity(), capacity);
    assert_eq!(budget.used(), charged);
    output.try_resize(8, 9).unwrap();
    assert_eq!(output.as_slice(), &[7, 7, 9, 9, 9, 9, 9, 9]);
    let held = budget.reserve(budget.available()).unwrap();
    let used = budget.used();
    assert!(matches!(
        output.try_resize(16, 0),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(output.as_slice(), &[7, 7, 9, 9, 9, 9, 9, 9]);
    assert_eq!(output.capacity(), capacity);
    assert_eq!(budget.used(), used);
    drop(held);
    drop(output);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn empty_reservations_and_repeated_growth_do_not_leak_or_exceed_the_pool() {
    let budget = Budget::new(1024).unwrap();
    let baseline = budget.used();
    for _ in 0..20 {
        let zero = budget.reserve(0).unwrap();
        let mut bytes = BudgetVec::new(&budget);
        bytes.try_resize(64, 0_u8).unwrap();
        bytes.try_resize(128, 1).unwrap();
        let owner = bytes.freeze().unwrap();
        assert!(budget.used() > baseline);
        drop(zero);
        drop(owner);
        assert_eq!(budget.used(), baseline);
    }
    assert!(budget.peak() <= budget.limit());
}
