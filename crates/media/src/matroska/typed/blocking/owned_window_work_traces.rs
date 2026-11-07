//! Literal whole-envelope work/read/event comparison, not a speed assertion.
use super::*;
use crate::ebml_kit::{element, mkv};
use crate::{
    Limits,
    incremental::{ElementEvent, ElementReader, ParseStep},
    storage::PayloadOwnership,
};
use std::io::Cursor;
use triergon_core::{Budget, OwnedInputBytes, SourceCapabilities, SourceId};

fn source(body_size: usize) -> Vec<u8> {
    let mut payload = vec![b'x'; body_size];
    payload[..4].copy_from_slice(&[0x81, 0, 0, 0x80]);
    mkv(&element(
        0x1f43b675,
        &[
            element(0xe7, &[0]),
            element(0xa3, &payload),
            element(0xa3, &[0x81, 0, 1, 0x80, b'a', b'b']),
        ]
        .concat(),
    ))
}
fn trace(raw: &[u8], owned: bool, quantum: u64, policy: PayloadOwnership) -> Vec<String> {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    // Both runs have the same charged root and budget; the source operation
    // comparison changes only original Read copying versus immutable views.
    let root = OwnedInputBytes::try_from_vec(&pool, raw.to_vec()).unwrap();
    let cursor = Cursor::new(root);
    let mut feed = if owned {
        InputFeeder::from_owned(cursor, 8192).unwrap()
    } else {
        InputFeeder::new(cursor, 8192).unwrap()
    };
    let mut parser = ElementReader::new(
        &pool,
        SourceId(7033),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    parser.set_payload_ownership(policy);
    let cancel = AtomicBool::new(false);
    let mut rows = Vec::new();
    for turn in 0..1_000_000 {
        let allowance = if turn % 2 == 0 { 0 } else { quantum };
        let mut work = WorkBudget::new(allowance);
        let step = parser.step(&mut work, &cancel).unwrap();
        let tag = match &step {
            ParseStep::Event(ElementEvent::Start(header)) => format!("start {header:?}"),
            ParseStep::Event(ElementEvent::Leaf { header, data }) => {
                format!("leaf {header:?} {:x?}", data.as_ref())
            }
            ParseStep::Event(ElementEvent::Skipped(header)) => format!("skip {header:?}"),
            ParseStep::Event(ElementEvent::End {
                id,
                offset,
                generation,
            }) => format!("end {id} {offset} {generation}"),
            ParseStep::NeedInput(request) => format!("need {} {}", request.offset, request.maximum),
            ParseStep::Yield => "yield".into(),
            ParseStep::Finished => "finished".into(),
            ParseStep::Backpressure { .. } => panic!("unexpected ample-pool pressure"),
        };
        rows.push(format!(
            "{tag}|pos{}|left{}",
            parser.position(),
            work.remaining()
        ));
        match step {
            ParseStep::NeedInput(request) => {
                let header = parser.needs_header_input();
                let plan = parser.bounded_header_refill(&request, 4096).unwrap();
                let supplied = if let Some(plan) = plan {
                    rows.push(format!(
                        "supply {} {}",
                        plan.expanded().offset,
                        plan.expanded().maximum
                    ));
                    feed.feed_typed_header_refill(parser.range_input(), &plan, &cancel, None)
                } else {
                    rows.push(format!("supply {} {}", request.offset, request.maximum));
                    feed.feed_typed_request(parser.range_input(), &request, header, &cancel, None)
                }
                .unwrap();
                assert!(matches!(supplied, FeedStep::Provided | FeedStep::End));
                rows.push(format!(
                    "provided {supplied:?} at{}",
                    feed.reader_mut().position()
                ));
            }
            ParseStep::Finished => return rows,
            _ => (),
        }
    }
    panic!("bounded complete source failed to terminate")
}

#[test]
fn header_views_keep_every_zero_one_seven_large_work_event_and_read_boundary() {
    for size in [6, 4096] {
        let raw = source(size);
        for quantum in [1, 7, 8192] {
            for policy in [
                PayloadOwnership::OwnedExact,
                PayloadOwnership::SharedBounded,
            ] {
                let before = trace(&raw, false, quantum, policy);
                let after = trace(&raw, true, quantum, policy);
                assert_eq!(after, before, "body{size} work{quantum} {policy:?}");
            }
        }
    }
}
