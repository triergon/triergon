//! Late controller debits must preserve the original owning-event drop boundary.
use super::*;
use crate::incremental::typed_bridge::CompactEnvelopeProgress;

fn admitted_body(pool: &Budget) -> Reader {
    let mut reader = Reader::new(
        pool,
        SourceId(3013),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        Limits::default(),
        32,
    )
    .unwrap();
    let request = reader.request();
    reader
        .envelope
        .range_input()
        .provide_inline(
            request.source,
            request.token,
            0,
            &[0x1a, 0x45, 0xdf, 0xa3, 0x83],
        )
        .unwrap();
    let result = reader
        .envelope
        .step_typed_compact(&mut WorkBudget::new(64), &AtomicBool::new(false))
        .unwrap();
    let CompactEnvelopeProgress::Header(header) = result else {
        panic!("missing exact admission {result:?}")
    };
    reader
        .envelope
        .admit_typed_leaf(header, None, false)
        .unwrap();
    let request = reader.request();
    reader
        .envelope
        .range_input()
        .provide_inline(request.source, request.token, request.offset, b"abc")
        .unwrap();
    reader
}
#[test]
fn compact_controller_late_counter_overflow_drops_new_ready_leaf_before_error() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = admitted_body(&pool);
    // Exercise the existing checked controller debit after real leaf completion.
    // The source bytes/header/work still pass through the actual parser.
    reader.scan_remaining = Some(u64::MAX);
    reader.demux_read_bytes = u64::MAX;
    assert!(matches!(
        reader.step(&mut WorkBudget::new(8192), &AtomicBool::new(false)),
        Err(Error::Overflow)
    ));
    assert_eq!(reader.position(), 8);
    assert!(reader.failed);
    assert_eq!(pool.used(), baseline);
    // Error priority consumed the ready owner, exactly like dropping old progress.
    // The envelope had already completed the leaf and may now request a header.
    assert!(
        matches!(reader.envelope.step_typed_compact(&mut WorkBudget::new(0),&AtomicBool::new(false)).unwrap(),CompactEnvelopeProgress::Step(ParseStep::NeedInput(request)) if request.offset==8 && request.maximum==1)
    );
    reader.reset().unwrap();
    assert_eq!(pool.used(), baseline);
}
#[test]
fn compact_controller_read_limit_keeps_unpublished_body_owner_until_reset() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = admitted_body(&pool);
    reader.scan_remaining = Some(0);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(8192), &AtomicBool::new(false)),
        Err(Error::Policy("operation read bytes"))
    ));
    assert_eq!(reader.position(), 8);
    assert!(pool.used() > baseline);
    // This failure occurred before LeafReady: do not over-clean the old body.
    reader.reset().unwrap();
    assert_eq!(pool.used(), baseline);
}
