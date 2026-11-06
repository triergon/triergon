//! Original full-reader guards for the explicit immutable source operation.
use super::*;
use std::io::Cursor;
use triergon_core::{Bytes, OwnedInputBytes};

fn element(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    assert!(data.len() < 127);
    out.push(0x80 | data.len() as u8);
    out.extend_from_slice(data);
    out
}
fn source() -> Vec<u8> {
    let track = element(
        0xae,
        &[
            element(0xd7, &[1]),
            element(0x73c5, &[1]),
            element(0x83, &[1]),
            element(0x86, b"V_AUTHORED/OPAQUE"),
        ]
        .concat(),
    );
    let cluster = element(
        0x1f43b675,
        &[
            element(0xe7, &[0]),
            element(0xa3, &[0x81, 0, 0, 0x80, b'x', b'y']),
            element(0xa3, &[0x81, 0, 1, 0x80, b'u', b'v']),
        ]
        .concat(),
    );
    let mut out = Vec::with_capacity(256);
    out.extend(element(0x1a45dfa3, &element(0x4282, b"matroska")));
    out.extend(element(
        0x18538067,
        &[element(0x1654ae6b, &track), cluster].concat(),
    ));
    out
}
fn input_payload(root: &Bytes) -> &[u8] {
    let at = root.windows(2).position(|bytes| bytes == b"xy").unwrap();
    &root[at..at + 2]
}
fn open(
    policy: crate::storage::PayloadOwnership,
) -> (Budget, Bytes, BlockingReader<Cursor<OwnedInputBytes>>) {
    let pool = Budget::new(Limits::default().live_storage_bytes).unwrap();
    let input = OwnedInputBytes::try_from_vec(&pool, source()).unwrap();
    let root = input.as_bytes().clone();
    let reader =
        BlockingReader::open_owned_seekable_in(input, Limits::default(), &pool, policy).unwrap();
    (pool, root, reader)
}

#[test]
fn shared_payload_really_aliases_the_complete_charged_input_parent() {
    let (_, root, mut reader) = open(crate::storage::PayloadOwnership::SharedBounded);
    let packet = reader
        .next_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(packet.data.as_ref(), b"xy");
    assert_eq!(packet.data.as_ref().as_ptr(), input_payload(&root).as_ptr());
    assert_eq!(packet.data.retained_capacity(), root.retained_capacity());
    assert_eq!((packet.pts_ns, packet.keyframe), (Some(0), true));
}

#[test]
fn owned_exact_payload_keeps_capacity_n_and_the_same_clock_and_bytes() {
    let (_, root, mut reader) = open(crate::storage::PayloadOwnership::OwnedExact);
    let packet = reader
        .next_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(packet.data.as_ref(), b"xy");
    assert_eq!(packet.data.retained_capacity(), 6);
    assert_ne!(packet.data.as_ref().as_ptr(), input_payload(&root).as_ptr());
    let next = reader
        .next_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(
        (next.data.as_ref(), next.pts_ns),
        (&b"uv"[..], Some(1_000_000))
    );
}

#[test]
fn close_drops_parser_aliases_and_returns_the_same_vector_without_copy() {
    let (pool, root, mut reader) = open(crate::storage::PayloadOwnership::SharedBounded);
    let pointer = root.as_ptr();
    let capacity = root.retained_capacity();
    let expected = root.as_ref().to_vec();
    drop(root);
    while reader
        .next_packet(&AtomicBool::new(false))
        .unwrap()
        .is_some()
    {}
    let bytes = reader.into_inner().into_inner();
    let vector = bytes.try_into_vec().unwrap();
    assert_eq!((vector.as_ptr(), vector.capacity()), (pointer, capacity));
    assert_eq!(vector, expected);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn caller_packet_survives_close_and_prevents_premature_source_recovery() {
    let (pool, root, mut reader) = open(crate::storage::PayloadOwnership::SharedBounded);
    let charge = root.retained_storage_bytes();
    let packet = reader
        .next_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    drop(root);
    let bytes = reader.into_inner().into_inner().try_into_vec().unwrap_err();
    assert_eq!(packet.data.as_ref(), b"xy");
    assert_eq!(pool.used(), Budget::control_storage_bytes() + charge);
    drop(packet);
    assert_eq!(bytes.try_into_vec().unwrap(), source());
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn an_unrelated_pool_cannot_admit_a_charged_source_as_operation_storage() {
    let first = Budget::new(4096).unwrap();
    let second = Budget::new(4096).unwrap();
    let root = OwnedInputBytes::try_from_vec(&first, source()).unwrap();
    let alias = root.as_bytes().clone();
    let before = (first.used(), second.used());
    assert!(matches!(
        BlockingReader::open_owned_seekable_in(
            root,
            Limits::default(),
            &second,
            crate::storage::PayloadOwnership::SharedBounded
        ),
        Err(Error::Core(triergon_core::Error::ForeignPool))
    ));
    assert_eq!(first.used(), before.0);
    assert_eq!(second.used(), before.1);
    assert_eq!(alias.as_ref(), source());
}

#[test]
fn explicit_seek_preserves_old_retained_packet_and_current_input_owner() {
    let (_, root, mut reader) = open(crate::storage::PayloadOwnership::SharedBounded);
    let cancel = AtomicBool::new(false);
    let old = reader.next_packet(&cancel).unwrap().unwrap();
    while reader.next_packet(&cancel).unwrap().is_some() {}
    let receipt = reader.seek_previous(0, 0, &cancel).unwrap();
    assert_eq!(receipt.actual_ns, 0);
    let new = reader.next_packet(&cancel).unwrap().unwrap();
    assert_eq!(old.data.as_ref(), b"xy");
    assert_eq!(new.data.as_ref(), b"xy");
    assert!(old.data.shares_storage_with(&new.data));
    assert_eq!(new.data.as_ref().as_ptr(), input_payload(&root).as_ptr());
}

#[test]
fn every_physical_prefix_keeps_the_existing_error_or_complete_packet_outcome() {
    let raw = source();
    type PacketObservation = (Vec<u8>, Option<i64>);
    type DrainOutcome = (Vec<PacketObservation>, Option<String>);
    fn drain<R: Read>(opened: Result<BlockingReader<R>>) -> DrainOutcome {
        let mut packets = Vec::new();
        let mut reader = match opened {
            Ok(reader) => reader,
            Err(error) => return (packets, Some(format!("{error:?}"))),
        };
        loop {
            match reader.next_packet(&AtomicBool::new(false)) {
                Ok(Some(packet)) => packets.push((packet.data.as_ref().to_vec(), packet.pts_ns)),
                Ok(None) => return (packets, None),
                Err(error) => return (packets, Some(format!("{error:?}"))),
            }
        }
    }
    for end in 0..=raw.len() {
        let before = drain(BlockingReader::open_seekable_with_payload_ownership(
            Cursor::new(&raw[..end]),
            Limits::default(),
            crate::storage::PayloadOwnership::SharedBounded,
        ));
        let pool = Budget::new(Limits::default().live_storage_bytes).unwrap();
        let root = OwnedInputBytes::try_from_vec(&pool, raw[..end].to_vec()).unwrap();
        let after = drain(BlockingReader::open_owned_seekable_in(
            root,
            Limits::default(),
            &pool,
            crate::storage::PayloadOwnership::SharedBounded,
        ));
        assert_eq!(after, before, "physical prefix {end}");
    }
}
