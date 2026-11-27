use crate::support::{SOURCE, cursor, window as response};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, InputWindow, SourceFailure, Step, WorkBudget};

#[test]
fn owned_spans_keep_hidden_backing_and_work_bounds_until_the_last_output_drops() {
    let pool = Budget::new(4096).unwrap();
    let original = Bytes::copy_from_slice(&pool, b"_abcdef_").unwrap();
    let charged = original.retained_storage_bytes();
    let mut input = cursor(&pool, Some(6), true);
    input
        .provide_owned(response(&input, original.slice(1..7).unwrap()))
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        input
            .read_span(6, &mut WorkBudget::new(0), &cancel)
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(input.position(), 0);
    let Step::Ready(first) = input
        .read_span(6, &mut WorkBudget::new(2), &cancel)
        .unwrap()
    else {
        panic!("first span")
    };
    let Step::Ready(last) = input
        .read_span(6, &mut WorkBudget::new(4), &cancel)
        .unwrap()
    else {
        panic!("last span")
    };
    assert_eq!(first.as_ref(), b"ab");
    assert_eq!(last.as_ref(), b"cdef");
    assert!(first.shares_storage_with(&original) && last.shares_storage_with(&original));
    assert_eq!(last.retained_storage_bytes(), charged);
    assert_eq!(input.position(), 6);
    assert!(matches!(
        input
            .read_span(1, &mut WorkBudget::new(1), &cancel)
            .unwrap(),
        Step::NeedInput(_)
    ));
    input.finish(SOURCE, input.token()).unwrap();
    assert!(
        matches!(input.read_span(1, &mut WorkBudget::new(1), &cancel).unwrap(), Step::End(at) if at.offset == 6)
    );
    drop(original);
    drop(first);
    assert_eq!(pool.used(), Budget::control_storage_bytes() + charged);
    drop(last);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn owned_response_rejection_preserves_cursor_and_distinguishes_stale_and_foreign_tokens() {
    let pool = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, None, true);
    let foreign = cursor(&other, None, true);
    let bytes = Bytes::copy_from_slice(&pool, b"abc").unwrap();
    let bad = InputWindow::new(SOURCE, foreign.token(), 0, bytes.clone()).unwrap();
    assert_eq!(input.provide_owned(bad), Err(Error::ForeignPool));
    let stale = response(&input, bytes.clone());
    input.reset(0).unwrap();
    assert!(matches!(
        input.provide_owned(stale),
        Err(Error::StaleGeneration { .. })
    ));
    assert_eq!(input.position(), 0);
    input.provide_owned(response(&input, bytes)).unwrap();
    let Step::Ready(span) = input
        .read_span(3, &mut WorkBudget::new(3), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("valid response survives rejection")
    };
    assert_eq!(span.as_ref(), b"abc");
}

#[test]
fn owned_empty_response_is_starvation_and_cancellation_and_transport_stay_sticky() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, None, true);
    let cancel = AtomicBool::new(false);
    input
        .provide_owned(response(
            &input,
            Bytes::copy_from_slice(&pool, b"").unwrap(),
        ))
        .unwrap();
    assert!(matches!(
        input
            .read_span(1, &mut WorkBudget::new(1), &cancel)
            .unwrap(),
        Step::NeedInput(_)
    ));
    input
        .provide_owned(response(
            &input,
            Bytes::copy_from_slice(&pool, b"abc").unwrap(),
        ))
        .unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        input.read_span(3, &mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        input.provide_owned(response(
            &input,
            Bytes::copy_from_slice(&pool, b"x").unwrap()
        )),
        Err(Error::Cancelled)
    );
    input.reset(0).unwrap();
    input
        .fail(SOURCE, input.token(), SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        input.read_span(1, &mut WorkBudget::new(1), &cancel),
        Err(Error::Transport)
    ));
    assert_eq!(
        input.provide_owned(response(
            &input,
            Bytes::copy_from_slice(&pool, b"x").unwrap()
        )),
        Err(Error::Transport)
    );
}
