//! Supply-path guards: real headers use fixed cursor storage; even tiny bodies
//! retain their charged owner and pending freeze. No alternate parser is used.
use crate::{
    Error,
    matroska::typed::blocking::{FeedStep, InputFeeder},
};
use std::{
    io::{self, Read},
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, InputCursor, Reservation, SourceCapabilities, SourceId, Step, WorkBudget,
};

#[test]
fn inline_header_supply_progresses_with_only_the_pool_control_charge() {
    struct Recorded {
        at: usize,
        calls: [(usize, usize); 2],
        count: usize,
    }
    impl Read for Recorded {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.calls[self.count] = (self.at, output.len());
            self.count += 1;
            let count = output.len().min(4 - self.at);
            output[..count].copy_from_slice(&b"abcd"[self.at..self.at + count]);
            self.at += count;
            Ok(count)
        }
    }
    let pool = Budget::new(Budget::control_storage_bytes()).unwrap();
    let baseline = pool.used();
    let mut input = InputCursor::new(
        &pool,
        SourceId(801),
        SourceCapabilities {
            length: Some(4),
            random_access: false,
        },
        InputCursor::INLINE_CAPACITY,
    )
    .unwrap();
    let mut feeder = InputFeeder::new(
        Recorded {
            at: 0,
            calls: [(0, 0); 2],
            count: 0,
        },
        InputCursor::INLINE_CAPACITY,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let request = input.request();
    assert!(matches!(
        feeder
            .feed_typed_request(&mut input, &request, true, &cancel, None)
            .unwrap(),
        FeedStep::Provided
    ));
    assert!(input.retained_window().is_none());
    assert_eq!(pool.used(), baseline);
    assert_eq!(feeder.retained_buffer_bytes(), 0);
    let mut byte = [0];
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        input.read_into(&mut byte, &mut zero, &cancel).unwrap(),
        Step::Yield
    ));
    assert_eq!((input.position(), zero.spent(), byte), (0, 0, [0]));
    for (at, expected) in b"abcd".iter().enumerate() {
        let mut one = WorkBudget::new(1);
        assert!(matches!(
            input.read_into(&mut byte, &mut one, &cancel).unwrap(),
            Step::Ready(1)
        ));
        assert_eq!(byte[0], *expected);
        assert_eq!((input.position(), one.spent()), ((at + 1) as u64, 1));
        assert!(input.retained_window().is_none());
        assert_eq!(pool.used(), baseline);
    }
    // Exhausting declared bytes still needs a real one-byte EOF probe.
    let request = input.request();
    assert_eq!(request.maximum, 0);
    assert!(matches!(
        input
            .read_into(&mut byte, &mut WorkBudget::new(1), &cancel)
            .unwrap(),
        Step::NeedInput(_)
    ));
    assert!(matches!(
        feeder
            .feed_typed_request(&mut input, &request, true, &cancel, None)
            .unwrap(),
        FeedStep::End
    ));
    assert_eq!(feeder.reader_mut().calls, [(0, 4), (4, 1)]);
    assert!(matches!(
        input
            .read_into(&mut byte, &mut WorkBudget::new(1), &cancel)
            .unwrap(),
        Step::End(_)
    ));
    assert_eq!(pool.peak(), baseline);
}

#[test]
fn tiny_typed_body_keeps_its_owned_freeze_retry_and_alias_lifetime() {
    struct FreezeBlocked {
        pool: Budget,
        blocker: Option<Reservation>,
        reads: usize,
    }
    impl Read for FreezeBlocked {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            assert_eq!(self.reads, 1, "a pending owned freeze must not repeat I/O");
            assert_eq!(output.len(), 3);
            output.copy_from_slice(b"abc");
            self.blocker = Some(self.pool.reserve(self.pool.available()).unwrap());
            Ok(3)
        }
    }
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut input = InputCursor::new(
        &pool,
        SourceId(802),
        SourceCapabilities {
            length: Some(3),
            random_access: false,
        },
        InputCursor::INLINE_CAPACITY,
    )
    .unwrap();
    let mut feeder = InputFeeder::new(
        FreezeBlocked {
            pool: pool.clone(),
            blocker: None,
            reads: 0,
        },
        InputCursor::INLINE_CAPACITY,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let request = input.request();
    for _ in 0..2 {
        assert!(matches!(
            feeder
                .feed_typed_request(&mut input, &request, false, &cancel, None)
                .unwrap(),
            FeedStep::Backpressure { .. }
        ));
        assert_eq!(input.position(), 0);
        assert!(input.retained_window().is_none());
        assert_eq!(feeder.retained_buffer_bytes(), 3);
        assert_eq!(feeder.reader_mut().reads, 1);
    }
    drop(feeder.reader_mut().blocker.take());
    assert!(matches!(
        feeder
            .feed_typed_request(&mut input, &request, false, &cancel, None)
            .unwrap(),
        FeedStep::Provided
    ));
    assert_eq!(feeder.reader_mut().reads, 1);
    assert_eq!(feeder.retained_buffer_bytes(), 0);
    let owner = input.retained_window().unwrap();
    let charge = 3 + Bytes::owner_overhead_bytes();
    assert_eq!(owner.retained_storage_bytes(), charge);
    assert_eq!(pool.used(), baseline + charge);
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        input.read_span(3, &mut zero, &cancel).unwrap(),
        Step::Yield
    ));
    assert_eq!((input.position(), zero.spent()), (0, 0));
    let first = match input
        .read_span(1, &mut WorkBudget::new(1), &cancel)
        .unwrap()
    {
        Step::Ready(bytes) => bytes,
        other => panic!("body prefix: {other:?}"),
    };
    let rest = match input
        .read_span(2, &mut WorkBudget::new(2), &cancel)
        .unwrap()
    {
        Step::Ready(bytes) => bytes,
        other => panic!("body suffix: {other:?}"),
    };
    assert_eq!(first.as_ref(), b"a");
    assert_eq!(rest.as_ref(), b"bc");
    assert!(first.shares_storage_with(&rest) && owner.shares_storage_with(&first));
    drop((input, feeder, owner));
    assert_eq!(pool.used(), baseline + charge);
    drop(first);
    assert_eq!(pool.used(), baseline + charge);
    drop(rest);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn inline_header_cancellation_after_read_is_sticky_without_publication() {
    struct CancelAfterRead<'a> {
        cancel: &'a AtomicBool,
        reads: usize,
    }
    impl Read for CancelAfterRead<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            output[0] = b'x';
            self.cancel.store(true, Ordering::Relaxed);
            Ok(1)
        }
    }
    let pool = Budget::new(Budget::control_storage_bytes()).unwrap();
    let baseline = pool.used();
    let mut input = InputCursor::new(
        &pool,
        SourceId(803),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut feeder = InputFeeder::new(
        CancelAfterRead {
            cancel: &cancel,
            reads: 0,
        },
        1,
    )
    .unwrap();
    let request = input.request();
    assert!(matches!(
        feeder.feed_typed_request(&mut input, &request, true, &cancel, None),
        Err(Error::Cancelled)
    ));
    assert_eq!(input.position(), 0);
    assert!(input.retained_window().is_none());
    assert_eq!(feeder.retained_buffer_bytes(), 0);
    assert_eq!(feeder.reader_mut().reads, 1);
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        feeder.feed_typed_request(&mut input, &request, true, &cancel, None),
        Err(Error::Cancelled)
    ));
    assert_eq!(feeder.reader_mut().reads, 1);
    assert_eq!(pool.peak(), baseline);
}

#[test]
fn inline_header_transport_failure_never_becomes_eof() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("authored inline-header transport failure"))
        }
    }
    let pool = Budget::new(Budget::control_storage_bytes()).unwrap();
    let baseline = pool.used();
    let mut input = InputCursor::new(
        &pool,
        SourceId(804),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        1,
    )
    .unwrap();
    let mut feeder = InputFeeder::new(Broken, 1).unwrap();
    let cancel = AtomicBool::new(false);
    let request = input.request();
    assert!(matches!(
        feeder.feed_typed_request(&mut input, &request, true, &cancel, None),
        Err(Error::Io(_))
    ));
    assert!(matches!(
        input.read_into(&mut [0], &mut WorkBudget::new(1), &cancel),
        Err(triergon_core::Error::Transport)
    ));
    assert!(matches!(
        input.finish(request.source, request.token),
        Err(triergon_core::Error::Transport)
    ));
    assert!(input.retained_window().is_none());
    assert_eq!(pool.peak(), baseline);
}
