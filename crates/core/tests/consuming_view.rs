//! External-crate API guards for the existing checked consuming view.
use triergon_core::{Budget, Bytes, Error};

#[test]
fn consuming_view_needs_no_free_pool_bytes_and_keeps_complete_hidden_capacity() {
    let pool = Budget::new(16 * 1024).unwrap();
    let baseline = pool.used();
    let mut data = Vec::with_capacity(4096);
    data.extend_from_slice(b"prefixPAYLOADsuffix");
    let source = Bytes::try_from_vec(&pool, data).unwrap();
    let pointer = source[6..13].as_ptr();
    let capacity = source.retained_capacity();
    let charge = source.retained_storage_bytes();
    let blocker = pool.reserve(pool.available()).unwrap();
    let before = pool.used();
    let view = source.into_slice(6..13).unwrap();
    assert_eq!(view.as_ref(), b"PAYLOAD");
    assert_eq!(view.as_ptr(), pointer);
    assert_eq!(view.retained_capacity(), capacity);
    assert_eq!(view.retained_storage_bytes(), charge);
    assert!(view.belongs_to(&pool));
    assert_eq!(pool.used(), before);
    drop(blocker);
    assert_eq!(pool.used(), baseline + charge);
    drop(view);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn consuming_empty_end_view_retains_its_full_owner_after_other_aliases_drop() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let source = Bytes::copy_from_slice(&pool, b"complete backing owner").unwrap();
    let alias = source.clone();
    let charge = source.retained_storage_bytes();
    let len = source.len();
    let empty = source.into_slice(len..len).unwrap();
    assert!(empty.is_empty());
    assert!(empty.shares_storage_with(&alias));
    assert_eq!(empty.retained_storage_bytes(), charge);
    drop(alias);
    assert_eq!(pool.used(), baseline + charge);
    drop(empty);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn consuming_subview_checks_visible_range_and_keeps_invalid_attempts_from_harming_aliases() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let source = Bytes::copy_from_slice(&pool, b"0123456789abcdef").unwrap();
    let charge = source.retained_storage_bytes();
    let middle = source.slice(3..8).unwrap();
    for range in [
        0..6,
        std::ops::Range { start: 4, end: 3 },
        usize::MAX..usize::MAX,
    ] {
        assert!(matches!(
            middle.clone().into_slice(range),
            Err(Error::Invalid("byte span range"))
        ));
        assert_eq!(source.as_ref(), b"0123456789abcdef");
        assert_eq!(middle.as_ref(), b"34567");
        assert_eq!(pool.used(), baseline + charge);
    }
    let nested = middle.into_slice(1..4).unwrap();
    assert_eq!(nested.as_ref(), b"456");
    assert!(nested.shares_storage_with(&source));
    drop(source);
    assert_eq!(pool.used(), baseline + charge);
    drop(nested);
    assert_eq!(pool.used(), baseline);
}
