use triergon_core::{Budget, BudgetVec, Charged, Error};

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

#[test]
fn reservation_resizes_in_both_directions_and_keeps_its_charge_on_refusal() {
    let budget = Budget::new(1024).unwrap();
    let baseline = budget.used();
    let mut charge = budget.reserve(100).unwrap();
    charge.resize_to(300).unwrap();
    assert_eq!((charge.bytes(), budget.used()), (300, baseline + 300));
    charge.resize_to(40).unwrap();
    assert_eq!((charge.bytes(), budget.used()), (40, baseline + 40));
    let maximum = budget.limit() - Budget::control_storage_bytes();
    assert_eq!(
        charge.resize_to(usize::MAX),
        Err(Error::Limit {
            requested: usize::MAX - 40,
            maximum,
        })
    );
    assert_eq!(charge.bytes(), 40);
    let held = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        charge.resize_to(41),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!((charge.bytes(), budget.used()), (40, budget.limit()));
    drop(held);
    drop(charge);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn charged_vectors_read_and_write_as_slices_and_truncate_without_releasing() {
    let budget = Budget::new(4096).unwrap();
    let mut samples = BudgetVec::new(&budget);
    samples.try_resize(8, [0i16; 2]).unwrap();
    samples[3] = [1, -1];
    samples[5..].fill([2, 2]);
    assert_eq!(samples.iter().filter(|s| s[0] != 0).count(), 4);
    let used = budget.used();
    samples.truncate(4);
    assert_eq!(&samples[..], &[[0, 0], [0, 0], [0, 0], [1, -1]]);
    assert_eq!(budget.used(), used);
}

#[test]
fn filled_and_cloned_vectors_charge_their_capacity_and_refuse_without_charging() {
    let budget = Budget::new(4096).unwrap();
    let idle = budget.used();
    let zeros = BudgetVec::try_filled(&budget, 0u32, 256).unwrap();
    assert_eq!(budget.used(), idle + 1024);
    let copy = zeros.try_clone().unwrap();
    assert_eq!((&copy[..], budget.used()), (&zeros[..], idle + 2048));
    assert!(matches!(
        BudgetVec::try_filled(&budget, 7u8, 4096),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(
        BudgetVec::try_filled(&budget, 0u64, usize::MAX),
        Err(Error::Overflow)
    ));
    assert_eq!(budget.used(), idle + 2048);
    drop((zeros, copy));
    assert_eq!(budget.used(), idle);
}

#[test]
fn charged_values_hold_their_charge_until_they_drop() {
    let budget = Budget::new(4096).unwrap();
    let idle = budget.used();
    let mut state = Charged::try_new(&budget, 1000, || [0u8; 16]).unwrap();
    state[3] = 9;
    assert_eq!(
        (state[3], state.charged_bytes(), budget.used()),
        (9, 1000, idle + 1000)
    );
    assert!(Charged::try_new(&budget, 4096, || ()).is_err());
    drop(state);
    assert_eq!(budget.used(), idle);
}

#[test]
fn boxed_values_charge_their_size() {
    let budget = Budget::new(4096).unwrap();
    let idle = budget.used();
    let state = Charged::boxed(&budget, [7u32; 64]).unwrap();
    assert_eq!((state[63], budget.used()), (7, idle + 256));
    assert!(Charged::boxed(&budget, [0u8; 8192]).is_err());
    drop(state);
    assert_eq!(budget.used(), idle);
}
