use std::sync::atomic::{AtomicBool, Ordering};
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

fn provide(input: &mut InputCursor, bytes: &[u8]) {
    let request = input.request();
    input
        .provide_inline(request.source, request.token, request.offset, bytes)
        .unwrap();
}

fn read(input: &mut InputCursor, output: &mut [u8], units: u64) -> usize {
    let mut work = WorkBudget::new(units);
    match input
        .read_into(output, &mut work, &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(count) => {
            assert_eq!(work.spent(), count as u64);
            count
        }
        other => panic!("expected actual byte progress: {other:?}"),
    }
}

#[test]
fn inline_headers_copy_caller_bytes_and_preserve_work_and_entity_end() {
    let pool = Budget::new(1024).unwrap();
    let base = pool.used();
    let mut input = cursor(&pool, Some(4), false);
    let mut caller = [10, 11, 12, 13];
    provide(&mut input, &caller);
    caller.fill(99);
    assert_eq!(pool.used(), base);
    assert!(input.retained_window().is_none());
    let mut output = [0; 4];
    assert!(matches!(
        input
            .read_into(
                &mut output,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(input.position(), 0);
    input.finish(SourceId(7), input.token()).unwrap();
    assert_eq!(read(&mut input, &mut output, 1), 1);
    assert_eq!(output[0], 10);
    assert_eq!(read(&mut input, &mut output, 2), 2);
    assert_eq!(&output[..2], &[11, 12]);
    assert_eq!(read(&mut input, &mut output, 8), 1);
    assert_eq!(output[0], 13);
    assert!(
        matches!(input.read_into(&mut output, &mut WorkBudget::new(8), &AtomicBool::new(false)).unwrap(), Step::End(at) if at.source == SourceId(7) && at.offset == 4)
    );
    assert_eq!(pool.used(), base);
}

#[test]
fn invalid_inline_responses_cannot_replace_requested_input() {
    let pool = Budget::new(1024).unwrap();
    let base = pool.used();
    let mut input = cursor(&pool, None, true);
    let other_pool = Budget::new(1024).unwrap();
    let other = cursor(&other_pool, None, true);
    let request = input.request();
    assert_eq!(
        input
            .provide_inline(request.source, other.token(), 0, b"x")
            .unwrap_err(),
        Error::ForeignPool
    );
    assert_eq!(
        input
            .provide_inline(SourceId(8), request.token.clone(), 0, b"x")
            .unwrap_err(),
        Error::SourceMismatch
    );
    assert!(matches!(
        input.provide_inline(request.source, request.token.clone(), 1, b"x"),
        Err(Error::Invalid(_))
    ));
    assert_eq!(InputCursor::INLINE_CAPACITY, 12);
    assert_eq!(
        input
            .provide_inline(request.source, request.token, 0, &[0; 13])
            .unwrap_err(),
        Error::Limit {
            requested: 13,
            maximum: 12
        }
    );
    assert_eq!(input.position(), 0);
    assert_eq!(pool.used(), base);
    provide(&mut input, b"abc");
    let request = input.request();
    assert!(matches!(
        input.provide_inline(request.source, request.token, request.offset, b"X"),
        Err(Error::Invalid(_))
    ));
    let mut out = [0; 3];
    assert_eq!(read(&mut input, &mut out, 3), 3);
    assert_eq!(&out, b"abc");
}

#[test]
fn inline_policy_and_known_length_rejections_are_nonmutating() {
    let pool = Budget::new(1024).unwrap();
    let mut narrow = InputCursor::new(
        &pool,
        SourceId(7),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        2,
    )
    .unwrap();
    let request = narrow.request();
    assert_eq!(
        narrow
            .provide_inline(request.source, request.token, 0, b"abc")
            .unwrap_err(),
        Error::Limit {
            requested: 3,
            maximum: 2
        }
    );
    provide(&mut narrow, b"ok");
    let mut out = [0; 2];
    assert_eq!(read(&mut narrow, &mut out, 2), 2);
    assert_eq!(&out, b"ok");
    let mut short = cursor(&pool, Some(1), false);
    let request = short.request();
    assert!(matches!(
        short.provide_inline(request.source, request.token, 0, b"ab"),
        Err(Error::Invalid(_))
    ));
    assert_eq!(short.position(), 0);
    provide(&mut short, b"z");
    assert_eq!(read(&mut short, &mut out, 1), 1);
    assert_eq!(out[0], b'z');
}

#[test]
fn range_and_logical_reset_invalidate_inline_data_and_old_tokens() {
    let pool = Budget::new(1024).unwrap();
    let mut input = cursor(&pool, Some(8), true);
    let first = input.request();
    provide(&mut input, &[0, 1, 2, 3]);
    assert_eq!(read(&mut input, &mut [0], 1), 1);
    let range = input.request_range(4).unwrap();
    assert_eq!(range.token.generation(), first.token.generation());
    assert!(matches!(
        input.provide_inline(range.source, first.token, 4, b"x"),
        Err(Error::StaleRequest { .. })
    ));
    input
        .provide_inline(range.source, range.token.clone(), 4, &[4, 5, 6, 7])
        .unwrap();
    let mut out = [0];
    assert_eq!(read(&mut input, &mut out, 1), 1);
    assert_eq!(out[0], 4);
    let reset = input.reset(0).unwrap();
    assert_ne!(reset.token.generation(), range.token.generation());
    assert!(matches!(
        input.provide_inline(reset.source, range.token, 0, b"x"),
        Err(Error::StaleGeneration { .. })
    ));
    provide(&mut input, &[9, 8]);
    assert_eq!(read(&mut input, &mut out, 1), 1);
    assert_eq!(out[0], 9);
    assert!(input.retained_window().is_none());
}

#[test]
fn empty_availability_does_not_mean_eof_and_transport_failure_stays_sticky() {
    let pool = Budget::new(1024).unwrap();
    let base = pool.used();
    let mut input = cursor(&pool, None, false);
    provide(&mut input, &[]);
    assert!(matches!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Step::NeedInput(_)
    ));
    provide(&mut input, b"data");
    input
        .fail(SourceId(7), input.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        input
            .read_into(&mut [], &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
    assert_eq!(
        input
            .read_span(1, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
    assert!(input.retained_window().is_none());
    assert_eq!(pool.used(), base);
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
fn cancellation_and_short_declared_entities_discard_inline_data_until_reset() {
    let pool = Budget::new(1024).unwrap();
    let mut input = cursor(&pool, Some(3), false);
    provide(&mut input, b"ab");
    assert_eq!(
        input.finish(SourceId(7), input.token()).unwrap_err(),
        Error::Truncated
    );
    assert_eq!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Truncated
    );
    input.reset(0).unwrap();
    provide(&mut input, b"xyz");
    let cancel = AtomicBool::new(true);
    assert_eq!(
        input
            .read_span(1, &mut WorkBudget::new(0), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    input.reset(0).unwrap();
    provide(&mut input, b"123");
    let mut out = [0; 3];
    assert_eq!(read(&mut input, &mut out, 3), 3);
    assert_eq!(&out, b"123");
}

#[test]
fn zero_work_does_not_promote_inline_storage_or_hide_a_later_limit() {
    let pool = Budget::new(Budget::control_storage_bytes()).unwrap();
    let mut input = cursor(&pool, None, false);
    provide(&mut input, &[1; 12]);
    let base = pool.used();
    assert!(matches!(
        input
            .read_span(6, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert!(input.retained_window().is_none());
    assert_eq!(pool.used(), base);
    assert!(matches!(
        input.read_span(6, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
    assert_eq!(input.position(), 0);
    assert_eq!(read(&mut input, &mut [0], 1), 1);
    assert_eq!(pool.used(), base);
}

#[test]
fn span_promotion_retries_pressure_and_retains_the_whole_original_owner() {
    let raw = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    let charge = Bytes::owner_overhead_bytes() + raw.len();
    let pool = Budget::new(Budget::control_storage_bytes() + charge + 16).unwrap();
    let base = pool.used();
    let hold = pool.reserve(32).unwrap();
    let mut input = cursor(&pool, None, false);
    provide(&mut input, &raw);
    assert_eq!(read(&mut input, &mut [0; 3], 3), 3);
    let mut work = WorkBudget::new(4);
    assert!(matches!(
        input.read_span(4, &mut work, &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(input.position(), 3);
    assert!(input.retained_window().is_none());
    drop(hold);
    let head = match input
        .read_span(4, &mut work, &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(bytes) => bytes,
        other => panic!("{other:?}"),
    };
    assert_eq!(head.as_ref(), &raw[3..7]);
    assert_eq!(work.spent(), 4);
    let owner = input.retained_window().unwrap();
    assert_eq!(owner.as_ref(), raw);
    assert!(head.shares_storage_with(&owner));
    assert!(owner.retained_capacity() >= raw.len());
    assert_eq!(
        owner.retained_storage_bytes(),
        owner.retained_capacity() + Bytes::owner_overhead_bytes()
    );
    let retained = pool.used();
    assert_eq!(retained, base + owner.retained_storage_bytes());
    input.finish(SourceId(7), input.token()).unwrap();
    let tail = match input
        .read_span(64, &mut WorkBudget::new(5), &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(bytes) => bytes,
        other => panic!("{other:?}"),
    };
    assert_eq!(tail.as_ref(), &raw[7..]);
    assert!(tail.shares_storage_with(&head));
    assert!(input.retained_window().is_none());
    drop(input);
    drop(owner);
    drop(tail);
    assert_eq!(pool.used(), retained);
    drop(head);
    assert_eq!(pool.used(), base);
}

#[test]
fn continued_entities_can_switch_between_inline_and_owned_windows() {
    let pool = Budget::new(1024).unwrap();
    let mut input = cursor(&pool, None, false);
    let first = input.request();
    provide(&mut input, b"ab");
    input.finish(first.source, first.token.clone()).unwrap();
    assert_eq!(read(&mut input, &mut [0; 2], 2), 2);
    let second = input
        .continue_source(
            SourceId(8),
            SourceCapabilities {
                length: Some(3),
                random_access: false,
            },
        )
        .unwrap();
    assert_eq!(second.token.generation(), first.token.generation());
    assert!(matches!(
        input.provide_inline(second.source, first.token, 0, b"x"),
        Err(Error::StaleRequest { .. })
    ));
    let owned = Bytes::copy_from_slice(&pool, b"xyz").unwrap();
    input
        .provide(&InputWindow::new(second.source, second.token, 0, owned.clone()).unwrap())
        .unwrap();
    let span = match input
        .read_span(2, &mut WorkBudget::new(2), &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(bytes) => bytes,
        other => panic!("{other:?}"),
    };
    assert!(span.shares_storage_with(&owned));
    assert_eq!(span.as_ref(), b"xy");
    assert_eq!(read(&mut input, &mut [0], 1), 1);
    input.finish(SourceId(8), input.token()).unwrap();
    let third = input
        .continue_source(
            SourceId(9),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
        )
        .unwrap();
    input
        .provide_inline(third.source, third.token, 0, b"new")
        .unwrap();
    let mut out = [0; 3];
    assert_eq!(read(&mut input, &mut out, 3), 3);
    assert_eq!(&out, b"new");
    assert_eq!(span.as_ref(), b"xy");
    assert!(input.retained_window().is_none());
}

#[test]
fn inline_bytes_use_relative_indices_at_large_and_maximum_source_offsets() {
    let pool = Budget::new(1024).unwrap();
    let mut input = cursor(&pool, None, true);
    input.request_range(1 << 40).unwrap();
    provide(&mut input, b"abc");
    let mut out = [0; 3];
    assert_eq!(read(&mut input, &mut out, 3), 3);
    assert_eq!(&out, b"abc");
    assert_eq!(input.position(), (1 << 40) + 3);
    let last = input.request_range(u64::MAX - 1).unwrap();
    assert_eq!(
        input
            .provide_inline(last.source, last.token.clone(), last.offset, b"ab")
            .unwrap_err(),
        Error::Overflow
    );
    assert_eq!(input.position(), u64::MAX - 1);
    input
        .provide_inline(last.source, last.token, last.offset, b"z")
        .unwrap();
    assert_eq!(read(&mut input, &mut out, 1), 1);
    assert_eq!(out[0], b'z');
    assert_eq!(input.position(), u64::MAX);
    input.finish(SourceId(7), input.token()).unwrap();
    assert!(
        matches!(input.read_into(&mut out, &mut WorkBudget::new(1), &AtomicBool::new(false)).unwrap(), Step::End(at) if at.offset == u64::MAX)
    );
}
