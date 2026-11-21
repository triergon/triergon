//! Subtitle recovery during seeks: work, cancellation, reset, pressure, scan
//! limits and transport failures.
use crate::reader::*;
use crate::subtitle_recovery_kit::{INPUT, assert_prefix};
use crate::{Error, Limits};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, InputCursor, SourceCapabilities, SourceFailure, SourceId, WorkBudget};

struct Driver {
    reader: Reader,
    read_bytes: u64,
}

fn request_fact(reader: &Reader) -> (SourceId, u64, u64, u64, usize) {
    let request = reader.request();
    (
        request.source,
        request.token.generation(),
        request.token.request(),
        request.offset,
        request.maximum,
    )
}

impl Driver {
    fn new(pool: &Budget, limits: Limits) -> Self {
        Self {
            reader: Reader::new(
                pool,
                SourceId(9817),
                SourceCapabilities {
                    length: Some(INPUT.len() as u64),
                    random_access: true,
                },
                limits,
                128,
            )
            .unwrap(),
            read_bytes: 0,
        }
    }
    fn supply(&mut self) {
        let request = self.reader.request();
        let at = usize::try_from(request.offset).unwrap();
        assert!(at <= INPUT.len());
        if at == INPUT.len() {
            self.reader.finish(request.source, request.token).unwrap();
            return;
        }
        let count = request
            .maximum
            .min(InputCursor::INLINE_CAPACITY)
            .min(INPUT.len() - at);
        assert!(count != 0);
        self.reader
            .envelope
            .range_input()
            .provide_inline(
                request.source,
                request.token,
                request.offset,
                &INPUT[at..at + count],
            )
            .unwrap();
    }
    // One real work unit cannot perform both a charged range jump and a byte
    // read. Observe actual cursor deltas in the same request, not a seek receipt
    // counter. Input provision itself never advances the parser cursor.
    fn step(&mut self, cancel: bool) -> Result<ParseStep<Event>> {
        let before = self.reader.request();
        let mut work = WorkBudget::new(1);
        let result = self.reader.step_parse(&mut work, &AtomicBool::new(cancel));
        assert!(work.spent() <= 1);
        let after = self.reader.request();
        if before.token.request() == after.token.request()
            && before.token.generation() == after.token.generation()
        {
            let consumed = after.offset.checked_sub(before.offset).unwrap();
            assert!(consumed <= 1, "one real work unit read multiple bytes");
            self.read_bytes += consumed;
        }
        result
    }
    fn open(&mut self) {
        for _ in 0..1_000_000 {
            if self.reader.is_ready() {
                return;
            }
            match self.step(false).unwrap() {
                ParseStep::NeedInput(_) => self.supply(),
                ParseStep::Yield | ParseStep::Event(_) => (),
                other => panic!("unexpected opening control: {other:?}"),
            }
        }
        panic!("bounded source opening did not finish");
    }
    fn begin(&mut self) {
        self.reader.seek_for_playback(0, 8_000_000_000).unwrap();
        self.read_bytes = 0;
    }
    fn reach_recovery(&mut self) {
        for _ in 0..1_000_000 {
            if self.reader.subtitle_recovery.is_some() {
                return;
            }
            match self.step(false).unwrap() {
                ParseStep::NeedInput(_) => self.supply(),
                ParseStep::Yield
                | ParseStep::Event(Event::Envelope(_) | Event::Configuration(_)) => (),
                other => panic!("packet/receipt before recovery source validation: {other:?}"),
            }
        }
        panic!("no bounded source recovery transition");
    }
    fn seek_receipt(&mut self) -> SeekReceipt {
        for _ in 0..1_000_000 {
            match self.step(false).unwrap() {
                ParseStep::NeedInput(_) => self.supply(),
                ParseStep::Yield
                | ParseStep::Event(Event::Envelope(_) | Event::Configuration(_)) => (),
                ParseStep::Event(Event::Seek(receipt)) => return receipt,
                other => panic!("invalid recovery seek publication: {other:?}"),
            }
        }
        panic!("bounded recovery did not publish receipt");
    }
    fn prefix_packet(&mut self) -> TypedPacket {
        for _ in 0..1_000_000 {
            match self.step(false).unwrap() {
                ParseStep::NeedInput(_) => self.supply(),
                ParseStep::Yield
                | ParseStep::Event(Event::Envelope(_) | Event::Configuration(_)) => (),
                ParseStep::Event(Event::Packet(packet)) => return packet,
                other => panic!("actual subtitle prefix absent: {other:?}"),
            }
        }
        panic!("bounded prefix packet did not arrive");
    }
}

#[test]
fn subtitle_recovery_work_zero_cancel_and_reset_keep_real_cursor_and_source_identity() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let initial = pool.used();
    let mut driver = Driver::new(&pool, Limits::default());
    let control = pool.used();
    driver.open();
    driver.begin();
    driver.reach_recovery();
    let before = request_fact(&driver.reader);
    let used = pool.used();
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        driver
            .reader
            .step_parse(&mut zero, &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield | ParseStep::NeedInput(_)
    ));
    assert_eq!(zero.spent(), 0);
    assert_eq!(request_fact(&driver.reader), before);
    assert_eq!(pool.used(), used);
    assert!(matches!(driver.step(true), Err(Error::Cancelled)));
    assert_eq!(request_fact(&driver.reader), before);
    assert_eq!(pool.used(), used);
    assert!(
        driver.step(false).is_err(),
        "cancelled transaction is sticky"
    );
    driver.reader.reset().unwrap();
    assert!(driver.reader.subtitle_recovery.is_none());
    assert!(driver.reader.replay_prefix.is_none());
    assert_ne!(driver.reader.generation(), before.1);
    assert_eq!(pool.used(), control);
    drop(driver);
    assert_eq!(pool.used(), initial);
}

#[test]
fn subtitle_recovery_external_pressure_retries_without_consuming_pending_input_or_losing_owner() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let initial = pool.used();
    let mut driver = Driver::new(&pool, Limits::default());
    driver.open();
    driver.begin();
    driver.reach_recovery();
    let held = pool.reserve(pool.available()).unwrap();
    let mut blocked = false;
    for _ in 0..100_000 {
        match driver.step(false).unwrap() {
            ParseStep::NeedInput(_) => driver.supply(),
            ParseStep::Yield | ParseStep::Event(Event::Envelope(_)) => (),
            ParseStep::Backpressure { .. } => {
                blocked = true;
                break;
            }
            other => panic!("pressure concealed or published recovery output: {other:?}"),
        }
    }
    assert!(
        blocked,
        "real inline-to-owned/body allocation must encounter retained caller pressure"
    );
    let before = request_fact(&driver.reader);
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        driver
            .reader
            .step_parse(&mut zero, &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield | ParseStep::Backpressure { .. }
    ));
    assert_eq!(zero.spent(), 0);
    assert_eq!(request_fact(&driver.reader), before);
    drop(held);
    let receipt = driver.seek_receipt();
    assert_eq!(receipt.actual_ns, 8_000_000_000);
    let packet = driver.prefix_packet();
    assert_prefix(&packet);
    assert!(packet.packet.data.retained_capacity() >= packet.packet.data.len());
    drop(driver);
    assert_prefix(&packet);
    assert!(pool.used() > initial);
    drop(packet);
    assert_eq!(pool.used(), initial);
}

#[test]
fn subtitle_recovery_whole_operation_limit_is_not_refilled_for_the_extra_scan() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let mut observed = Driver::new(&pool, Limits::default());
    observed.open();
    observed.begin();
    observed.reach_recovery();
    // An independent count of real InputCursor advancement identifies work
    // already consumed by the validated original destination, not by a receipt.
    let used = observed.read_bytes;
    assert!(used > 0);
    drop(observed);
    let limits = Limits {
        scan_bytes: used + 1,
        ..Limits::default()
    };
    let mut driver = Driver::new(&pool, limits);
    driver.open();
    driver.begin();
    let mut rejected = false;
    for _ in 0..1_000_000 {
        match driver.step(false) {
            Ok(ParseStep::NeedInput(_)) => driver.supply(),
            Ok(
                ParseStep::Yield | ParseStep::Event(Event::Envelope(_) | Event::Configuration(_)),
            ) => (),
            Err(Error::Policy("operation read bytes")) => {
                rejected = true;
                break;
            }
            other => panic!("extra pass refilled its allowance or published a receipt: {other:?}"),
        }
    }
    assert!(rejected);
    // Retain the existing one-byte exhausted-header detection position; this
    // does not permit another body, Cluster or phase to reset the allowance.
    assert!(driver.read_bytes <= used + 2);
}

#[test]
fn subtitle_recovery_transport_failure_stays_failure_before_any_receipt_or_packet() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let mut driver = Driver::new(&pool, Limits::default());
    driver.open();
    driver.begin();
    driver.reach_recovery();
    let mut failed = false;
    for _ in 0..100_000 {
        match driver.step(false).unwrap() {
            ParseStep::NeedInput(request) => {
                driver
                    .reader
                    .fail(request.source, request.token, SourceFailure::Transport)
                    .unwrap();
                assert!(matches!(
                    driver.step(false),
                    Err(triergon_core::Error::Transport)
                ));
                assert!(matches!(
                    driver.step(false),
                    Err(triergon_core::Error::Transport)
                ));
                failed = true;
                break;
            }
            ParseStep::Yield | ParseStep::Event(Event::Envelope(_)) => (),
            other => panic!("transport fault escaped recovery boundary: {other:?}"),
        }
    }
    assert!(failed);
}
