use triergon_core::{Budget, BudgetVec, Error};

#[test]
fn reserved_single_copy_and_exact_growth_fallback_preserve_budget_and_values() {
    let budget = Budget::new(4096).unwrap();
    let baseline = budget.used();
    {
        let mut bytes: BudgetVec<u8> = BudgetVec::new(&budget);
        bytes.try_reserve_exact(3).unwrap();
        let capacity = bytes.capacity();
        assert!(capacity >= 3);
        let mut expected = vec![11; capacity - 2];
        bytes.try_extend_from_slice(&expected).unwrap();
        let pointer = bytes.as_slice().as_ptr();
        let blocker = budget.reserve(budget.available()).unwrap();
        let held = budget.used();
        bytes.try_extend_from_slice(&[22]).unwrap();
        expected.push(22);
        bytes.try_extend_from_slice(&[]).unwrap();
        assert_eq!(bytes.as_slice(), expected);
        assert!(matches!(
            bytes.try_extend_from_slice(&[33, 44]),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(bytes.as_slice(), expected);
        bytes.try_extend_from_slice(&[33]).unwrap();
        expected.push(33);
        assert_eq!(bytes.as_slice(), expected);
        assert!(matches!(
            bytes.try_extend_from_slice(&[44]),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(bytes.as_slice().as_ptr(), pointer);
        assert_eq!(bytes.capacity(), capacity);
        assert_eq!(budget.used(), held);
        drop(blocker);
        bytes.try_extend_from_slice(&[44]).unwrap();
        expected.push(44);
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(budget.used(), baseline + bytes.capacity());
        let capacity = bytes.capacity();
        assert_eq!(bytes.try_reserve_exact(usize::MAX), Err(Error::Overflow));
        assert_eq!(bytes.capacity(), capacity);
        assert_eq!(bytes.as_slice(), expected);
        let mut wide = BudgetVec::new(&budget);
        wide.try_reserve_exact(2).unwrap();
        wide.try_extend_from_slice(&[u128::MAX]).unwrap();
        wide.try_extend_from_slice(&[1_u128 << 127]).unwrap();
        assert_eq!(wide.as_slice(), &[u128::MAX, 1_u128 << 127]);
    }
    assert_eq!(budget.used(), baseline);
}
