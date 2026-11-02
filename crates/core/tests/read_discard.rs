//! Original cursor contracts: no destination, owner, allocation or inferred EOF.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure, SourceId,
    Step, WorkBudget,
};

fn cursor(pool: &Budget, length: Option<u64>, ranges: bool) -> InputCursor {
    InputCursor::new(
        pool,
        SourceId(1801),
        SourceCapabilities {
            length,
            random_access: ranges,
        },
        64,
    )
    .unwrap()
}
fn inline(input: &mut InputCursor, bytes: &[u8]) {
    let request = input.request();
    input
        .provide_inline(request.source, request.token, request.offset, bytes)
        .unwrap();
}
fn owned(input: &mut InputCursor, bytes: Bytes) {
    let request = input.request();
    input
        .provide_owned(
            InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
        )
        .unwrap();
}

#[test]
fn inline_discard_needs_no_free_pool_or_temporary_owner() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    {
        let mut input = cursor(&pool, Some(3), false);
        inline(&mut input, b"abc");
        input.finish(input.source(), input.token()).unwrap();
        let token = input.token();
        let blocker = pool.reserve(pool.available()).unwrap();
        let held = pool.used();
        for position in 1..=3 {
            let mut work = WorkBudget::new(1);
            assert!(matches!(
                input
                    .read_discard(8, &mut work, &AtomicBool::new(false))
                    .unwrap(),
                Step::Ready(1)
            ));
            assert_eq!(input.position(), position);
            assert_eq!(input.token(), token);
            assert_eq!(work.spent(), 1);
            assert!(input.retained_window().is_none());
            assert_eq!(pool.used(), held);
        }
        assert!(
            matches!(input.read_discard(1, &mut WorkBudget::new(0), &AtomicBool::new(false)).unwrap(), Step::End(at) if at.source == SourceId(1801) && at.offset == 3)
        );
        drop(blocker);
    }
    assert_eq!(pool.used(), baseline);
}

#[test]
fn owned_hidden_capacity_lives_until_cursor_and_last_independent_alias_drop() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut input = cursor(&pool, Some(3), false);
    let mut source = Vec::with_capacity(64);
    source.extend_from_slice(b"preabcpost");
    let root = Bytes::try_from_vec(&pool, source).unwrap();
    let charge = root.retained_storage_bytes();
    let alias = root.slice(3..6).unwrap();
    owned(&mut input, root.slice(3..6).unwrap());
    drop(root);
    assert_eq!(pool.used(), baseline + charge);
    let mut work = WorkBudget::new(2);
    assert!(matches!(
        input
            .read_discard(2, &mut work, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(input.position(), 2);
    assert_eq!(input.retained_window().unwrap().retained_capacity(), 64);
    assert_eq!(pool.used(), baseline + charge);
    assert!(matches!(
        input
            .read_discard(usize::MAX, &mut WorkBudget::new(8), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(1)
    ));
    assert!(input.retained_window().is_none());
    assert_eq!(alias.as_ref(), b"abc");
    assert_eq!(pool.used(), baseline + charge);
    drop(input);
    assert_eq!(pool.used(), baseline + charge);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn consumed_inline_prefix_and_unread_suffix_are_not_promoted() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, None, false);
    inline(&mut input, b"012345");
    let mut byte = [0];
    assert!(matches!(
        input
            .read_into(&mut byte, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(1)
    ));
    assert_eq!(byte, [b'0']);
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut work = WorkBudget::new(2);
    assert!(matches!(
        input
            .read_discard(4, &mut work, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(input.position(), 3);
    assert_eq!(work.spent(), 2);
    assert!(input.retained_window().is_none());
    assert!(matches!(
        input
            .read_into(&mut byte, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(1)
    ));
    assert_eq!(byte, [b'3']);
    drop(blocker);
}

#[test]
fn empty_wait_zero_work_and_explicit_end_follow_read_into_order() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, None, false);
    let token = input.token();
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        input
            .read_discard(0, &mut zero, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(0)
    ));
    assert!(
        matches!(input.read_discard(1, &mut zero, &AtomicBool::new(false)).unwrap(), Step::NeedInput(request) if request.offset == 0 && request.token == token)
    );
    inline(&mut input, b"ab");
    assert!(matches!(
        input
            .read_discard(1, &mut zero, &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(input.position(), 0);
    assert!(input.retained_window().is_none());
    assert!(matches!(
        input
            .read_discard(2, &mut WorkBudget::new(2), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(2)
    ));
    assert!(
        matches!(input.read_discard(1, &mut zero, &AtomicBool::new(false)).unwrap(), Step::NeedInput(request) if request.offset == 2 && request.token == token)
    );
    input.finish(input.source(), input.token()).unwrap();
    assert!(matches!(
        input
            .read_discard(0, &mut zero, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(0)
    ));
    assert!(
        matches!(input.read_discard(1, &mut zero, &AtomicBool::new(false)).unwrap(), Step::End(at) if at.offset == 2)
    );
    assert_eq!(zero.spent(), 0);
}

#[test]
fn maximum_is_bounded_by_buffered_bytes_at_sparse_u64_endpoint() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, None, true);
    let request = input.request_range(u64::MAX - 2).unwrap();
    let generation = input.generation();
    input
        .provide_inline(request.source, request.token.clone(), request.offset, b"xy")
        .unwrap();
    input.finish(input.source(), input.token()).unwrap();
    let mut work = WorkBudget::new(u64::MAX);
    assert!(matches!(
        input
            .read_discard(usize::MAX, &mut work, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(work.spent(), 2);
    assert_eq!(input.position(), u64::MAX);
    assert_eq!(input.token(), request.token);
    assert_eq!(input.generation(), generation);
    assert!(
        matches!(input.read_discard(1, &mut work, &AtomicBool::new(false)).unwrap(), Step::End(at) if at.offset == u64::MAX)
    );
}

#[test]
fn sticky_transport_precedes_empty_read_and_cancellation_without_work() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, None, false);
    owned(&mut input, Bytes::copy_from_slice(&pool, b"abc").unwrap());
    input
        .fail(input.source(), input.token(), SourceFailure::Transport)
        .unwrap();
    let mut work = WorkBudget::new(5);
    assert_eq!(
        input
            .read_discard(0, &mut work, &AtomicBool::new(true))
            .unwrap_err(),
        Error::Transport
    );
    assert_eq!(
        input
            .read_discard(1, &mut work, &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
    assert_eq!(input.position(), 0);
    assert_eq!(work.spent(), 0);
    assert!(input.retained_window().is_none());
}

#[test]
fn cancellation_drops_only_cursor_owner_and_reset_rejects_old_request() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut input = cursor(&pool, None, true);
    let alias = Bytes::copy_from_slice(&pool, b"abc").unwrap();
    owned(&mut input, alias.clone());
    let token = input.token();
    let held = pool.used();
    let mut work = WorkBudget::new(8);
    assert_eq!(
        input
            .read_discard(0, &mut work, &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(input.position(), 0);
    assert_eq!(input.token(), token);
    assert_eq!(work.spent(), 0);
    assert!(input.retained_window().is_none());
    assert_eq!(pool.used(), held);
    assert_eq!(alias.as_ref(), b"abc");
    assert_eq!(
        input
            .read_discard(1, &mut work, &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
    let request = input.reset(0).unwrap();
    assert_ne!(request.token.generation(), token.generation());
    assert!(
        input
            .provide_inline(SourceId(1801), token, 0, b"x")
            .is_err()
    );
    assert!(
        matches!(input.read_discard(1, &mut work, &AtomicBool::new(false)).unwrap(), Step::NeedInput(next) if next.token == request.token)
    );
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn declared_short_finish_is_sticky_truncation_not_successful_discard() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, Some(4), false);
    inline(&mut input, b"abc");
    assert_eq!(
        input.finish(input.source(), input.token()).unwrap_err(),
        Error::Truncated
    );
    let mut work = WorkBudget::new(4);
    assert_eq!(
        input
            .read_discard(0, &mut work, &AtomicBool::new(false))
            .unwrap_err(),
        Error::Truncated
    );
    assert_eq!(input.position(), 0);
    assert_eq!(work.spent(), 0);
}
