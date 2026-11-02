use safe_media_core::{Budget, BudgetVec, Bytes, Error, WorkBudget};

#[test]
fn capacity_and_hidden_prefixes_remain_charged_until_the_last_view_drops() {
    let budget = Budget::new(4096).unwrap();
    let baseline = budget.used();
    let mut data = Vec::with_capacity(1024);
    data.extend_from_slice(b"original bytes");
    let original = Bytes::try_from_vec(&budget, data).unwrap();
    let charged = budget.used();
    assert!(charged >= baseline + 1024);
    let tail = original.slice(9..14).unwrap();
    assert_eq!(tail.as_ref(), b"bytes");
    assert_eq!(tail.retained_capacity(), 1024);
    assert!(tail.shares_storage_with(&original));
    let clone = tail.clone();
    assert_eq!(budget.used(), charged);
    drop(original);
    drop(tail);
    assert_eq!(clone.as_ref(), b"bytes");
    assert_eq!(budget.used(), charged);
    drop(clone);
    assert_eq!(budget.used(), baseline);
    assert_eq!(budget.peak(), charged);
}

#[test]
fn temporary_backpressure_recovers_after_caller_retention_ends() {
    let budget = Budget::new(4096).unwrap();
    let first = Bytes::copy_from_slice(&budget, &[1; 3000]).unwrap();
    let used = budget.used();
    assert!(matches!(
        Bytes::copy_from_slice(&budget, &[2; 1000]),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(budget.used(), used);
    drop(first);
    assert_eq!(
        Bytes::copy_from_slice(&budget, &[2; 1000]).unwrap().len(),
        1000
    );
    assert!(matches!(
        Bytes::copy_from_slice(&budget, &[0; 8192]),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn rejected_transfer_returns_the_original_allocation_without_a_charge() {
    let budget = Budget::new(256).unwrap();
    let baseline = budget.used();
    let mut input = Vec::with_capacity(1024);
    input.extend_from_slice(b"keep this input");
    let error = Bytes::try_from_vec(&budget, input).unwrap_err();
    assert!(matches!(error.error(), Error::Limit { .. }));
    let input = error.into_vec();
    assert_eq!(input, b"keep this input");
    assert_eq!(input.capacity(), 1024);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn invalid_ranges_and_failed_reservations_do_not_change_live_usage() {
    let budget = Budget::new(4096).unwrap();
    let bytes = Bytes::copy_from_slice(&budget, b"source").unwrap();
    let used = budget.used();
    assert!(bytes.slice(7..8).is_err());
    let start = 4;
    let end = 2;
    assert!(bytes.slice(start..end).is_err());
    assert!(bytes.slice(0..usize::MAX).is_err());
    assert_eq!(budget.used(), used);
    assert!(matches!(
        budget.reserve(usize::MAX),
        Err(Error::Limit { .. })
    ));
    assert_eq!(budget.used(), used);
}

#[test]
fn storage_outlives_the_reader_and_can_be_released_on_another_thread() {
    let owner = Budget::new(4096).unwrap();
    let witness = owner.clone();
    let baseline = owner.used();
    let bytes = Bytes::copy_from_slice(&owner, b"still owned").unwrap();
    drop(owner);
    let charged = witness.used();
    std::thread::scope(|scope| {
        let mut threads = Vec::new();
        for _ in 0..4 {
            let copy = bytes.clone();
            threads.push(scope.spawn(move || {
                for _ in 0..100 {
                    assert_eq!(copy.slice(6..11).unwrap().as_ref(), b"owned");
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
    });
    assert_eq!(witness.used(), charged);
    drop(bytes);
    assert_eq!(witness.used(), baseline);
}

#[test]
fn vector_growth_and_workspace_stay_in_the_same_live_pool() {
    let budget = Budget::new(4096).unwrap();
    let baseline = budget.used();
    let mut values = BudgetVec::new(&budget);
    for value in 0_u64..40 {
        values.try_push(value).unwrap();
    }
    assert_eq!(values.as_slice(), &(0..40).collect::<Vec<_>>());
    assert!(budget.used() >= baseline + values.capacity() * 8);
    let before = values.as_slice().to_vec();
    let used = budget.used();
    assert!(values.try_reserve_exact(usize::MAX).is_err());
    assert_eq!(values.as_slice(), before);
    assert_eq!(budget.used(), used);
    drop(values);
    assert_eq!(budget.used(), baseline);
    assert!(budget.peak() <= budget.limit());
}

#[test]
fn vector_backpressure_preserves_the_rejected_value_and_existing_contents() {
    let budget = Budget::new(1024).unwrap();
    let held = Bytes::copy_from_slice(&budget, &[0; 700]).unwrap();
    let mut values = BudgetVec::new(&budget);
    let value = [9_u8; 300];
    let error = values.try_push(value).unwrap_err();
    assert!(matches!(error.error, Error::Backpressure { .. }));
    assert_eq!(error.value, value);
    assert!(values.is_empty());
    drop(held);
    values.try_push(error.value).unwrap();
    assert_eq!(values.as_slice(), &[value]);
}

#[test]
fn work_exhaustion_yields_without_spending_unavailable_units() {
    let mut work = WorkBudget::new(5);
    assert_eq!(work.take(3), 3);
    assert_eq!(work.remaining(), 2);
    assert!(!work.try_charge(3));
    assert_eq!(work.remaining(), 2);
    assert!(work.try_charge(2));
    assert_eq!(work.take(1), 0);
    assert_eq!(work.spent(), 5);
}

#[test]
fn freeze_preserves_allocation_and_returns_builder_when_a_header_cannot_fit() {
    let budget = Budget::new(2048).unwrap();
    let baseline = budget.used();
    let mut buffer = BudgetVec::new(&budget);
    buffer
        .try_extend_from_slice(b"immutable after freeze")
        .unwrap();
    let pointer = buffer.as_slice().as_ptr();
    let held = budget.reserve(budget.available()).unwrap();
    let used = budget.used();
    let rejected = buffer.freeze().unwrap_err();
    assert!(matches!(rejected.error, Error::Backpressure { .. }));
    assert_eq!(rejected.buffer.as_slice(), b"immutable after freeze");
    assert_eq!(rejected.buffer.as_slice().as_ptr(), pointer);
    assert_eq!(budget.used(), used);
    drop(held);
    let bytes = rejected.buffer.freeze().unwrap();
    assert_eq!(bytes.as_ref().as_ptr(), pointer);
    let alias = bytes.slice(10..bytes.len()).unwrap();
    drop(bytes);
    assert_eq!(alias.as_ref(), b"after freeze");
    assert!(budget.used() > baseline);
    drop(alias);
    assert_eq!(budget.used(), baseline);
}
