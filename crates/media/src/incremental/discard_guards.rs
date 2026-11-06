//! Independent envelope guards; inline delivery is the blocking adapter's real seam.
use super::*;
use triergon_core::SourceFailure;

// Empty EBML envelope, unknown-sized Segment, three-byte leaf. This is an
// envelope fixture, not a claim of a complete Matroska document/track profile.
fn body(pool: &Budget, id: u8, limits: Limits) -> ElementReader {
    let mut reader = ElementReader::new(
        pool,
        SourceId(1802),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        limits,
        32,
    )
    .unwrap();
    let prefix = [
        0x1a, 0x45, 0xdf, 0xa3, 0x80, 0x18, 0x53, 0x80, 0x67, 0xff, id, 0x83,
    ];
    let request = reader.request();
    reader
        .input
        .provide_inline(request.source, request.token, 0, &prefix)
        .unwrap();
    for _ in 0..8 {
        match reader
            .step(&mut WorkBudget::new(64), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(ElementEvent::Leaf { header, data }) => {
                assert_eq!(header.id, EBML);
                assert!(data.is_empty());
            }
            ParseStep::Event(ElementEvent::Start(header)) => assert_eq!(header.id, SEGMENT),
            ParseStep::NeedInput(request) => {
                assert_eq!(request.offset, 12);
                assert_eq!(request.maximum, 3);
                assert_eq!(reader.scan_left, limits.scan_bytes - 12);
                return reader;
            }
            other => panic!("unexpected prefix state: {other:?}"),
        }
    }
    panic!("prefix did not reach bounded leaf body")
}
fn supply(reader: &mut ElementReader, bytes: &[u8]) {
    let request = reader.request();
    reader
        .input
        .provide_inline(request.source, request.token, request.offset, bytes)
        .unwrap();
}
fn expected(reader: &ElementReader, id: u64) -> ElementHeader {
    ElementHeader {
        id,
        size: Some(3),
        start: 10,
        data: 12,
        generation: reader.generation(),
    }
}

#[test]
fn ignored_inline_body_emits_exact_skipped_header_with_no_available_pool() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = body(&pool, 0xec, Limits::default());
    supply(&mut reader, b"abc");
    let token = reader.request().token;
    let header = expected(&reader, 0xec);
    let blocker = pool.reserve(pool.available()).unwrap();
    let held = pool.used();
    for position in 13..=15 {
        let mut work = WorkBudget::new(1);
        assert!(matches!(
            reader.step(&mut work, &AtomicBool::new(false)).unwrap(),
            ParseStep::Yield
        ));
        assert_eq!(reader.position(), position);
        assert_eq!(reader.request().token, token);
        assert_eq!(work.spent(), 1);
        assert_eq!(pool.used(), held);
        assert!(reader.input.retained_window().is_none());
    }
    let mut work = WorkBudget::new(1);
    assert!(
        matches!(reader.step(&mut work, &AtomicBool::new(false)).unwrap(), ParseStep::Event(ElementEvent::Skipped(actual)) if actual == header)
    );
    assert_eq!(work.spent(), 1);
    assert_eq!(reader.scan_left, Limits::default().scan_bytes - 15);
    drop(blocker);
    drop(reader);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn ignored_owned_hidden_window_releases_cursor_charge_but_preserves_alias() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = body(&pool, 0xec, Limits::default());
    let mut source = Vec::with_capacity(64);
    source.extend_from_slice(b"preabcpost");
    let root = Bytes::try_from_vec(&pool, source).unwrap();
    let charge = root.retained_storage_bytes();
    let alias = root.slice(3..6).unwrap();
    let request = reader.request();
    reader
        .input
        .provide_owned(
            InputWindow::new(
                request.source,
                request.token,
                request.offset,
                root.slice(3..6).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    drop(root);
    let blocker = pool.reserve(pool.available()).unwrap();
    let held = pool.used();
    let header = expected(&reader, 0xec);
    let mut work = WorkBudget::new(4);
    assert!(
        matches!(reader.step(&mut work, &AtomicBool::new(false)).unwrap(), ParseStep::Event(ElementEvent::Skipped(actual)) if actual == header)
    );
    assert_eq!(reader.position(), 15);
    assert_eq!(work.spent(), 4);
    assert_eq!(pool.used(), held);
    assert!(reader.input.retained_window().is_none());
    drop(blocker);
    drop(reader);
    assert_eq!(pool.used(), baseline + charge);
    assert_eq!(alias.as_ref(), b"abc");
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn ignored_body_work_zero_one_requests_and_explicit_eof_are_distinct() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = body(&pool, 0xec, Limits::default());
    let token = reader.request().token;
    let mut zero = WorkBudget::new(0);
    assert!(
        matches!(reader.step(&mut zero, &AtomicBool::new(false)).unwrap(), ParseStep::NeedInput(request) if request.offset == 12 && request.maximum == 3 && request.token == token)
    );
    supply(&mut reader, b"a");
    assert!(matches!(
        reader.step(&mut zero, &AtomicBool::new(false)).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(reader.position(), 12);
    let mut one = WorkBudget::new(1);
    assert!(
        matches!(reader.step(&mut one, &AtomicBool::new(false)).unwrap(), ParseStep::NeedInput(request) if request.offset == 13 && request.maximum == 2 && request.token == token)
    );
    assert_eq!(one.spent(), 1);
    supply(&mut reader, b"bc");
    let header = expected(&reader, 0xec);
    assert!(
        matches!(reader.step(&mut WorkBudget::new(3), &AtomicBool::new(false)).unwrap(), ParseStep::Event(ElementEvent::Skipped(actual)) if actual == header)
    );
    assert!(
        matches!(reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)).unwrap(), ParseStep::NeedInput(request) if request.offset == 15 && request.maximum == 1)
    );
    reader
        .finish(SourceId(1802), reader.request().token)
        .unwrap();
    assert!(matches!(
        reader.step(&mut zero, &AtomicBool::new(false)).unwrap(),
        ParseStep::Event(ElementEvent::End {
            id: SEGMENT,
            offset: 15,
            ..
        })
    ));
    assert!(matches!(
        reader.step(&mut zero, &AtomicBool::new(false)).unwrap(),
        ParseStep::Finished
    ));
}

#[test]
fn ignored_body_transport_precedes_cancel_and_empty_work_without_advancing() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = body(&pool, 0xec, Limits::default());
    supply(&mut reader, b"abc");
    reader
        .fail(
            SourceId(1802),
            reader.request().token,
            SourceFailure::Transport,
        )
        .unwrap();
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut work, &AtomicBool::new(true)),
        Err(Error::Core(triergon_core::Error::Transport))
    ));
    assert_eq!(reader.position(), 12);
    assert_eq!(work.spent(), 0);
}

#[test]
fn ignored_body_cancellation_releases_input_without_emitting_skipped() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = body(&pool, 0xec, Limits::default());
    supply(&mut reader, b"abc");
    let mut work = WorkBudget::new(4);
    assert!(matches!(
        reader.step(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(reader.position(), 12);
    assert_eq!(work.spent(), 0);
    assert!(reader.input.retained_window().is_none());
    assert!(matches!(
        reader.step(&mut work, &AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
}

#[test]
fn explicit_short_body_is_truncated_after_exact_bytes_and_scan_work() {
    let pool = Budget::new(4096).unwrap();
    let limits = Limits::default();
    let mut reader = body(&pool, 0xec, limits);
    supply(&mut reader, b"ab");
    reader
        .finish(SourceId(1802), reader.request().token)
        .unwrap();
    let mut work = WorkBudget::new(10);
    assert!(matches!(
        reader.step(&mut work, &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
    assert_eq!(reader.position(), 14);
    assert_eq!(reader.scan_left, limits.scan_bytes - 14);
    assert_eq!(work.spent(), 2);
    assert!(matches!(
        reader.step(&mut work, &AtomicBool::new(false)),
        Err(Error::Invalid("reader requires explicit reset after error"))
    ));
}

#[test]
fn skipped_block_outside_cluster_does_not_refill_scan_budget() {
    let pool = Budget::new(4096).unwrap();
    let limits = Limits {
        scan_bytes: 15,
        ..Limits::default()
    };
    let mut reader = body(&pool, 0xa3, limits);
    supply(&mut reader, b"abc");
    let header = expected(&reader, 0xa3);
    assert!(
        matches!(reader.step(&mut WorkBudget::new(4), &AtomicBool::new(false)).unwrap(), ParseStep::Event(ElementEvent::Skipped(actual)) if actual == header)
    );
    assert_eq!(reader.scan_left, 0);
    supply(&mut reader, &[0xec]);
    let mut work = WorkBudget::new(5);
    assert!(matches!(
        reader.step(&mut work, &AtomicBool::new(false)),
        Err(Error::Limit("operation read bytes"))
    ));
    assert_eq!(reader.position(), 16);
    assert_eq!(work.spent(), 1);
}
