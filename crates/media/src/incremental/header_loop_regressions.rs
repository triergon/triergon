//! Authored source/work/health guards for the existing envelope engine.
use super::*;
use crate::ebml_kit::element;
use triergon_core::{Budget, Bytes, InputWindow};

fn fixture(width: usize) -> Vec<u8> {
    let mut out = element(0x1a45dfa3, &element(0x4282, b"matroska"));
    out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
    // Unknown-sized Segment, authored for all eight size-width representations.
    let mut size = vec![0xff; width];
    size[0] = ((1_u16 << (9 - width)) - 1) as u8;
    out.extend_from_slice(&size);
    out.extend_from_slice(&element(
        0x1f43b675,
        &[
            element(0xe7, &[0]),
            element(0xa3, &[0x81, 0, 0, 0x80, b'a', b'b', b'c']),
        ]
        .concat(),
    ));
    out
}
fn reader(pool: &Budget) -> ElementReader {
    ElementReader::new(
        pool,
        SourceId(9160),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap()
}
fn provide_prefix(reader: &mut ElementReader, pool: &Budget, source: &[u8], chunk: usize) {
    // Deliberately use the public core-window request, not a forged widened
    // parser hint. The same source/token/offset and actual supplied prefix are
    // checked by provide. This exercises already-owned multi-part headers.
    let request = reader.request();
    let at = request.offset as usize;
    if at == source.len() {
        reader.finish(request.source, request.token).unwrap();
    } else {
        let end = source.len().min(at + chunk.min(request.maximum));
        assert!(end > at);
        let bytes = Bytes::copy_from_slice(pool, &source[at..end]).unwrap();
        let window =
            InputWindow::new(request.source, request.token, request.offset, bytes).unwrap();
        reader.provide(&window).unwrap();
    }
}

#[test]
fn header_loop_widths_fragmentation_and_literal_work_keep_source_and_owners() {
    for width in 1..=8 {
        for chunk in [1, 7, 8192] {
            for quantum in [1, 7, 8192] {
                let source = fixture(width);
                let pool = Budget::new(64 * 1024).unwrap();
                let base = pool.used();
                let mut reader = reader(&pool);
                let cancel = AtomicBool::new(false);
                let mut held = Vec::new();
                let mut blocks = Vec::new();
                let mut trace = Vec::new();
                for turn in 0..10000 {
                    let allowance = if turn % 3 == 0 { 0 } else { quantum };
                    let mut work = WorkBudget::new(allowance);
                    let before = reader.position();
                    let step = reader.step(&mut work, &cancel).unwrap();
                    assert!(work.spent() <= allowance);
                    let fact = match step {
                        ParseStep::NeedInput(request) => {
                            let core = reader.request();
                            assert_eq!(request.source, core.source);
                            assert_eq!(request.token, core.token);
                            assert_eq!(request.offset, core.offset);
                            let text = format!(
                                "N:{}:{}:{}:{}",
                                request.offset,
                                request.maximum,
                                request.token.generation(),
                                request.token.request()
                            );
                            provide_prefix(&mut reader, &pool, &source, chunk);
                            text
                        }
                        ParseStep::Event(ElementEvent::Start(h)) => {
                            if h.id == 0x18538067 {
                                assert_eq!(h.size, None);
                            }
                            format!(
                                "S:{}:{}:{}:{:?}:{}",
                                h.id, h.start, h.data, h.size, h.generation
                            )
                        }
                        ParseStep::Event(ElementEvent::Leaf { header: h, data }) => {
                            assert_eq!(h.size, Some(data.len() as u64));
                            if h.id == 0xa3 {
                                blocks.push(data.as_ref().to_vec());
                            }
                            let text = format!(
                                "L:{}:{}:{}:{:?}:{}:{:?}",
                                h.id,
                                h.start,
                                h.data,
                                h.size,
                                h.generation,
                                data.as_ref()
                            );
                            held.push(data);
                            text
                        }
                        ParseStep::Event(ElementEvent::Skipped(h)) => {
                            format!("K:{}:{}:{}:{:?}", h.id, h.start, h.data, h.size)
                        }
                        ParseStep::Event(ElementEvent::End {
                            id,
                            offset,
                            generation,
                        }) => format!("E:{id}:{offset}:{generation}"),
                        ParseStep::Yield => "Y".to_owned(),
                        ParseStep::Backpressure { .. } => panic!("ample authored pool"),
                        ParseStep::Finished => {
                            trace.push(format!(
                                "F:{before}:{}:{}",
                                reader.position(),
                                work.spent()
                            ));
                            break;
                        }
                    };
                    trace.push(format!(
                        "{fact}:{before}:{}:{}",
                        reader.position(),
                        work.spent()
                    ));
                    assert!(turn < 9999, "bounded authored stream must finish");
                }
                assert_eq!(blocks, [vec![0x81, 0, 0, 0x80, b'a', b'b', b'c']]);
                assert_eq!(reader.position(), source.len() as u64);
                drop(reader);
                assert!(pool.used() > base);
                drop(held);
                assert_eq!(pool.used(), base);
                println!("HEADER_LOOP_TRACE width={width} chunk={chunk} work={quantum} {trace:?}");
            }
        }
    }
}

#[test]
fn header_loop_malformed_or_partial_header_keeps_exact_error_position() {
    let cases: &[(&[u8], u64, Option<&str>)] = &[
        (&[0], 1, Some("zero VINT marker")),
        (&[0x08], 1, Some("element ID width")),
        (&[0x1a], 1, None),
        (&[0x1a, 0x45], 2, None),
        (&[0x1a, 0x45, 0xdf, 0xa3], 4, None),
        (&[0x1a, 0x45, 0xdf, 0xa3, 0x40], 5, None),
        (&[0x1a, 0x45, 0xdf, 0xa3, 0], 5, Some("zero VINT marker")),
    ];
    for &(source, position, invalid) in cases {
        for quantum in [1, 7, 8192] {
            let pool = Budget::new(32 * 1024).unwrap();
            let base = pool.used();
            let mut reader = reader(&pool);
            let cancel = AtomicBool::new(false);
            let mut trace = Vec::new();
            let error = (0..128)
                .find_map(|_| {
                    let mut work = WorkBudget::new(quantum);
                    let step = reader.step(&mut work, &cancel);
                    trace.push(format!("{}:{}", reader.position(), work.spent()));
                    match step {
                        Err(error) => Some(error),
                        Ok(ParseStep::NeedInput(_)) => {
                            provide_prefix(&mut reader, &pool, source, 8192);
                            None
                        }
                        Ok(ParseStep::Yield) => None,
                        other => panic!("unexpected partial-header result: {other:?}"),
                    }
                })
                .expect("partial or malformed header must refuse");
            match (invalid, error) {
                (Some(text), Error::Invalid(actual)) => assert_eq!(text, actual),
                (None, Error::Truncated) => (),
                (_, other) => panic!("wrong header error: {other:?}"),
            }
            assert_eq!(reader.position(), position);
            drop(reader);
            assert_eq!(pool.used(), base);
            println!(
                "HEADER_LOOP_ERROR source={source:?} work={quantum} invalid={invalid:?} {trace:?}"
            );
        }
    }
}

#[test]
fn header_loop_partial_header_transport_precedes_cancel_and_reset_rejects_stale() {
    let pool = Budget::new(32 * 1024).unwrap();
    let base = pool.used();
    let mut reader = reader(&pool);
    let cancel = AtomicBool::new(false);
    provide_prefix(&mut reader, &pool, &[0x1a, 0x45], 8192);
    let ParseStep::NeedInput(old) = reader.step(&mut WorkBudget::new(32), &cancel).unwrap() else {
        panic!("the unfinished four-byte ID needs a real prefix");
    };
    assert_eq!(old.offset, 2);
    reader
        .fail(old.source, old.token.clone(), SourceFailure::Transport)
        .unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(0), &cancel),
        Err(Error::Transport)
    ));
    reader.reset().unwrap();
    cancel.store(false, Ordering::Relaxed);
    let ParseStep::NeedInput(current) = reader.step(&mut WorkBudget::new(1), &cancel).unwrap()
    else {
        panic!("reset must request new source data");
    };
    assert_eq!(current.offset, 0);
    assert!(current.token.generation() > old.token.generation());
    let bytes = Bytes::copy_from_slice(&pool, &[0x1a, 0x45]).unwrap();
    let stale = InputWindow::new(old.source, old.token, current.offset, bytes).unwrap();
    assert!(reader.provide(&stale).is_err());
    assert_eq!(reader.position(), 0);
    drop(stale);
    drop(reader);
    assert_eq!(pool.used(), base);
}

#[test]
fn header_loop_owned_width_bits_need_no_work_but_final_header_needs_one_unit() {
    use super::typed_bridge::CompactEnvelopeProgress;
    let pool = Budget::new(32 * 1024).unwrap();
    let base = pool.used();
    let mut reader = reader(&pool);
    let cancel = AtomicBool::new(false);
    // Four-byte ID, two-byte size, two bytes of body. Only the envelope header
    // is admitted here; this is not a complete semantic EBML document test.
    provide_prefix(
        &mut reader,
        &pool,
        &[0x1a, 0x45, 0xdf, 0xa3, 0x40, 2, b'x', b'y'],
        8192,
    );
    let pressure = pool.reserve(pool.available()).unwrap();
    let mut five = WorkBudget::new(5);
    assert!(matches!(
        reader.step_typed_compact(&mut five, &cancel).unwrap(),
        CompactEnvelopeProgress::Step(ParseStep::Yield)
    ));
    assert_eq!((reader.position(), five.spent()), (5, 5));
    for _ in 0..4 {
        let mut zero = WorkBudget::new(0);
        assert!(matches!(
            reader.step_typed_compact(&mut zero, &cancel).unwrap(),
            CompactEnvelopeProgress::Step(ParseStep::Yield)
        ));
        assert_eq!((reader.position(), zero.spent()), (5, 0));
    }
    assert!(matches!(
        reader
            .step_typed_compact(&mut WorkBudget::new(1), &cancel)
            .unwrap(),
        CompactEnvelopeProgress::Step(ParseStep::Yield)
    ));
    assert_eq!(reader.position(), 6);
    assert!(matches!(
        reader
            .step_typed_compact(&mut WorkBudget::new(0), &cancel)
            .unwrap(),
        CompactEnvelopeProgress::Step(ParseStep::Yield)
    ));
    let mut one = WorkBudget::new(1);
    let CompactEnvelopeProgress::Header(header) =
        reader.step_typed_compact(&mut one, &cancel).unwrap()
    else {
        panic!("one final syntax unit admits the already-owned header");
    };
    assert_eq!(one.spent(), 1);
    assert_eq!(
        (header.id, header.start, header.data, header.size),
        (0x1a45dfa3, 0, 6, Some(2))
    );
    drop(pressure);
    drop(reader);
    assert_eq!(pool.used(), base);
}
