use crate::support::cursor;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputWindow, Step, WorkBudget};

#[test]
fn retained_input_handle_deduplicates_spans_and_survives_full_consumption_and_reset() {
    let pool = Budget::new(4096).unwrap();
    let mut cursor = cursor(&pool, Some(4), true);
    assert!(cursor.buffered_owner().is_none());
    let mut raw = Vec::with_capacity(511);
    raw.extend_from_slice(b"abcd");
    let bytes = Bytes::try_from_vec(&pool, raw).unwrap();
    let footprint = bytes.retained_storage_bytes();
    let request = cursor.request();
    let supplied = InputWindow::new(request.source, request.token, request.offset, bytes).unwrap();
    cursor.provide(&supplied).unwrap();
    drop(supplied);
    let before = pool.used();
    let retained = cursor.buffered_owner().cloned().unwrap();
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
    assert!(retained.shares_storage_with(&cursor.buffered_owner().cloned().unwrap()));
    let mut last = [0; 2];
    assert!(matches!(
        cursor
            .read_into(&mut last, &mut WorkBudget::new(2), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(&last, b"cd");
    assert!(cursor.buffered_owner().is_none());
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
    let mut cursor = cursor(&pool, Some(4), true);
    let request = cursor.request();
    let bytes = Bytes::copy_from_slice(&pool, b"abcd").unwrap();
    let window = InputWindow::new(request.source, request.token, request.offset, bytes).unwrap();
    cursor.provide(&window).unwrap();
    drop(window);
    let retained = cursor.buffered_owner().cloned().unwrap();
    assert!(
        cursor
            .read_span(1, &mut WorkBudget::new(1), &AtomicBool::new(true))
            .is_err()
    );
    assert!(cursor.buffered_owner().is_none());
    drop(cursor);
    assert_eq!(
        pool.used(),
        Budget::control_storage_bytes() + retained.retained_storage_bytes()
    );
    drop(retained);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
