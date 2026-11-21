//! The element reader's input contract: one-byte windows under one-unit work,
//! starvation against transport failure and completion, Cluster resets, skipped
//! blocks and freeze backpressure.
use super::{ElementEvent, ElementReader};
use crate::ebml_kit::{avc_file_with_attachment, element as e, mkv};
use crate::{Error, Limits, ParseStep};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, WorkBudget};

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
    // and accidentally supply the space that this regression intentionally holds.
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
