//! Only original caller input can leave its pool, after all produced views drop.
use triergon_core::{Budget, Bytes, OwnedInputBytes};

fn input() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(96);
    bytes.extend(0_u8..31);
    bytes
}

#[test]
fn unique_input_returns_the_same_initialized_allocation_and_capacity() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let vector = input();
    let pointer = vector.as_ptr();
    let capacity = vector.capacity();
    let source = OwnedInputBytes::try_from_vec(&pool, vector).unwrap();
    assert_eq!(
        pool.used(),
        baseline + capacity + Bytes::owner_overhead_bytes()
    );
    let mut vector = source.try_into_vec().unwrap();
    assert_eq!((vector.as_ptr(), vector.capacity()), (pointer, capacity));
    assert_eq!(vector, (0_u8..31).collect::<Vec<_>>());
    assert_eq!(pool.used(), baseline);
    vector[0] = 99;
    assert_eq!(vector[0], 99);
}

#[test]
fn shared_rejection_returns_the_input_capability_then_recovers_after_alias_drop() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let vector = input();
    let pointer = vector.as_ptr();
    let source = OwnedInputBytes::try_from_vec(&pool, vector).unwrap();
    let alias = source.as_bytes().slice(4..9).unwrap();
    let charge = alias.retained_storage_bytes();
    let source = source.try_into_vec().unwrap_err();
    assert!(source.as_bytes().shares_storage_with(&alias));
    assert_eq!(alias.as_ref(), &[4, 5, 6, 7, 8]);
    assert_eq!(pool.used(), baseline + charge);
    drop(alias);
    let vector = source.try_into_vec().unwrap();
    assert_eq!(vector.as_ptr(), pointer);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn a_produced_partial_view_keeps_its_hidden_parent_charged_after_input_drop() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let source = OwnedInputBytes::try_from_vec(&pool, input()).unwrap();
    let view = source.as_bytes().slice(4..9).unwrap();
    let charge = view.retained_storage_bytes();
    let source = source.try_into_vec().unwrap_err();
    drop(source);
    assert_eq!(view.as_ref(), &[4, 5, 6, 7, 8]);
    assert_eq!(view.retained_storage_bytes(), charge);
    assert_eq!(pool.used(), baseline + charge);
    // There is intentionally no conversion from this produced Bytes view to
    // an input capability or an uncharged Vec (compile-fail docs enforce it).
    drop(view);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn a_full_range_produced_alias_also_blocks_original_input_recovery() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let source = OwnedInputBytes::try_from_vec(&pool, input()).unwrap();
    let alias = source.as_bytes().slice(0..source.as_ref().len()).unwrap();
    let source = source.try_into_vec().unwrap_err();
    drop(alias);
    assert_eq!(source.try_into_vec().unwrap().len(), 31);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn empty_input_keeps_its_real_capacity_when_transferred_back() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let vector = Vec::with_capacity(37);
    let pointer = vector.as_ptr();
    let capacity = vector.capacity();
    let source = OwnedInputBytes::try_from_vec(&pool, vector).unwrap();
    let vector = source.try_into_vec().unwrap();
    assert_eq!(
        (vector.as_ptr(), vector.len(), vector.capacity()),
        (pointer, 0, capacity)
    );
    assert_eq!(pool.used(), baseline);
}

#[test]
fn even_an_empty_produced_subview_retains_the_complete_original_charge() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let source = OwnedInputBytes::try_from_vec(&pool, input()).unwrap();
    let view = source.as_bytes().slice(4..4).unwrap();
    let charge = view.retained_storage_bytes();
    let source = source.try_into_vec().unwrap_err();
    drop(source);
    assert!(view.is_empty());
    assert_eq!(pool.used(), baseline + charge);
    drop(view);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn a_retained_packet_alias_cannot_strip_the_original_budget() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let source = OwnedInputBytes::try_from_vec(&pool, input()).unwrap();
    let alias = source.as_bytes().slice(2..7).unwrap();
    let charge = alias.retained_storage_bytes();
    let source = source.try_into_vec().unwrap_err();
    drop(source);
    assert_eq!(alias.as_ref(), &[2, 3, 4, 5, 6]);
    assert_eq!(pool.used(), baseline + charge);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn the_same_input_vector_crosses_fresh_operation_pools_without_copy_or_pool_cache() {
    let mut vector = input();
    let pointer = vector.as_ptr();
    let capacity = vector.capacity();
    for _ in 0..32 {
        let pool = Budget::new(4096).unwrap();
        let baseline = pool.used();
        let source = OwnedInputBytes::try_from_vec(&pool, vector).unwrap();
        vector = source.try_into_vec().unwrap();
        assert_eq!((vector.as_ptr(), vector.capacity()), (pointer, capacity));
        assert_eq!(pool.used(), baseline);
    }
    assert_eq!(vector, (0_u8..31).collect::<Vec<_>>());
}
