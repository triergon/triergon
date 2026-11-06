use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure,
    SourceId, Step, WorkBudget,
};
fn cursor(pool: &Budget, length: Option<u64>, ranges: bool) -> InputCursor {
    InputCursor::new(
        pool,
        SourceId(40),
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
#[test]
fn reserved_append_consumes_inline_with_no_free_pool_or_temporary_owner() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    {
        let mut input = cursor(&pool, Some(3), false);
        let mut output = BudgetVec::new(&pool);
        output.try_reserve_exact(8).unwrap();
        output.try_extend_from_slice(b"pre").unwrap();
        let pointer = output.as_slice().as_ptr();
        inline(&mut input, b"abc");
        input.finish(input.source(), input.token()).unwrap();
        let token = input.token();
        let blocker = pool.reserve(pool.available()).unwrap();
        let held = pool.used();
        for count in 1..=3 {
            let mut work = WorkBudget::new(1);
            assert!(matches!(
                input
                    .read_append(&mut output, 8, &mut work, &AtomicBool::new(false))
                    .unwrap(),
                Step::Ready(1)
            ));
            assert_eq!(work.spent(), 1);
            assert_eq!(input.position(), count);
            assert_eq!(input.token(), token);
            assert!(input.retained_window().is_none());
            assert_eq!(output.as_slice().as_ptr(), pointer);
            assert_eq!(pool.used(), held);
        }
        assert_eq!(output.as_slice(), b"preabc");
        assert!(
            matches!(input.read_append(&mut output, 1, &mut WorkBudget::new(0), &AtomicBool::new(false)).unwrap(), Step::End(at) if at.offset == 3)
        );
        drop(blocker);
    }
    assert_eq!(pool.used(), baseline);
}
#[test]
fn destination_pressure_preserves_source_work_bytes_capacity_and_allows_retry() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, Some(3), false);
    inline(&mut input, b"abc");
    let token = input.token();
    let mut output = BudgetVec::new(&pool);
    output.try_extend_from_slice(b"pre").unwrap();
    let capacity = output.capacity();
    let pointer = output.as_slice().as_ptr();
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut work = WorkBudget::new(2);
    assert!(matches!(
        input.read_append(&mut output, 2, &mut work, &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(input.position(), 0);
    assert_eq!(input.token(), token);
    assert!(input.retained_window().is_none());
    assert_eq!(output.as_slice(), b"pre");
    assert_eq!(output.capacity(), capacity);
    assert_eq!(output.as_slice().as_ptr(), pointer);
    drop(blocker);
    assert!(matches!(
        input
            .read_append(&mut output, 2, &mut work, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(work.spent(), 2);
    assert_eq!(input.position(), 2);
    assert_eq!(output.as_slice(), b"preab");
    assert!(matches!(
        input
            .read_append(
                &mut output,
                usize::MAX,
                &mut WorkBudget::new(u64::MAX),
                &AtomicBool::new(false)
            )
            .unwrap(),
        Step::Ready(1)
    ));
    assert_eq!(output.as_slice(), b"preabc");
}
#[test]
fn a_single_inline_byte_is_charged_only_after_append_succeeds() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, Some(1), false);
    let mut output = BudgetVec::new(&pool);
    inline(&mut input, b"z");
    input.finish(input.source(), input.token()).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        input.read_append(&mut output, 1, &mut WorkBudget::new(0), &cancel),
        Ok(Step::Yield)
    ));
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut work = WorkBudget::new(1);
    assert!(matches!(
        input.read_append(&mut output, 1, &mut work, &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(work.spent(), 0);
    assert_eq!(input.position(), 0);
    assert_eq!(input.buffered(), b"z");
    drop(blocker);
    assert!(matches!(
        input.read_append(&mut output, 1, &mut work, &cancel),
        Ok(Step::Ready(1))
    ));
    assert_eq!(work.spent(), 1);
    assert_eq!(output.as_slice(), b"z");
    assert!(matches!(
        input.read_append(&mut output, 1, &mut work, &cancel),
        Ok(Step::End(at)) if at.offset == 1
    ));
}

#[test]
fn append_may_copy_to_a_different_budget_without_foreign_pool_policy() {
    let source_pool = Budget::new(4096).unwrap();
    let target_pool = Budget::new(4096).unwrap();
    let source_baseline = source_pool.used();
    let mut input = cursor(&source_pool, Some(3), false);
    inline(&mut input, b"abc");
    let mut output = BudgetVec::new(&target_pool);
    let blocker = target_pool.reserve(target_pool.available()).unwrap();
    let mut work = WorkBudget::new(3);
    assert!(matches!(
        input.read_append(&mut output, 3, &mut work, &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(input.position(), 0);
    assert_eq!(work.spent(), 0);
    assert!(output.is_empty());
    drop(blocker);
    assert!(matches!(
        input
            .read_append(&mut output, 3, &mut work, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(3)
    ));
    assert_eq!(output.as_slice(), b"abc");
    assert_eq!(source_pool.used(), source_baseline);
    assert!(target_pool.used() > Budget::control_storage_bytes());
}
#[test]
fn append_preserves_owned_hidden_capacity_and_independent_retained_handles() {
    let pool = Budget::new(4096).unwrap();
    let target = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut raw = Vec::with_capacity(128);
    raw.extend_from_slice(b"_abcdef_");
    let bytes = Bytes::try_from_vec(&pool, raw)
        .unwrap()
        .slice(1..7)
        .unwrap();
    let alias = bytes.clone();
    let charge = pool.used();
    let mut input = cursor(&pool, Some(6), false);
    let request = input.request();
    input
        .provide_owned(
            InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
        )
        .unwrap();
    input.finish(input.source(), input.token()).unwrap();
    let mut output = BudgetVec::new(&target);
    let mut work = WorkBudget::new(2);
    assert!(matches!(
        input
            .read_append(&mut output, usize::MAX, &mut work, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(output.as_slice(), b"ab");
    assert!(input.retained_window().is_some());
    assert_eq!(pool.used(), charge);
    assert!(matches!(
        input
            .read_append(
                &mut output,
                4,
                &mut WorkBudget::new(9),
                &AtomicBool::new(false)
            )
            .unwrap(),
        Step::Ready(4)
    ));
    assert_eq!(output.as_slice(), b"abcdef");
    assert!(input.retained_window().is_none());
    assert_eq!(pool.used(), charge);
    assert_eq!(alias.as_ref(), b"abcdef");
    drop(input);
    assert_eq!(pool.used(), charge);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn empty_wait_work_and_explicit_eof_follow_read_into_order() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, Some(3), false);
    let mut output = BudgetVec::new(&pool);
    output.try_extend_from_slice(b"pre").unwrap();
    assert!(matches!(
        input
            .read_append(
                &mut output,
                0,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap(),
        Step::Ready(0)
    ));
    assert!(
        matches!(input.read_append(&mut output, 1, &mut WorkBudget::new(0), &AtomicBool::new(false)).unwrap(), Step::NeedInput(at) if at.offset == 0)
    );
    inline(&mut input, b"abc");
    assert!(matches!(
        input
            .read_append(
                &mut output,
                3,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(input.position(), 0);
    assert_eq!(output.as_slice(), b"pre");
    input.finish(input.source(), input.token()).unwrap();
    assert!(matches!(
        input
            .read_append(
                &mut output,
                3,
                &mut WorkBudget::new(3),
                &AtomicBool::new(false)
            )
            .unwrap(),
        Step::Ready(3)
    ));
    assert!(
        matches!(input.read_append(&mut output, 1, &mut WorkBudget::new(0), &AtomicBool::new(false)).unwrap(), Step::End(at) if at.offset == 3 && at.source == SourceId(40))
    );
    assert_eq!(output.as_slice(), b"preabc");
}
#[test]
fn errors_precede_empty_reads_and_cancel_releases_only_cursor_ownership() {
    let pool = Budget::new(4096).unwrap();
    let mut failed = cursor(&pool, None, false);
    inline(&mut failed, b"abc");
    failed
        .fail(failed.source(), failed.token(), SourceFailure::Transport)
        .unwrap();
    let cancel = AtomicBool::new(true);
    let mut output = BudgetVec::new(&pool);
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        failed.read_append(&mut output, 0, &mut work, &cancel),
        Err(Error::Transport)
    ));
    assert!(output.is_empty());
    let bytes = Bytes::copy_from_slice(&pool, b"owned").unwrap();
    let alias = bytes.clone();
    let mut input = cursor(&pool, Some(5), false);
    let request = input.request();
    input
        .provide_owned(
            InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
        )
        .unwrap();
    let held = pool.used();
    assert!(matches!(
        input.read_append(&mut output, 0, &mut work, &cancel),
        Err(Error::Cancelled)
    ));
    assert!(input.retained_window().is_none());
    assert_eq!(input.position(), 0);
    assert_eq!(work.spent(), 0);
    assert!(output.is_empty());
    assert_eq!(pool.used(), held);
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        input.read_append(&mut output, 1, &mut work, &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(alias.as_ref(), b"owned");
}
#[test]
fn maximum_count_is_bounded_by_actual_bytes_at_a_u64_sparse_endpoint() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, Some(u64::MAX), true);
    let generation = input.generation();
    input.request_range(u64::MAX - 2).unwrap();
    inline(&mut input, b"xy");
    input.finish(input.source(), input.token()).unwrap();
    let mut output = BudgetVec::new(&pool);
    let mut work = WorkBudget::new(u64::MAX);
    assert!(matches!(
        input
            .read_append(&mut output, usize::MAX, &mut work, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(2)
    ));
    assert_eq!(input.position(), u64::MAX);
    assert_eq!(input.generation(), generation);
    assert_eq!(output.as_slice(), b"xy");
    assert_eq!(work.spent(), 2);
}
