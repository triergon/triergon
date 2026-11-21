//! The element reader's input contract: one-byte windows under one-unit work,
//! starvation against transport failure and completion, Cluster resets, skipped
//! blocks and freeze backpressure.
use super::{ElementEvent, ElementReader};
use crate::ebml_kit::{avc_file_with_attachment, element as e, mkv, uint as u};
use crate::storage::{SharedBytes, usage::Pressure};
use crate::{Error, Limits, ParseStep};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, WorkBudget};

mod compact_envelope;
mod discard;
mod header;
pub(crate) mod lazy_body;

#[test]
fn one_byte_windows_and_one_unit_work_preserve_incremental_envelopes() {
    let bytes = avc_file_with_attachment();
    let pool = Budget::new(1024 * 1024).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(7),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        Limits::default(),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut blocks = Vec::new();
    let mut cursor = 0;
    loop {
        let mut work = WorkBudget::new(1);
        match reader.step(&mut work, &cancel).unwrap() {
            ParseStep::NeedInput(request) => {
                assert_eq!(request.offset, cursor as u64);
                if cursor == bytes.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let data = Bytes::copy_from_slice(&pool, &bytes[cursor..cursor + 1]).unwrap();
                    let window =
                        InputWindow::new(request.source, request.token, request.offset, data)
                            .unwrap();
                    reader.provide(&window).unwrap();
                    cursor += 1;
                }
            }
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                blocks.push(data);
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            ParseStep::Backpressure { .. } => panic!("ample pool"),
            ParseStep::Finished => break,
        }
        assert!(work.spent() <= 1);
    }
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].as_ref(), &[0x81, 0, 0, 0x80, 1, 2, 3]);
    assert_eq!(blocks[1].as_ref(), &[0x81, 0, 1, 0x80, 4, 5, 6]);
    assert_eq!(reader.position(), bytes.len() as u64);
}

#[test]
fn incremental_starvation_transport_failure_and_explicit_completion_are_distinct() {
    use triergon_core::{Error as CoreError, SourceFailure};
    let pool = Budget::new(4096).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        32,
    )
    .unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::NeedInput(_)
    ));
    let request = reader.request();
    let empty = Bytes::copy_from_slice(&pool, &[]).unwrap();
    reader
        .provide(&InputWindow::new(request.source, request.token.clone(), 0, empty).unwrap())
        .unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::NeedInput(_)
    ));
    reader
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(CoreError::Transport)
    ));
    reader.reset().unwrap();
    let request = reader.request();
    reader.finish(request.source, request.token).unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Truncated)
    ));
}

#[test]
fn cluster_range_reset_invalidates_old_input_but_preserves_earlier_leaf_storage() {
    let bytes = avc_file_with_attachment();
    let pool = Budget::new(64 * 1024).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(9),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        },
        Limits::default(),
        bytes.len(),
    )
    .unwrap();
    let request = reader.request();
    let old_token = request.token.clone();
    let window = InputWindow::new(
        request.source,
        request.token,
        0,
        Bytes::copy_from_slice(&pool, &bytes).unwrap(),
    )
    .unwrap();
    reader.provide(&window).unwrap();
    drop(window);
    let mut checkpoint = None;
    let mut earlier = None;
    for _ in 0..10_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(ElementEvent::Start(header)) if header.id == 0x1f43b675 => {
                checkpoint = reader.cluster_checkpoint();
            }
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                earlier = Some(data);
                break;
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            _ => panic!("unexpected input transition"),
        }
    }
    let earlier = earlier.unwrap();
    let checkpoint = checkpoint.unwrap();
    let request = reader.reset_to_cluster(&checkpoint).unwrap();
    assert_eq!(request.offset, checkpoint.offset());
    assert_ne!(reader.generation(), old_token.generation());
    let stale = InputWindow::new(
        request.source,
        old_token,
        request.offset,
        Bytes::copy_from_slice(&pool, &[]).unwrap(),
    )
    .unwrap();
    assert!(reader.provide(&stale).is_err());
    let start = request.offset as usize;
    let replay = InputWindow::new(
        request.source,
        request.token,
        request.offset,
        Bytes::copy_from_slice(&pool, &bytes[start..]).unwrap(),
    )
    .unwrap();
    reader.provide(&replay).unwrap();
    drop(replay);
    let mut found = false;
    for _ in 0..10_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                assert_eq!(data.as_ref(), earlier.as_ref());
                found = true;
                break;
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            _ => panic!("unexpected replay transition"),
        }
    }
    assert!(found);
    drop(reader);
    assert_eq!(earlier.as_ref(), &[0x81, 0, 0, 0x80, 1, 2, 3]);
}

#[test]
fn skipped_block_outside_cluster_does_not_reset_scan_policy() {
    // These elements are skipped at Segment level. Their Block ID alone must
    // not let an irrelevant scan repeatedly replenish its operation budget.
    let outside = e(0xa3, &[0x81, 0, 0, 0x80, 1]).repeat(32);
    let bytes = mkv(&outside);
    let pool = Budget::new(64 * 1024).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(21),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        Limits {
            scan_bytes: 64,
            ..Limits::default()
        },
        bytes.len(),
    )
    .unwrap();
    let request = reader.request();
    let window = InputWindow::new(
        request.source,
        request.token,
        0,
        Bytes::copy_from_slice(&pool, &bytes).unwrap(),
    )
    .unwrap();
    reader.provide(&window).unwrap();
    let mut outcome = None;
    for _ in 0..10_000 {
        match reader.step(&mut WorkBudget::new(3), &AtomicBool::new(false)) {
            Ok(ParseStep::NeedInput(request)) => {
                reader.finish(request.source, request.token).unwrap();
            }
            Ok(ParseStep::Event(_) | ParseStep::Yield) => (),
            Ok(ParseStep::Finished) => break,
            Ok(ParseStep::Backpressure { .. }) => panic!("ample pool"),
            Err(error) => {
                outcome = Some(error);
                break;
            }
        }
    }
    assert!(
        matches!(outcome, Some(Error::Policy("operation read bytes"))),
        "a skipped Block must not replenish a bounded scan: {outcome:?}"
    );
}

#[test]
fn completed_leaf_survives_freeze_backpressure_without_consuming_more_input() {
    let bytes = avc_file_with_attachment();
    let pool = Budget::new(64 * 1024).unwrap();
    let source = Bytes::copy_from_slice(&pool, &bytes).unwrap();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(22),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: false,
        },
        Limits::default(),
        bytes.len(),
    )
    .unwrap();
    let request = reader.request();
    let window = InputWindow::new(request.source, request.token, 0, source.clone()).unwrap();
    reader.provide(&window).unwrap();
    for _ in 0..20_000 {
        if reader.position() == bytes.len() as u64 {
            break;
        }
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(_) | ParseStep::Yield => (),
            _ => panic!("whole source is supplied with ample storage"),
        }
    }
    assert_eq!(reader.position(), bytes.len() as u64);
    // Keep the source alias so copying the final span cannot release its owner
    // and accidentally supply the space that this test intentionally holds.
    let occupied = pool.reserve(pool.available()).unwrap();
    let mut blocked = false;
    for _ in 0..100 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Backpressure { .. } => {
                blocked = true;
                break;
            }
            ParseStep::Yield => (),
            _ => panic!("completed leaf must wait for its storage header"),
        }
    }
    assert!(blocked);
    assert_eq!(reader.position(), bytes.len() as u64);
    drop(occupied);
    let mut payload = None;
    for _ in 0..100 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                payload = Some(data);
                break;
            }
            ParseStep::Yield => (),
            _ => panic!("retry must publish the same completed leaf"),
        }
    }
    assert_eq!(payload.unwrap().as_ref(), &[0x81, 0, 1, 0x80, 4, 5, 6]);
    assert_eq!(source.as_ref(), bytes.as_slice());
}

fn collected_leaf(hidden_parent: bool, quantum: u64) -> (Budget, usize, SharedBytes, SharedBytes) {
    let header = e(0x1a45dfa3, &e(0x4282, b"matroska"));
    let info = u(0x2ad7b1, 1_000_000);
    let info_element = e(0x1549a966, &info);
    let segment = e(0x18538067, &info_element);
    let start = header.len() + segment.len() - info.len();
    let raw = [header, segment].concat();
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(901),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut input_owner = None;
    let mut output = None;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(quantum);
        let step = reader.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        match step {
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                let bytes = if at == start {
                    assert!(request.maximum >= info.len());
                    let bytes = if hidden_parent {
                        Bytes::copy_from_slice(&pool, &raw)
                            .unwrap()
                            .slice(start..raw.len())
                            .unwrap()
                    } else {
                        Bytes::copy_from_slice(&pool, &info).unwrap()
                    };
                    input_owner = Some(SharedBytes::from_core(bytes.clone()));
                    bytes
                } else {
                    let end = (at + request.maximum).min(raw.len());
                    assert!(at < end);
                    Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap()
                };
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0x1549a966 => {
                assert_eq!(header.data, start as u64);
                assert_eq!(data.as_ref(), info.as_slice());
                assert_eq!(data.retained_capacity(), info.len());
                output = Some(data);
                break;
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            other => panic!("complete source leaf: {other:?}"),
        }
    }
    drop(reader);
    (pool, baseline, input_owner.unwrap(), output.unwrap())
}

#[test]
fn complete_leaf_reuses_its_exact_input_owner() {
    let (pool, baseline, input, output) = collected_leaf(false, 8192);
    assert!(
        output.shares_storage_with(&input),
        "exact whole body owner should transfer without a second allocation/copy"
    );
    // Exact tracked charge, not SharedBytes' conservative hard-policy allowance.
    let charge = input.retained_capacity() + Bytes::owner_overhead_bytes();
    assert_eq!(pool.used(), baseline + charge);
    drop(input);
    assert_eq!(output.as_ref(), u(0x2ad7b1, 1_000_000).as_slice());
    assert_eq!(pool.used(), baseline + charge);
    drop(output);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn hidden_leaf_parent_keeps_the_exact_output_capacity_under_all_work_schedules() {
    for quantum in [1, 7, 8192] {
        let (pool, baseline, input, output) = collected_leaf(true, quantum);
        assert!(!output.shares_storage_with(&input));
        let output_charge = output.retained_capacity() + Bytes::owner_overhead_bytes();
        drop(input);
        assert_eq!(pool.used(), baseline + output_charge);
        assert_eq!(output.as_ref(), u(0x2ad7b1, 1_000_000).as_slice());
        drop(output);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn complete_leaf_waits_for_work_accounts_its_owner_and_observes_cancel() {
    let header = e(0x1a45dfa3, &e(0x4282, b"matroska"));
    let info = u(0x2ad7b1, 1_000_000);
    let info_element = e(0x1549a966, &info);
    let segment = e(0x18538067, &info_element);
    let start = header.len() + segment.len() - info.len();
    let raw = [header, segment].concat();
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(902),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut owner = None;
    for _ in 0..1000 {
        match reader.step(&mut WorkBudget::new(8192), &cancel).unwrap() {
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                let end = (at + request.maximum).min(raw.len());
                assert!(at < end);
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap();
                if at == start {
                    owner = Some(SharedBytes::from_core(bytes.clone()));
                }
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
                if at == start {
                    break;
                }
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            other => panic!("header-only progress: {other:?}"),
        }
    }
    let owner = owner.unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(info.len() as u64), &cancel)
            .unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(reader.position(), raw.len() as u64);
    let before = pool.used();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(pool.used(), before);
    // No outside allocation is unowned by this graph: the complete source
    // owner (shared with this alias) must appear in the minimum-pool proof.
    for (requested, permanent) in [(pool.available(), false), (pool.available() + 1, true)] {
        let mut proof = Pressure::new(requested);
        let mut done = false;
        for _ in 0..1000 {
            let mut work = WorkBudget::new(1);
            match proof.step(&reader, &pool, &mut work, &cancel) {
                Ok(ParseStep::Yield) => (),
                Ok(ParseStep::Event(())) => {
                    assert!(!permanent);
                    done = true;
                    break;
                }
                Err(Error::Policy("operation retained storage")) => {
                    assert!(permanent);
                    done = true;
                    break;
                }
                other => panic!("pending leaf owner proof: {other:?}"),
            }
            assert!(work.spent() <= 1);
        }
        assert!(done);
    }
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    drop(reader);
    assert_eq!(owner.as_ref(), info.as_slice());
    assert_eq!(
        pool.used(),
        baseline + owner.retained_capacity() + Bytes::owner_overhead_bytes()
    );
    drop(owner);
    assert_eq!(pool.used(), baseline);
}
