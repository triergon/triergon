use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Bytes, InputCursor, InputWindow, SourceCapabilities, SourceId, Step,
    WorkBudget,
};

#[test]
fn immutable_footprints_report_the_whole_owner_through_empty_and_nested_views() {
    let pool = Budget::new(4096).unwrap();
    assert_eq!(pool.used(), Budget::control_storage_bytes());
    let mut raw = Vec::with_capacity(257);
    raw.extend_from_slice(b"source");
    let capacity = raw.capacity();
    let bytes = Bytes::try_from_vec(&pool, raw).unwrap();
    let retained = capacity + Bytes::owner_overhead_bytes();
    assert_eq!(bytes.retained_storage_bytes(), retained);
    assert_eq!(pool.used(), Budget::control_storage_bytes() + retained);
    let inner = bytes.slice(1..4).unwrap();
    let empty = inner.slice(2..2).unwrap();
    assert!(bytes.shares_storage_with(&empty));
    for view in [&bytes, &inner, &empty] {
        assert_eq!(view.retained_storage_bytes(), retained);
    }
    // Other live allocations do not change this owner's immutable footprint.
    let unrelated = pool.reserve(1000).unwrap();
    assert_eq!(empty.retained_storage_bytes(), retained);
    drop((bytes, inner, unrelated));
    assert_eq!(pool.used(), Budget::control_storage_bytes() + retained);
    drop(empty);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn copy_and_freeze_charge_the_same_header_including_an_empty_owner() {
    let pool = Budget::new(4096).unwrap();
    let empty = Bytes::copy_from_slice(&pool, b"").unwrap();
    assert_eq!(empty.retained_capacity(), 0);
    assert_eq!(
        empty.retained_storage_bytes(),
        Bytes::owner_overhead_bytes()
    );
    let mut mutable = BudgetVec::new(&pool);
    mutable.try_extend_from_slice(b"payload").unwrap();
    let capacity = mutable.capacity();
    let frozen = mutable.freeze().unwrap();
    let copied = Bytes::copy_from_slice(&pool, b"payload").unwrap();
    assert_eq!(frozen.retained_capacity(), capacity);
    assert_eq!(
        frozen.retained_storage_bytes(),
        capacity + Bytes::owner_overhead_bytes()
    );
    assert_eq!(
        copied.retained_storage_bytes(),
        copied.retained_capacity() + Bytes::owner_overhead_bytes()
    );
    assert!(!copied.shares_storage_with(&frozen));
    assert_eq!(
        pool.used(),
        Budget::control_storage_bytes()
            + empty.retained_storage_bytes()
            + frozen.retained_storage_bytes()
            + copied.retained_storage_bytes()
    );
    drop((empty, frozen, copied));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

fn input(pool: &Budget) -> InputCursor {
    InputCursor::new(
        pool,
        SourceId(7),
        SourceCapabilities {
            length: Some(4),
            random_access: true,
        },
        64,
    )
    .unwrap()
}

#[test]
fn retained_input_handle_deduplicates_spans_and_survives_full_consumption_and_reset() {
    let pool = Budget::new(4096).unwrap();
    let mut cursor = input(&pool);
    assert!(cursor.retained_window().is_none());
    let mut raw = Vec::with_capacity(511);
    raw.extend_from_slice(b"abcd");
    let bytes = Bytes::try_from_vec(&pool, raw).unwrap();
    let footprint = bytes.retained_storage_bytes();
    let request = cursor.request();
    let supplied = InputWindow::new(request.source, request.token, request.offset, bytes).unwrap();
    cursor.provide(&supplied).unwrap();
    drop(supplied);
    let before = pool.used();
    let retained = cursor.retained_window().unwrap();
    assert_eq!(pool.used(), before);
    assert_eq!(retained.retained_storage_bytes(), footprint);
    let span = match cursor
        .read_span(2, &mut WorkBudget::new(2), &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(span) => span,
        other => panic!("{other:?}"),
    };
    assert_eq!(span.as_ref(), b"ab");
    assert!(retained.shares_storage_with(&span));
    assert!(retained.shares_storage_with(&cursor.retained_window().unwrap()));
    let mut last = [0; 2];
    assert!(matches!(
        cursor
            .read_into(&mut last, &mut WorkBudget::new(2), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(&last, b"cd");
    assert!(cursor.retained_window().is_none());
    cursor.reset(0).unwrap();
    drop((span, cursor));
    assert_eq!(retained.as_ref(), b"abcd");
    assert_eq!(pool.used(), Budget::control_storage_bytes() + footprint);
    drop(retained);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn cancellation_releases_cursor_ownership_but_preserves_an_explicit_retained_handle() {
    let pool = Budget::new(4096).unwrap();
    let mut cursor = input(&pool);
    let request = cursor.request();
    let bytes = Bytes::copy_from_slice(&pool, b"abcd").unwrap();
    let window = InputWindow::new(request.source, request.token, request.offset, bytes).unwrap();
    cursor.provide(&window).unwrap();
    drop(window);
    let retained = cursor.retained_window().unwrap();
    assert!(
        cursor
            .read_span(1, &mut WorkBudget::new(1), &AtomicBool::new(true))
            .is_err()
    );
    assert!(cursor.retained_window().is_none());
    drop(cursor);
    assert_eq!(
        pool.used(),
        Budget::control_storage_bytes() + retained.retained_storage_bytes()
    );
    drop(retained);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
