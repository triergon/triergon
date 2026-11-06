//! Original empty/fragmented envelope controls for lazy empty-builder ownership.
//! These assert public bytes, work, errors and charges, not an Option layout.
use super::*;

const EMPTY: &[u8] = &[0x1a, 0x45, 0xdf, 0xa3, 0x80];

fn new(pool: &Budget) -> ElementReader {
    ElementReader::new(
        pool,
        SourceId(4016),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        Limits::default(),
        32,
    )
    .unwrap()
}

fn before_empty_publication(pool: &Budget) -> ElementReader {
    let mut reader = new(pool);
    let request = reader.request();
    reader
        .input
        .provide_inline(request.source, request.token, request.offset, EMPTY)
        .unwrap();
    let mut spent = 0;
    for _ in 0..32 {
        let mut work = WorkBudget::new(1);
        let step = reader.step(&mut work, &AtomicBool::new(false)).unwrap();
        spent += work.spent();
        assert!(matches!(step, ParseStep::Yield));
        if matches!(&reader.phase, Phase::Body(body) if body.allocated && body.remaining==0) {
            assert_eq!(reader.position(), EMPTY.len() as u64);
            assert_eq!(spent, 7); // five input bytes, header parse, output admission
            return reader;
        }
    }
    panic!("empty publication boundary stalled");
}

fn empty_leaf(reader: &mut ElementReader, work: &mut WorkBudget) -> SharedBytes {
    match reader.step(work, &AtomicBool::new(false)).unwrap() {
        ParseStep::Event(ElementEvent::Leaf { header, data }) => {
            assert_eq!(header.id, EBML);
            assert_eq!(header.size, Some(0));
            assert_eq!(header.start, 0);
            assert_eq!(header.data, 5);
            assert!(data.as_ref().is_empty());
            assert_eq!(data.retained_capacity(), 0);
            data
        }
        other => panic!("empty leaf not published: {other:?}"),
    }
}

#[test]
fn lazy_body_empty_leaf_keeps_one_publication_unit_and_immutable_owner() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = before_empty_publication(&pool);
    assert_eq!(pool.used(), baseline);
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut zero, &AtomicBool::new(false)).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(zero.spent(), 0);
    let mut one = WorkBudget::new(1);
    let data = empty_leaf(&mut reader, &mut one);
    assert_eq!(one.spent(), 1);
    let held = pool.used();
    assert!(held > baseline); // zero visible capacity still owns a charged header
    let alias = data.clone();
    reader.reset().unwrap();
    drop(reader);
    drop(data);
    assert_eq!(pool.used(), held);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn lazy_body_empty_freeze_pressure_recovers_then_publishes_exact_zero_capacity() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = before_empty_publication(&pool);
    let blocker = pool.reserve(pool.available()).unwrap();
    let held = pool.used();
    let mut one = WorkBudget::new(1);
    assert!(matches!(
        reader.step(&mut one, &AtomicBool::new(false)).unwrap(),
        ParseStep::Backpressure { available: 0, .. }
    ));
    assert_eq!(one.spent(), 1);
    assert_eq!(reader.position(), 5);
    assert_eq!(pool.used(), held);
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut zero, &AtomicBool::new(false)).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(zero.spent(), 0);
    assert_eq!(pool.used(), held);
    drop(blocker);
    let mut one = WorkBudget::new(1);
    let data = empty_leaf(&mut reader, &mut one);
    assert_eq!(one.spent(), 1);
    drop(data);
    drop(reader);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn lazy_body_empty_freeze_observes_sticky_cancel_before_work_or_pressure() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = before_empty_publication(&pool);
    let blocker = pool.reserve(pool.available()).unwrap();
    let cancel = AtomicBool::new(true);
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut zero, &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(zero.spent(), 0);
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    reader.reset().unwrap();
    drop(blocker);
    drop(reader);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn lazy_body_empty_freeze_transport_wins_cancel_and_reset_rejects_old_generation() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = before_empty_publication(&pool);
    let old = reader.request();
    let old_generation = old.token.generation();
    reader
        .fail(old.source, old.token, SourceFailure::Transport)
        .unwrap();
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut zero, &AtomicBool::new(true)),
        Err(triergon_core::Error::Transport)
    ));
    assert_eq!(zero.spent(), 0);
    let next = reader.reset().unwrap();
    assert_ne!(next.token.generation(), old_generation);
    drop(reader);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn lazy_body_fragmented_owned_exact_bytes_keep_literal_copy_work_and_alias_charge() {
    const WIRE: &[u8] = &[0x1a, 0x45, 0xdf, 0xa3, 0x83, b'a', b'b', b'c'];
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = new(&pool);
    let mut spent = 0;
    let mut supplied = 0;
    for _ in 0..128 {
        let mut work = WorkBudget::new(1);
        let step = reader.step(&mut work, &AtomicBool::new(false)).unwrap();
        spent += work.spent();
        match step {
            ParseStep::NeedInput(request) => {
                assert_eq!(request.offset as usize, supplied);
                assert!(request.maximum > 0 && supplied < WIRE.len());
                reader
                    .input
                    .provide_inline(
                        request.source,
                        request.token,
                        request.offset,
                        &WIRE[supplied..supplied + 1],
                    )
                    .unwrap();
                supplied += 1;
            }
            ParseStep::Yield => (),
            ParseStep::Event(ElementEvent::Leaf { header, data }) => {
                assert_eq!(header.size, Some(3));
                assert_eq!(header.data, 5);
                assert_eq!(reader.position(), 8);
                assert_eq!(supplied, 8);
                assert_eq!(spent, 14); // eight reads, header/admission/publication, three copies
                assert_eq!(data.as_ref(), b"abc");
                assert_eq!(data.retained_capacity(), 3);
                let alias = data.clone();
                let held = pool.used();
                reader.reset().unwrap();
                drop(reader);
                drop(data);
                assert_eq!(pool.used(), held);
                assert_eq!(alias.as_ref(), b"abc");
                drop(alias);
                assert_eq!(pool.used(), baseline);
                return;
            }
            other => panic!("unexpected fragmented envelope: {other:?}"),
        }
    }
    panic!("fragmented body stalled");
}

// Test-only conservative original workspace; all observations use Reader.step.
pub(crate) fn original_empty_workspace(reader: &mut ElementReader) {
    let Phase::Body(body) = &mut reader.phase else {
        panic!("no admitted body")
    };
    assert!(
        body.buffer
            .as_ref()
            .is_none_or(|b| b.is_empty() && b.capacity() == 0)
    );
    body.buffer = Some(BudgetVec::new(reader.input.budget()));
}
