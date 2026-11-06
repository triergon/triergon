//! Original envelope-only source fixture and ownership/error-boundary guards.
//! No timing assertion or native oracle. Full existing typed tests stay literal.
use super::typed_bridge::{CompactElementEvent, CompactEnvelopeProgress, EnvelopeProgress};
use super::*;

// Envelope-only document: a three-byte EBML leaf, unknown Segment and Cluster,
// Timestamp0, one five-byte SimpleBlock and one ignored three-byte Void.
// This does not assert semantic validity of the EBML payload or a full track.
const WIRE: &[u8] = &[
    0x1a, 0x45, 0xdf, 0xa3, 0x83, b'd', b'o', b'c', 0x18, 0x53, 0x80, 0x67, 0xff, 0x1f, 0x43, 0xb6,
    0x75, 0xff, 0xe7, 0x81, 0, 0xa3, 0x85, 0x81, 0, 0, 0x80, b'B', 0xec, 0x83, b'a', b'b', b'c',
];
fn new(pool: &Budget, policy: PayloadOwnership) -> ElementReader {
    let mut reader = ElementReader::new(
        pool,
        SourceId(3012),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        Limits::default(),
        32,
    )
    .unwrap();
    reader.set_payload_ownership(policy);
    reader
}
fn inline(reader: &mut ElementReader, bytes: &[u8]) {
    let request = reader.request();
    reader
        .input
        .provide_inline(request.source, request.token, request.offset, bytes)
        .unwrap();
}
fn leaf_view(header: ElementHeader, data: SharedBytes) -> String {
    format!(
        "leaf {} {:?} {} {} {} {:?}",
        header.id,
        header.size,
        header.start,
        header.data,
        data.retained_capacity(),
        data.as_ref()
    )
}
fn step_view(
    reader: &mut ElementReader,
    compact: bool,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> Result<(String, Option<InputRequest>, bool)> {
    if compact {
        match reader.step_typed_compact(work, cancel)? {
            CompactEnvelopeProgress::Header(header) => {
                assert_eq!(header.generation, reader.generation());
                reader.admit_typed_leaf(header, None, false)?;
                Ok((
                    format!(
                        "admit {} {:?} {} {}",
                        header.id, header.size, header.start, header.data
                    ),
                    None,
                    false,
                ))
            }
            CompactEnvelopeProgress::Step(step) => match step {
                ParseStep::Event(CompactElementEvent::Leaf(header)) => {
                    let data = reader.take_compact_leaf(header)?;
                    Ok((leaf_view(header, data), None, false))
                }
                ParseStep::Event(CompactElementEvent::Start(header)) => {
                    assert_eq!(header.generation, reader.generation());
                    Ok((
                        format!(
                            "start {} {:?} {} {}",
                            header.id, header.size, header.start, header.data
                        ),
                        None,
                        false,
                    ))
                }
                ParseStep::Event(CompactElementEvent::Skipped(header)) => Ok((
                    format!(
                        "skip {} {:?} {} {}",
                        header.id, header.size, header.start, header.data
                    ),
                    None,
                    false,
                )),
                ParseStep::Event(CompactElementEvent::End {
                    id,
                    offset,
                    generation,
                }) => {
                    assert_eq!(generation, reader.generation());
                    Ok((format!("end {id} {offset}"), None, false))
                }
                ParseStep::NeedInput(request) => Ok((
                    format!("input {} {}", request.offset, request.maximum),
                    Some(request),
                    false,
                )),
                ParseStep::Yield => Ok(("yield".into(), None, false)),
                ParseStep::Backpressure {
                    requested,
                    available,
                } => Ok((format!("pressure {requested} {available}"), None, false)),
                ParseStep::Finished => Ok(("finished".into(), None, true)),
            },
        }
    } else {
        match reader.step_typed_envelope(work, cancel)? {
            EnvelopeProgress::Header(header) => {
                assert_eq!(header.generation, reader.generation());
                reader.admit_typed_leaf(header, None, false)?;
                Ok((
                    format!(
                        "admit {} {:?} {} {}",
                        header.id, header.size, header.start, header.data
                    ),
                    None,
                    false,
                ))
            }
            EnvelopeProgress::Step(step) => match step {
                ParseStep::Event(ElementEvent::Leaf { header, data }) => {
                    Ok((leaf_view(header, data), None, false))
                }
                ParseStep::Event(ElementEvent::Start(header)) => {
                    assert_eq!(header.generation, reader.generation());
                    Ok((
                        format!(
                            "start {} {:?} {} {}",
                            header.id, header.size, header.start, header.data
                        ),
                        None,
                        false,
                    ))
                }
                ParseStep::Event(ElementEvent::Skipped(header)) => Ok((
                    format!(
                        "skip {} {:?} {} {}",
                        header.id, header.size, header.start, header.data
                    ),
                    None,
                    false,
                )),
                ParseStep::Event(ElementEvent::End {
                    id,
                    offset,
                    generation,
                }) => {
                    assert_eq!(generation, reader.generation());
                    Ok((format!("end {id} {offset}"), None, false))
                }
                ParseStep::NeedInput(request) => Ok((
                    format!("input {} {}", request.offset, request.maximum),
                    Some(request),
                    false,
                )),
                ParseStep::Yield => Ok(("yield".into(), None, false)),
                ParseStep::Backpressure {
                    requested,
                    available,
                } => Ok((format!("pressure {requested} {available}"), None, false)),
                ParseStep::Finished => Ok(("finished".into(), None, true)),
            },
        }
    }
}
fn trace(
    compact: bool,
    quantum: u64,
    chunk: usize,
    wire: &[u8],
    policy: PayloadOwnership,
    owned: bool,
) -> Vec<String> {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let mut reader = new(&pool, policy);
    let mut rows = Vec::new();
    let mut ended = false;
    for at in 0..4096 {
        let mut work = WorkBudget::new(if at % 3 == 0 { 0 } else { quantum });
        let result = step_view(&mut reader, compact, &mut work, &AtomicBool::new(false));
        let (view, request, done) = match result {
            Ok(result) => result,
            Err(error) => {
                rows.push(format!(
                    "error {error:?} {} {} {} {}",
                    reader.position(),
                    work.spent(),
                    reader.scan_left,
                    pool.used() - baseline
                ));
                return rows;
            }
        };
        rows.push(format!(
            "{view} pos={} work={} scan={} live={}",
            reader.position(),
            work.spent(),
            reader.scan_left,
            pool.used() - baseline
        ));
        if done {
            return rows;
        }
        if let Some(request) = request {
            assert_eq!(request.source, reader.input.source());
            assert_eq!(request.token, reader.input.token());
            let offset = request.offset as usize;
            if offset == wire.len() {
                assert!(!ended, "repeated input after explicit finish");
                reader.finish(request.source, request.token).unwrap();
                ended = true;
            } else {
                let count = chunk.min(request.maximum).min(wire.len() - offset);
                assert!(count != 0);
                if owned {
                    let mut raw = Vec::with_capacity(count + 9);
                    raw.extend_from_slice(b"pre");
                    raw.extend_from_slice(&wire[offset..offset + count]);
                    raw.extend_from_slice(b"suffix");
                    let root = Bytes::try_from_vec(&pool, raw).unwrap();
                    let view = root.slice(3..3 + count).unwrap();
                    reader
                        .input
                        .provide_owned(
                            InputWindow::new(request.source, request.token, request.offset, view)
                                .unwrap(),
                        )
                        .unwrap();
                } else {
                    inline(&mut reader, &wire[offset..offset + count]);
                }
            }
        }
    }
    panic!("bounded trace stalled")
}

#[test]
fn compact_envelope_preserves_all_request_work_scan_and_storage_rows() {
    for policy in [
        PayloadOwnership::OwnedExact,
        PayloadOwnership::SharedBounded,
    ] {
        for quantum in [1, 7, 8192] {
            for chunk in [1, 3, 17] {
                for owned in [false, true] {
                    assert_eq!(
                        trace(false, quantum, chunk, WIRE, policy, owned),
                        trace(true, quantum, chunk, WIRE, policy, owned),
                        "{policy:?}/{quantum}/{chunk}/{owned}"
                    );
                }
            }
        }
    }
}
#[test]
fn compact_envelope_all_truncations_preserve_failure_and_request_positions() {
    for cut in 0..=WIRE.len() {
        assert_eq!(
            trace(
                false,
                7,
                3,
                &WIRE[..cut],
                PayloadOwnership::OwnedExact,
                false
            ),
            trace(
                true,
                7,
                3,
                &WIRE[..cut],
                PayloadOwnership::OwnedExact,
                false
            ),
            "cut {cut}"
        );
    }
}
fn first_leaf(reader: &mut ElementReader, payload: &[u8]) -> ElementHeader {
    let mut prefix = vec![0x1a, 0x45, 0xdf, 0xa3, 0x80 | (payload.len() as u8)];
    prefix.extend_from_slice(payload);
    inline(reader, &prefix);
    for _ in 0..16 {
        match reader
            .step_typed_compact(&mut WorkBudget::new(8192), &AtomicBool::new(false))
            .unwrap()
        {
            CompactEnvelopeProgress::Header(header) => {
                reader.admit_typed_leaf(header, None, false).unwrap()
            }
            CompactEnvelopeProgress::Step(ParseStep::Event(CompactElementEvent::Leaf(header))) => {
                return header;
            }
            CompactEnvelopeProgress::Step(ParseStep::Yield) => (),
            other => panic!("unexpected first leaf {other:?}"),
        }
    }
    panic!("leaf not ready")
}
#[test]
fn compact_envelope_ready_owns_exact_bytes_and_take_is_single_use() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = new(&pool, PayloadOwnership::OwnedExact);
    let header = first_leaf(&mut reader, b"abc");
    let held = pool.used();
    assert!(held > baseline);
    let mut wrong = header;
    wrong.start += 1;
    assert!(matches!(
        reader.take_compact_leaf(wrong),
        Err(Error::Invalid("compact leaf identity changed"))
    ));
    assert_eq!(pool.used(), held);
    let mut zero = WorkBudget::new(0);
    assert!(
        matches!(reader.step_typed_compact(&mut zero, &AtomicBool::new(false)).unwrap(),
        CompactEnvelopeProgress::Step(ParseStep::Event(CompactElementEvent::Leaf(actual))) if actual==header)
    );
    assert_eq!(zero.spent(), 0);
    assert_eq!(pool.used(), held);
    let bytes = reader.take_compact_leaf(header).unwrap();
    assert_eq!(bytes.as_ref(), b"abc");
    assert_eq!(bytes.retained_capacity(), 3);
    assert!(reader.take_compact_leaf(header).is_err());
    let alias = bytes.clone();
    reader.reset().unwrap();
    drop(reader);
    drop(bytes);
    assert_eq!(pool.used(), held);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn compact_envelope_cancel_at_ready_is_sticky_and_reset_releases_owner() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = new(&pool, PayloadOwnership::OwnedExact);
    first_leaf(&mut reader, b"abc");
    let held = pool.used();
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        reader.step_typed_compact(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        reader.step_typed_compact(&mut WorkBudget::new(8192), &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), held);
    reader.reset().unwrap();
    assert_eq!(pool.used(), baseline);
}
#[test]
fn compact_envelope_transport_at_ready_precedes_cancel_and_reset_changes_generation() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = new(&pool, PayloadOwnership::OwnedExact);
    first_leaf(&mut reader, b"abc");
    let old = reader.request();
    let held = pool.used();
    let old_generation = old.token.generation();
    reader
        .fail(old.source, old.token, SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        reader.step_typed_compact(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Core(triergon_core::Error::Transport))
    ));
    assert_eq!(pool.used(), held);
    let next = reader.reset().unwrap();
    assert_ne!(next.token.generation(), old_generation);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn compact_envelope_discard_drops_the_pending_leaf_at_old_error_boundary() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = new(&pool, PayloadOwnership::OwnedExact);
    first_leaf(&mut reader, b"abc");
    assert!(pool.used() > baseline);
    reader.discard_compact_leaf();
    assert_eq!(pool.used(), baseline);
    assert!(matches!(reader.phase, Phase::Between));
    reader.discard_compact_leaf();
    assert_eq!(pool.used(), baseline);
}
#[test]
fn compact_envelope_freeze_pressure_retains_builder_and_retries_once() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = new(&pool, PayloadOwnership::OwnedExact);
    inline(
        &mut reader,
        &[0x1a, 0x45, 0xdf, 0xa3, 0x83, b'a', b'b', b'c'],
    );
    for _ in 0..128 {
        if matches!(&reader.phase, Phase::Body(body) if body.remaining==0 && body.span.is_none() && body.direct.is_none() && body.allocated)
        {
            break;
        }
        match reader
            .step_typed_compact(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CompactEnvelopeProgress::Header(header) => {
                reader.admit_typed_leaf(header, None, false).unwrap()
            }
            CompactEnvelopeProgress::Step(ParseStep::Yield) => (),
            other => panic!("unexpected before freeze {other:?}"),
        }
    }
    assert!(
        matches!(&reader.phase, Phase::Body(body) if body.buffer.as_ref().is_some_and(|b| b.as_slice()==b"abc") && body.direct.is_none())
    );
    let blocker = pool.reserve(pool.available()).unwrap();
    let held = pool.used();
    let mut one = WorkBudget::new(1);
    assert!(matches!(
        reader
            .step_typed_compact(&mut one, &AtomicBool::new(false))
            .unwrap(),
        CompactEnvelopeProgress::Step(ParseStep::Backpressure { .. })
    ));
    assert_eq!(one.spent(), 1);
    assert_eq!(pool.used(), held);
    assert!(
        matches!(&reader.phase, Phase::Body(body) if body.transfer!=BodyTransfer::Ready && body.buffer.as_ref().is_some_and(|b| b.as_slice()==b"abc"))
    );
    drop(blocker);
    let result = reader
        .step_typed_compact(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap();
    let CompactEnvelopeProgress::Step(ParseStep::Event(CompactElementEvent::Leaf(header))) = result
    else {
        panic!("retry did not publish {result:?}")
    };
    let bytes = reader.take_compact_leaf(header).unwrap();
    assert_eq!(bytes.as_ref(), b"abc");
    drop(bytes);
    drop(reader);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn compact_envelope_progress_has_no_leaf_owner_and_uses_bounded_existing_state() {
    assert_eq!(size_of::<BodyTransfer>(), 1);
    assert!(size_of::<CompactEnvelopeProgress>() < size_of::<EnvelopeProgress>());
    assert!(size_of::<CompactElementEvent>() <= size_of::<ElementHeader>() + size_of::<usize>());
    println!(
        "COMPACT_ENVELOPE_LAYOUT old={} new={} body={} header={} transfer={}",
        size_of::<EnvelopeProgress>(),
        size_of::<CompactEnvelopeProgress>(),
        size_of::<BodyState>(),
        size_of::<ElementHeader>(),
        size_of::<BodyTransfer>()
    );
}

#[test]
fn compact_envelope_shared_hidden_parent_is_charged_once_through_reset_and_aliases() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(3012),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        Limits::default(),
        128,
    )
    .unwrap();
    reader.set_payload_ownership(PayloadOwnership::SharedBounded);
    let mut raw = Vec::with_capacity(128);
    raw.extend_from_slice(b"pre");
    raw.extend_from_slice(WIRE);
    raw.extend_from_slice(b"suffix");
    let root = Bytes::try_from_vec(&pool, raw).unwrap();
    let charge = root.retained_storage_bytes();
    let req = reader.request();
    reader
        .input
        .provide_owned(
            InputWindow::new(
                req.source,
                req.token,
                0,
                root.slice(3..3 + WIRE.len()).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    drop(root);
    let mut found = None;
    for _ in 0..64 {
        match reader
            .step_typed_compact(&mut WorkBudget::new(8192), &AtomicBool::new(false))
            .unwrap()
        {
            CompactEnvelopeProgress::Header(header) => {
                reader.admit_typed_leaf(header, None, false).unwrap()
            }
            CompactEnvelopeProgress::Step(ParseStep::Event(CompactElementEvent::Leaf(header))) => {
                if header.id == 0xa3 {
                    found = Some(header);
                    break;
                }
                drop(reader.take_compact_leaf(header).unwrap());
            }
            CompactEnvelopeProgress::Step(
                ParseStep::Event(CompactElementEvent::Start(_)) | ParseStep::Yield,
            ) => (),
            other => panic!("unexpected hidden source progress {other:?}"),
        }
    }
    let header = found.expect("actual block leaf");
    assert_eq!((header.start, header.data, header.size), (21, 23, Some(5)));
    assert_eq!(pool.used(), baseline + charge); // cursor and body alias ONE root.
    let data = reader.take_compact_leaf(header).unwrap();
    let alias = data.clone();
    assert_eq!(data.retained_capacity(), 128);
    reader.reset().unwrap();
    drop(reader);
    assert_eq!(pool.used(), baseline + charge);
    drop(data);
    assert_eq!(alias.as_ref(), &[0x81, 0, 0, 0x80, b'B']);
    assert_eq!(pool.used(), baseline + charge);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}
