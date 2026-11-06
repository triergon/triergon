//! Real typed-reader retention, pool graph, generation and cancellation guards.
use super::*;
use crate::storage::PayloadOwnership;
use std::io::Cursor;
use triergon_core::Bytes;

const RAW: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/shared_bounded/hevc-aac.mkv"
));
fn data_owner(packet: &crate::matroska::Packet) -> Bytes {
    use crate::matroska::typed::usage::{Owner, Packets, Storage};
    let slot = Some(packet.clone());
    match Packets(std::slice::from_ref(&slot))
        .storage_owner(0)
        .unwrap()
    {
        Some(Owner::Bytes(bytes)) => bytes,
        other => panic!("missing actual packet owner: {other:?}"),
    }
}

fn reader(pool: &Budget) -> Reader {
    Reader::new_with_payload_ownership(
        pool,
        SourceId(2301),
        SourceCapabilities {
            length: Some(RAW.len() as u64),
            random_access: true,
        },
        Limits::default(),
        4096,
        PayloadOwnership::SharedBounded,
    )
    .unwrap()
}
fn packet(reader: &mut Reader, root: &Bytes) -> crate::typed::Packet {
    let cancel = AtomicBool::new(false);
    for _ in 0..1_000_000 {
        match reader
            .step_parse(&mut WorkBudget::new(8192), &cancel)
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                if at == RAW.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    assert!(at < RAW.len() && request.maximum != 0);
                    let end = at.saturating_add(request.maximum).min(RAW.len());
                    reader
                        .provide(
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                root.slice(at..end).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            ParseStep::Event(Event::Packet(packet)) => return packet,
            ParseStep::Event(_) | ParseStep::Yield => (),
            other => panic!("real typed source {other:?}"),
        }
    }
    panic!("bounded first packet did not finish");
}

// Pinned source facts: first block at3178 has two spans915+967 and
// capacity1882. The next block at5063 has one complete1141-byte span.
fn complete_packet(reader: &mut Reader, root: &Bytes) -> crate::typed::Packet {
    let first = packet(reader, root);
    assert_eq!(reader.position(), 5063);
    assert_eq!(first.packet.data.retained_capacity(), 1882);
    drop(first);
    let complete = packet(reader, root);
    assert_eq!(reader.position(), 6207);
    assert_eq!(reader.payload_ownership(), PayloadOwnership::SharedBounded);
    complete
}
#[test]
fn shared_bounded_real_first_fragmented_packet_keeps_exact_capacity() {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let root = Bytes::copy_from_slice(&pool, RAW).unwrap();
    let mut reader = reader(&pool);
    let first = packet(&mut reader, &root);
    assert_eq!(reader.position(), 5063);
    assert_eq!(first.packet.data.retained_capacity(), 1882);
    assert!(!data_owner(&first.packet).shares_storage_with(&root));
}

#[test]
fn shared_bounded_real_typed_packet_keeps_full_parent_only_until_last_data_alias() {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = pool.used();
    let root = Bytes::copy_from_slice(&pool, RAW).unwrap();
    let charge = root.retained_storage_bytes();
    let mut reader = reader(&pool);
    let packet = complete_packet(&mut reader, &root);
    let data = data_owner(&packet.packet);
    assert!(data.shares_storage_with(&root));
    assert_eq!(data.retained_capacity(), root.retained_capacity());
    assert!(!data.is_empty());
    let expected = data.as_ref().to_vec();
    drop((packet, reader, root));
    assert_eq!(pool.used(), baseline + charge);
    assert_eq!(data.as_ref(), expected);
    drop(data);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn shared_bounded_real_typed_owner_graph_deduplicates_input_and_packet_parent() {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let root = Bytes::copy_from_slice(&pool, RAW).unwrap();
    let mut reader = reader(&pool);
    let packet = complete_packet(&mut reader, &root);
    assert!(data_owner(&packet.packet).shares_storage_with(&root));
    let used = pool.used();
    for extra in [0, 1] {
        let requested = pool.limit() - used + extra;
        let mut proof = crate::matroska::typed::usage::Pressure::new(requested);
        let mut done = false;
        for _ in 0..2_000_000 {
            let mut work = WorkBudget::new(7);
            let result = proof.step(&reader, &pool, &mut work, &AtomicBool::new(false));
            assert!(work.spent() <= 7);
            match result {
                Ok(ParseStep::Yield) => (),
                Ok(ParseStep::Event(())) => {
                    assert_eq!(extra, 0, "missing owned capacity");
                    done = true;
                    break;
                }
                Err(Error::Policy("operation retained storage")) => {
                    assert_eq!(extra, 1, "double-counted shared parent");
                    done = true;
                    break;
                }
                other => panic!("owner graph {other:?}"),
            }
        }
        assert!(done);
    }
}

#[test]
fn shared_bounded_real_reset_changes_generation_preserves_policy_and_old_data() {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = pool.used();
    let root = Bytes::copy_from_slice(&pool, RAW).unwrap();
    let charge = root.retained_storage_bytes();
    let mut reader = reader(&pool);
    let first = complete_packet(&mut reader, &root);
    let data = data_owner(&first.packet);
    let expected = data.as_ref().to_vec();
    let old = reader.request();
    let stale = InputWindow::new(
        old.source,
        old.token.clone(),
        old.offset,
        root.slice(old.offset as usize..old.offset as usize + 1)
            .unwrap(),
    )
    .unwrap();
    let next = reader.reset().unwrap();
    assert_ne!(old.token.generation(), next.token.generation());
    assert_eq!(reader.payload_ownership(), PayloadOwnership::SharedBounded);
    assert!(reader.provide(&stale).is_err());
    assert_eq!(reader.position(), 0);
    let second = complete_packet(&mut reader, &root);
    assert_eq!(first.packet.track, second.packet.track);
    assert_eq!(first.packet.pts_ns, second.packet.pts_ns);
    assert_eq!(first.packet.data.as_ref(), second.packet.data.as_ref());
    assert_eq!(data.as_ref(), expected);
    drop((reader, first, second, stale, root));
    assert_eq!(pool.used(), baseline + charge);
    drop(data);
    assert_eq!(pool.used(), baseline);
}

fn pending_commit(pool: &Budget) -> (Reader, Bytes) {
    let mut reader = Reader::new_with_payload_ownership(
        pool,
        SourceId(2302),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        256,
        PayloadOwnership::SharedBounded,
    )
    .unwrap();
    let prefix = [
        0x1a, 0x45, 0xdf, 0xa3, 0x80, 0x18, 0x53, 0x80, 0x67, 0xff, 0x1f, 0x43, 0xb6, 0x75, 0xff,
        0xa3, 0x87,
    ];
    let request = reader.request();
    reader
        .provide(
            &InputWindow::new(
                request.source,
                request.token,
                0,
                Bytes::copy_from_slice(pool, &prefix).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let mut waiting = false;
    for _ in 0..10 {
        if let ParseStep::NeedInput(_) = reader
            .envelope
            .step(&mut WorkBudget::new(64), &AtomicBool::new(false))
            .unwrap()
        {
            waiting = true;
            break;
        }
    }
    assert!(waiting, "bounded prefix did not reach its body");
    let root = Bytes::copy_from_slice(pool, b"pre\x81\0\0\x80abctail").unwrap();
    let request = reader.request();
    reader
        .provide(
            &InputWindow::new(
                request.source,
                request.token,
                request.offset,
                root.slice(3..10).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let mut work = WorkBudget::new(8);
    assert!(matches!(
        reader
            .envelope
            .step(&mut work, &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(work.spent(), 8); // Read seven, commit one: no publication yet.
    (reader, root)
}

#[test]
fn shared_bounded_partial_commit_cancel_has_no_early_output_and_reset_releases_parent() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let (mut reader, root) = pending_commit(&pool);
    assert!(matches!(
        reader
            .envelope
            .step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(root.as_ref(), b"pre\x81\0\0\x80abctail");
    reader.reset().unwrap();
    assert_eq!(reader.payload_ownership(), PayloadOwnership::SharedBounded);
    drop((reader, root));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn shared_bounded_partial_commit_transport_stays_before_cancel_and_final_unit() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let (mut reader, root) = pending_commit(&pool);
    let request = reader.request();
    reader
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    assert!(matches!(
        reader
            .envelope
            .step(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Transport)
    ));
    assert_eq!(root.as_ref(), b"pre\x81\0\0\x80abctail");
    drop((reader, root));
    assert_eq!(pool.used(), baseline);
}

#[derive(Debug, PartialEq)]
struct Fact(
    usize,
    Option<i64>,
    Option<i64>,
    Option<u64>,
    bool,
    i64,
    Vec<u8>,
);
fn facts(policy: PayloadOwnership) -> Vec<Fact> {
    let mut reader = BlockingReader::open_seekable_with_payload_ownership(
        Cursor::new(RAW),
        Limits::default(),
        policy,
    )
    .unwrap();
    let mut facts = Vec::new();
    while let Some(packet) = reader.next_typed_packet(&AtomicBool::new(false)).unwrap() {
        let p = packet.packet;
        facts.push(Fact(
            p.track,
            p.pts_ns,
            p.dts_ns,
            p.duration_ns,
            p.keyframe,
            p.discard_padding_ns,
            p.data.as_ref().to_vec(),
        ));
    }
    facts
}
#[test]
fn shared_bounded_blocking_real_source_keeps_all_packet_bytes_and_clocks() {
    let old = facts(PayloadOwnership::OwnedExact);
    let shared = facts(PayloadOwnership::SharedBounded);
    assert_eq!(old.len(), 237);
    assert_eq!(old, shared);
}
