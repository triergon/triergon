use super::*;
use crate::{
    Limits,
    element::{
        ElementReader,
        control::{EnvelopeProgress, HeaderRefillRequest},
    },
    step::ParseStep,
};
use std::{io::Cursor, sync::atomic::Ordering};
use triergon_core::{Budget, Reservation, SourceCapabilities, SourceId};

mod inline_input;
mod owned_windows;

#[test]
fn blocking_feeder_retries_freeze_without_reading_again_and_observes_known_eof() {
    use std::io::Read;
    struct OnceBlocked {
        pool: Budget,
        blocker: Option<Reservation>,
        reads: usize,
        at: usize,
    }
    impl Read for OnceBlocked {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.at == 3 {
                return Ok(0);
            }
            let count = out.len().min(3 - self.at);
            out[..count].copy_from_slice(&b"abc"[self.at..self.at + count]);
            self.at += count;
            if self.blocker.is_none() {
                self.blocker = Some(self.pool.reserve(self.pool.available()).unwrap());
            }
            Ok(count)
        }
    }
    let pool = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(31),
        SourceCapabilities {
            length: Some(3),
            random_access: false,
        },
        3,
    )
    .unwrap();
    let mut feeder = InputFeeder::new(
        OnceBlocked {
            pool: pool.clone(),
            blocker: None,
            reads: 0,
            at: 0,
        },
        3,
    )
    .unwrap();
    assert!(matches!(
        feeder
            .feed_sequential(&mut input, &AtomicBool::new(false))
            .unwrap(),
        FeedStep::Backpressure { .. }
    ));
    assert_eq!(feeder.reader_mut().reads, 1);
    drop(feeder.reader_mut().blocker.take());
    assert!(matches!(
        feeder
            .feed_sequential(&mut input, &AtomicBool::new(false))
            .unwrap(),
        FeedStep::Provided
    ));
    assert_eq!(feeder.reader_mut().reads, 1);
    let mut bytes = [0; 3];
    assert!(matches!(
        input
            .read_into(&mut bytes, &mut WorkBudget::new(3), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(3)
    ));
    assert_eq!(&bytes, b"abc");
    // Reaching declared length is starvation until an actual Read confirms EOF.
    assert!(matches!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Step::NeedInput(_)
    ));
    assert!(matches!(
        feeder
            .feed_sequential(&mut input, &AtomicBool::new(false))
            .unwrap(),
        FeedStep::End
    ));
    assert_eq!(feeder.reader_mut().reads, 2);
    assert!(matches!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Step::End(_)
    ));
}

#[test]
fn blocking_reposition_observes_cancellation_during_seek() {
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::Ordering;

    struct CancelOnSeek<'a>(&'a AtomicBool);
    impl Read for CancelOnSeek<'_> {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            panic!("reposition does not read");
        }
    }
    impl Seek for CancelOnSeek<'_> {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            assert_eq!(from, SeekFrom::Start(6));
            self.0.store(true, Ordering::Relaxed);
            Ok(6)
        }
    }

    let cancel = AtomicBool::new(false);
    let pool = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(32),
        SourceCapabilities {
            length: Some(8),
            random_access: true,
        },
        4,
    )
    .unwrap();
    let request = input.request_range(6).unwrap();
    let mut feeder = InputFeeder::new(CancelOnSeek(&cancel), 4).unwrap();
    assert!(matches!(
        feeder.synchronize_seekable_position(&mut input, &request, &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    feeder
        .synchronize_seekable_position(&mut input, &request, &cancel)
        .unwrap();
    assert!(!cancel.load(Ordering::Relaxed));
}

#[test]
fn blocking_feeder_keeps_transport_failure_distinct_from_explicit_end() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("authored transport failure"))
        }
    }
    let pool = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(32),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        4,
    )
    .unwrap();
    let mut feeder = InputFeeder::new(Broken, 4).unwrap();
    assert!(matches!(
        feeder.feed_sequential(&mut input, &AtomicBool::new(false)),
        Err(Error::Io(_))
    ));
    assert!(matches!(
        input.read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Transport)
    ));
    let request = input.request();
    assert!(matches!(
        input.finish(request.source, request.token),
        Err(Error::Transport)
    ));
}

fn pending(pool: &Budget) -> (ElementReader, HeaderRefillRequest) {
    let mut envelope = ElementReader::new(
        pool,
        SourceId(2102),
        SourceCapabilities {
            length: Some(4096),
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let exact = match envelope
        .step_typed_envelope(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        EnvelopeProgress::Step(ParseStep::NeedInput(request)) => request,
        other => panic!("initial header: {other:?}"),
    };
    let plan = envelope
        .bounded_header_refill(&exact, 1024)
        .unwrap()
        .unwrap();
    (envelope, plan)
}

#[test]
fn header_refill_retains_full_owner_and_consumes_only_actual_byte_work() {
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let (mut envelope, plan) = pending(&pool);
    let mut feeder = InputFeeder::new(Cursor::new(vec![b'q'; 4096]), 8192).unwrap();
    assert!(matches!(
        feeder
            .feed_typed_header_refill(envelope.range_input(), &plan, &AtomicBool::new(false), None)
            .unwrap(),
        FeedStep::Provided
    ));
    assert_eq!(feeder.position, 1024);
    assert_eq!(envelope.position(), 0);
    let owner = envelope.range_input().buffered_owner().cloned().unwrap();
    assert_eq!((owner.len(), owner.retained_capacity()), (1024, 1024));
    let alias = owner.slice(1023..1024).unwrap();
    let charge = owner.retained_storage_bytes();
    let mut byte = [0];
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        envelope
            .range_input()
            .read_into(&mut byte, &mut zero, &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!((envelope.position(), zero.spent()), (0, 0));
    let mut one = WorkBudget::new(1);
    assert!(matches!(
        envelope
            .range_input()
            .read_into(&mut byte, &mut one, &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(1)
    ));
    assert_eq!((envelope.position(), one.spent(), byte), (1, 1, *b"q"));
    drop((envelope, feeder, owner));
    assert_eq!(pool.used(), baseline + charge);
    assert_eq!(alias.retained_capacity(), 1024);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn header_refill_pressure_falls_back_to_original_fixed_inline_hint() {
    let pool = Budget::new(Budget::control_storage_bytes()).unwrap();
    let baseline = pool.used();
    let (mut envelope, plan) = pending(&pool);
    let mut feeder = InputFeeder::new(Cursor::new(vec![b'q'; 4096]), 8192).unwrap();
    assert!(matches!(
        feeder
            .feed_typed_header_refill(envelope.range_input(), &plan, &AtomicBool::new(false), None)
            .unwrap(),
        FeedStep::Provided
    ));
    assert_eq!(feeder.position, 1);
    assert_eq!(feeder.retained_buffer_bytes(), 0);
    assert!(envelope.range_input().buffered_owner().is_none());
    assert_eq!(pool.used(), baseline);
    assert_eq!(pool.peak(), baseline);
}

#[test]
fn header_refill_freeze_pressure_retains_read_once_and_retries_same_chunk() {
    struct Blocked {
        pool: Budget,
        held: Option<Reservation>,
        reads: usize,
    }
    impl Read for Blocked {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            assert_eq!(self.reads, 1);
            assert_eq!(output.len(), 1024);
            output.fill(b'q');
            self.held = Some(self.pool.reserve(self.pool.available()).unwrap());
            Ok(output.len())
        }
    }
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let (mut envelope, plan) = pending(&pool);
    let mut feeder = InputFeeder::new(
        Blocked {
            pool: pool.clone(),
            held: None,
            reads: 0,
        },
        8192,
    )
    .unwrap();
    for _ in 0..2 {
        assert!(matches!(
            feeder
                .feed_typed_header_refill(
                    envelope.range_input(),
                    &plan,
                    &AtomicBool::new(false),
                    None
                )
                .unwrap(),
            FeedStep::Backpressure { .. }
        ));
        assert_eq!(feeder.reader_mut().reads, 1);
        assert_eq!(feeder.retained_buffer_bytes(), 1024);
        assert_eq!(envelope.position(), 0);
    }
    drop(feeder.reader_mut().held.take());
    assert!(matches!(
        feeder
            .feed_typed_header_refill(envelope.range_input(), &plan, &AtomicBool::new(false), None)
            .unwrap(),
        FeedStep::Provided
    ));
    assert_eq!(feeder.reader_mut().reads, 1);
    assert_eq!(
        envelope
            .range_input()
            .buffered_owner()
            .cloned()
            .unwrap()
            .retained_capacity(),
        1024
    );
    drop((envelope, feeder, plan));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn header_refill_cancellation_after_read_never_publishes_or_rereads() {
    struct Cancel<'a> {
        cancel: &'a AtomicBool,
        reads: usize,
    }
    impl Read for Cancel<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            output.fill(b'q');
            self.cancel.store(true, Ordering::Relaxed);
            Ok(output.len())
        }
    }
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let (mut envelope, plan) = pending(&pool);
    let cancel = AtomicBool::new(false);
    let mut feeder = InputFeeder::new(
        Cancel {
            cancel: &cancel,
            reads: 0,
        },
        8192,
    )
    .unwrap();
    assert!(matches!(
        feeder.feed_typed_header_refill(envelope.range_input(), &plan, &cancel, None),
        Err(Error::Cancelled)
    ));
    assert_eq!(envelope.position(), 0);
    assert!(envelope.range_input().buffered_owner().is_none());
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        feeder.feed_typed_header_refill(envelope.range_input(), &plan, &cancel, None),
        Err(Error::Cancelled)
    ));
    assert_eq!(feeder.reader_mut().reads, 1);
    drop((envelope, feeder, plan));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn header_refill_transport_error_is_sticky_and_never_end() {
    struct Broken {
        reads: usize,
    }
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            Err(io::Error::other("authored refill transport failure"))
        }
    }
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let (mut envelope, plan) = pending(&pool);
    let mut feeder = InputFeeder::new(Broken { reads: 0 }, 8192).unwrap();
    assert!(matches!(
        feeder.feed_typed_header_refill(
            envelope.range_input(),
            &plan,
            &AtomicBool::new(false),
            None
        ),
        Err(Error::Io(_))
    ));
    assert!(matches!(
        envelope.range_input().read_into(
            &mut [0],
            &mut WorkBudget::new(1),
            &AtomicBool::new(false)
        ),
        Err(Error::Transport)
    ));
    assert!(
        feeder
            .feed_typed_header_refill(envelope.range_input(), &plan, &AtomicBool::new(false), None)
            .is_err()
    );
    assert_eq!(feeder.reader_mut().reads, 1);
    drop((envelope, feeder, plan));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn header_refill_known_entity_end_keeps_real_one_byte_eof_probe() {
    struct Empty {
        sizes: Vec<usize>,
    }
    impl Read for Empty {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.sizes.push(output.len());
            Ok(0)
        }
    }
    let pool = Budget::new(Budget::control_storage_bytes()).unwrap();
    let mut envelope = ElementReader::new(
        &pool,
        SourceId(2104),
        SourceCapabilities {
            length: Some(0),
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let exact = match envelope
        .step_typed_envelope(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        EnvelopeProgress::Step(ParseStep::NeedInput(request)) => request,
        other => panic!("known entity end: {other:?}"),
    };
    assert_eq!(exact.maximum, 0);
    assert!(
        envelope
            .bounded_header_refill(&exact, 1024)
            .unwrap()
            .is_none()
    );
    let mut feeder = InputFeeder::new(Empty { sizes: Vec::new() }, 8192).unwrap();
    assert!(matches!(
        feeder
            .feed_typed_request(
                envelope.range_input(),
                &exact,
                true,
                &AtomicBool::new(false),
                None
            )
            .unwrap(),
        FeedStep::End
    ));
    assert_eq!(feeder.reader_mut().sizes, [1]);
}
