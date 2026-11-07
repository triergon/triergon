use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, WorkBudget,
};

fn cursor(pool: &Budget, length: Option<u64>, random_access: bool) -> InputCursor {
    InputCursor::new(
        pool,
        SourceId(7),
        SourceCapabilities {
            length,
            random_access,
        },
        64,
    )
    .unwrap()
}
fn window(cursor: &InputCursor, bytes: &[u8]) -> InputWindow {
    let request = cursor.request();
    InputWindow::new(
        request.source,
        request.token,
        request.offset,
        Bytes::copy_from_slice(cursor.budget(), bytes).unwrap(),
    )
    .unwrap()
}

#[test]
fn starvation_empty_input_work_yield_and_explicit_entity_end_are_distinct() {
    let pool = Budget::new(8192).unwrap();
    let mut input = cursor(&pool, None, false);
    let cancel = AtomicBool::new(false);
    let mut work = WorkBudget::new(32);
    let mut out = [0; 4];
    assert!(matches!(
        input.read_into(&mut out, &mut work, &cancel).unwrap(),
        Step::NeedInput(_)
    ));
    input.provide(&window(&input, &[])).unwrap();
    assert!(matches!(
        input.read_into(&mut out, &mut work, &cancel).unwrap(),
        Step::NeedInput(_)
    ));
    input.provide(&window(&input, b"abcd")).unwrap();
    assert!(matches!(
        input
            .read_into(&mut out, &mut WorkBudget::new(0), &cancel)
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(input.position(), 0);
    assert!(matches!(
        input
            .read_into(&mut out, &mut WorkBudget::new(2), &cancel)
            .unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(&out[..2], b"ab");
    input.finish(SourceId(7), input.token()).unwrap();
    assert!(matches!(
        input.read_into(&mut out, &mut work, &cancel).unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(&out[..2], b"cd");
    assert!(
        matches!(input.read_into(&mut out, &mut work, &cancel).unwrap(), Step::End(at) if at.offset == 4)
    );
}

#[test]
fn range_request_reuses_shared_bytes_with_a_fresh_token() {
    let pool = Budget::new(8192).unwrap();
    let mut input = cursor(&pool, Some(24), true);
    input.request_range(8).unwrap();
    let original_token = input.token();
    let original = window(&input, b"abcdef");
    input.provide(&original).unwrap();
    let cancel = AtomicBool::new(false);
    let mut work = WorkBudget::new(8);
    assert!(matches!(
        input.read_discard(3, &mut work, &cancel).unwrap(),
        Step::Ready(3)
    ));
    let request = input.request_range_reusing_window(9).unwrap();
    assert_eq!(request.offset, 9);
    assert_eq!(request.token.generation(), original_token.generation());
    assert!(matches!(
        input.provide(&original),
        Err(Error::StaleRequest { .. })
    ));
    let Step::Ready(span) = input.read_span(2, &mut work, &cancel).unwrap() else {
        panic!("repositioned bytes must be available");
    };
    assert_eq!(span.as_ref(), b"bc");
    assert!(span.shares_storage_with(original.bytes()));
    input.request_range_reusing_window(14).unwrap();
    assert!(input.buffered().is_empty());
    assert_eq!(input.position(), 14);
}

#[test]
fn source_offsets_above_four_gib_are_not_lengths_or_usize_indices() {
    let pool = Budget::new(8192).unwrap();
    let mut input = cursor(&pool, Some(1 << 42), true);
    input.request_range(1 << 40).unwrap();
    input.provide(&window(&input, b"xyz")).unwrap();
    let retained = match input
        .read_span(2, &mut WorkBudget::new(8), &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(value) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(retained.as_ref(), b"xy");
    assert_eq!(input.position(), (1 << 40) + 2);
    let used = pool.used();
    input.reset(0).unwrap();
    drop(input);
    assert_eq!(pool.used(), used);
    drop(retained);
    assert!(pool.used() < used);
    let bytes = Bytes::copy_from_slice(&pool, b"xx").unwrap();
    let input = cursor(&pool, None, true);
    assert!(matches!(
        InputWindow::new(SourceId(7), input.token(), u64::MAX, bytes),
        Err(Error::Overflow)
    ));
}

#[test]
fn old_ranges_errors_and_source_reuse_cannot_poison_a_new_request() {
    let pool = Budget::new(8192).unwrap();
    let mut input = cursor(&pool, Some(100), true);
    let stale = window(&input, b"old");
    let generation = input.generation();
    input.request_range(50).unwrap();
    assert_eq!(input.generation(), generation);
    assert!(input.provide(&stale).is_err());
    assert!(
        input
            .fail(SourceId(7), stale.token.clone(), SourceFailure::Transport)
            .is_err()
    );
    input.provide(&window(&input, b"new")).unwrap();
    let mut out = [0; 3];
    assert!(matches!(
        input
            .read_into(&mut out, &mut WorkBudget::new(8), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(3)
    ));
    assert_eq!(&out, b"new");
    input.reset(0).unwrap();
    assert_ne!(input.generation(), generation);
    assert!(input.provide(&stale).is_err());
}

#[test]
fn failure_is_sticky_even_at_known_end_and_cancellation_needs_explicit_recovery() {
    let pool = Budget::new(8192).unwrap();
    let mut input = cursor(&pool, Some(0), true);
    input
        .fail(SourceId(7), input.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
    input.reset(0).unwrap();
    assert_eq!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
    input.reset(0).unwrap();
    input.finish(SourceId(7), input.token()).unwrap();
    assert!(matches!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Step::End(_)
    ));
}

#[test]
fn pending_refill_observes_cancellation_after_a_partial_read() {
    let pool = Budget::new(8192).unwrap();
    let mut input = cursor(&pool, None, false);
    let cancel = AtomicBool::new(false);
    input.provide(&window(&input, b"a")).unwrap();
    assert!(input.pending_request(&cancel).unwrap().is_none());
    let mut out = [0; 2];
    assert!(matches!(
        input
            .read_into(&mut out, &mut WorkBudget::new(2), &cancel)
            .unwrap(),
        Step::Ready(1)
    ));
    assert_eq!(input.pending_request(&cancel).unwrap().unwrap().offset, 1);
    assert_eq!(
        input.pending_request(&AtomicBool::new(true)).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        input.pending_request(&cancel).unwrap_err(),
        Error::Cancelled
    );
}

#[test]
fn invalid_windows_and_truncation_are_checked_before_committing() {
    let pool = Budget::new(8192).unwrap();
    let foreign = Budget::new(8192).unwrap();
    let mut input = cursor(&pool, Some(3), false);
    let request = input.request();
    let outside = InputWindow::new(
        request.source,
        request.token.clone(),
        request.offset,
        Bytes::copy_from_slice(&foreign, b"x").unwrap(),
    )
    .unwrap();
    assert!(input.provide(&outside).is_err());
    assert!(input.provide(&window(&input, b"four")).is_err());
    assert_eq!(input.position(), 0);
    input.provide(&window(&input, b"a")).unwrap();
    assert!(input.provide(&window(&input, b"b")).is_err());
    assert_eq!(
        input.finish(SourceId(7), input.token()),
        Err(Error::Truncated)
    );
    assert_eq!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Truncated
    );
    assert!(input.request_range(0).is_err());
}

#[test]
fn completing_one_resource_is_not_completion_of_a_transport_or_playlist() {
    let pool = Budget::new(8192).unwrap();
    let mut input = cursor(&pool, Some(1), false);
    let old = input.token();
    input.provide(&window(&input, b"a")).unwrap();
    input.finish(SourceId(7), input.token()).unwrap();
    assert!(
        input
            .continue_source(
                SourceId(8),
                SourceCapabilities {
                    length: None,
                    random_access: false
                }
            )
            .is_err()
    );
    input
        .read_span(1, &mut WorkBudget::new(8), &AtomicBool::new(false))
        .unwrap();
    let generation = input.generation();
    input
        .continue_source(
            SourceId(8),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
        )
        .unwrap();
    assert_eq!(input.generation(), generation);
    assert_eq!(input.position(), 0);
    assert!(input.finish(SourceId(8), old).is_err());
    input.provide(&window(&input, b"b")).unwrap();
}

#[test]
fn tokens_identify_the_pool_and_new_parser_instance_not_just_a_small_counter() {
    let pool = Budget::new(8192).unwrap();
    let mut a = cursor(&pool, None, true);
    let b = cursor(&pool, None, true);
    assert_ne!(a.generation(), b.generation());
    assert!(a.provide(&window(&b, b"x")).is_err());
    let other_pool = Budget::new(8192).unwrap();
    let other = cursor(&other_pool, None, true);
    let bogus = InputWindow::new(
        SourceId(7),
        other.token(),
        0,
        Bytes::copy_from_slice(&pool, b"x").unwrap(),
    )
    .unwrap();
    assert!(a.provide(&bogus).is_err());
}
