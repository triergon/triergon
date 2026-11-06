//! Authored ownership-seam fixtures. These drive the existing envelope through
//! the typed policy constructor; full typed-file parity is a separate gate.
use super::*;
use crate::storage::PayloadOwnership;
use triergon_core::Bytes;

const BLOCK: [u8; 7] = [0x81, 0, 0, 0x80, b'a', b'b', b'c'];

fn pending(pool: &Budget, policy: PayloadOwnership, block: bool) -> Reader {
    let mut reader = Reader::new_with_payload_ownership(
        pool,
        SourceId(2201),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        256,
        policy,
    )
    .unwrap();
    let mut prefix = vec![0x1a, 0x45, 0xdf, 0xa3, 0x80, 0x18, 0x53, 0x80, 0x67, 0xff];
    if block {
        prefix.extend([0x1f, 0x43, 0xb6, 0x75, 0xff, 0xa3, 0x87]);
    } else {
        prefix.extend([0x15, 0x49, 0xa9, 0x66, 0x87]);
    }
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
    for _ in 0..10 {
        match reader
            .envelope
            .step(&mut WorkBudget::new(64), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                assert_eq!(request.offset, prefix.len() as u64);
                assert_eq!(request.maximum, 7);
                return reader;
            }
            ParseStep::Event(_) => (),
            other => panic!("prefix {other:?}"),
        }
    }
    panic!("missing block body request");
}
fn parent(pool: &Budget, payload: &[u8]) -> Bytes {
    let mut data = Vec::with_capacity(128);
    data.extend_from_slice(b"pre");
    data.extend_from_slice(payload);
    data.extend_from_slice(b"tail");
    Bytes::try_from_vec(pool, data).unwrap()
}
fn supply(reader: &mut Reader, root: &Bytes, length: usize) {
    let request = reader.request();
    reader
        .provide(
            &InputWindow::new(
                request.source,
                request.token,
                request.offset,
                root.slice(3..3 + length).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
}
fn leaf(reader: &mut Reader, quantum: u64) -> (SharedBytes, u64) {
    let mut total = 0;
    for _ in 0..100 {
        let mut work = WorkBudget::new(quantum);
        let step = reader
            .envelope
            .step(&mut work, &AtomicBool::new(false))
            .unwrap();
        total += work.spent();
        assert!(work.spent() <= quantum);
        match step {
            ParseStep::Yield => (),
            ParseStep::Event(ElementEvent::Leaf { data, .. }) => return (data, total),
            other => panic!("body {other:?}"),
        }
    }
    panic!("bounded body did not finish");
}

#[test]
fn owned_exact_default_and_explicit_policy_keep_the_legacy_hidden_parent_rule() {
    let pool = Budget::new(4096).unwrap();
    let plain = Reader::new(
        &pool,
        SourceId(2200),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        256,
    )
    .unwrap();
    assert_eq!(plain.payload_ownership(), PayloadOwnership::OwnedExact);
    drop(plain);
    let mut reader = pending(&pool, PayloadOwnership::OwnedExact, true);
    let root = parent(&pool, &BLOCK);
    supply(&mut reader, &root, 7);
    let (output, total) = leaf(&mut reader, 8192);
    assert_eq!(total, 15);
    assert_eq!(output.as_ref(), BLOCK);
    assert_eq!(output.retained_capacity(), 7);
    assert!(!output.shares_storage_with(&SharedBytes::from_core(root.clone())));
}

#[test]
fn shared_bounded_complete_block_borrows_the_actual_input_parent() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = pending(&pool, PayloadOwnership::SharedBounded, true);
    let root = parent(&pool, &BLOCK);
    supply(&mut reader, &root, 7);
    let (output, total) = leaf(&mut reader, 8192);
    assert_eq!(total, 15);
    assert_eq!(output.as_ref(), BLOCK);
    assert!(
        output.shares_storage_with(&SharedBytes::from_core(root.clone())),
        "explicit SharedBounded must share the source parent"
    );
    assert_eq!(output.retained_capacity(), root.retained_capacity());
}

#[test]
fn shared_bounded_last_alias_keeps_full_parent_charge_after_reader_drop() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = pending(&pool, PayloadOwnership::SharedBounded, true);
    let root = parent(&pool, &BLOCK);
    let charge = root.retained_storage_bytes();
    let capacity = root.retained_capacity();
    supply(&mut reader, &root, 7);
    let (output, _) = leaf(&mut reader, 8192);
    assert_eq!(
        output.retained_capacity(),
        capacity,
        "never report leafN for a larger shared parent"
    );
    let alias = output.clone();
    drop((root, reader, output));
    assert_eq!(pool.used(), baseline + charge);
    assert_eq!(alias.as_ref(), BLOCK);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn shared_bounded_precharged_parent_needs_no_new_leaf_owner_under_full_pool() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = pending(&pool, PayloadOwnership::SharedBounded, true);
    let root = parent(&pool, &BLOCK);
    supply(&mut reader, &root, 7);
    let hold = pool.reserve(pool.available()).unwrap();
    let before = pool.used();
    let mut work = WorkBudget::new(15);
    let output = match reader
        .envelope
        .step(&mut work, &AtomicBool::new(false))
        .unwrap()
    {
        ParseStep::Event(ElementEvent::Leaf { data, .. }) => data,
        other => panic!("SharedBounded must use the already charged parent: {other:?}"),
    };
    assert_eq!(work.spent(), 15);
    assert_eq!(pool.used(), before);
    assert_eq!(output.as_ref(), BLOCK);
    drop((hold, root, output, reader));
}

#[test]
fn shared_bounded_fragmented_block_keeps_the_owned_exact_fallback() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = pending(&pool, PayloadOwnership::SharedBounded, true);
    let a = parent(&pool, &BLOCK[..3]);
    supply(&mut reader, &a, 3);
    let mut first = WorkBudget::new(8192);
    assert!(matches!(
        reader
            .envelope
            .step(&mut first, &AtomicBool::new(false))
            .unwrap(),
        ParseStep::NeedInput(_)
    ));
    assert_eq!(first.spent(), 6);
    let b = parent(&pool, &BLOCK[3..]);
    supply(&mut reader, &b, 4);
    let (output, rest) = leaf(&mut reader, 8192);
    assert_eq!(first.spent() + rest, 15);
    assert_eq!(output.as_ref(), BLOCK);
    assert_eq!(output.retained_capacity(), 7);
    assert!(!output.shares_storage_with(&SharedBytes::from_core(a)));
    assert!(!output.shares_storage_with(&SharedBytes::from_core(b)));
}

#[test]
fn shared_bounded_does_not_change_metadata_parent_ownership() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = pending(&pool, PayloadOwnership::SharedBounded, false);
    let root = parent(&pool, b"opaque!");
    supply(&mut reader, &root, 7);
    let (output, total) = leaf(&mut reader, 8192);
    assert_eq!(total, 15);
    assert_eq!(output.as_ref(), b"opaque!");
    assert_eq!(output.retained_capacity(), 7);
    assert!(!output.shares_storage_with(&SharedBytes::from_core(root)));
}

#[test]
fn both_policies_keep_zero_work_and_literal_byte_work_under_one_seven_large_schedules() {
    for policy in [
        PayloadOwnership::OwnedExact,
        PayloadOwnership::SharedBounded,
    ] {
        for quantum in [1, 7, 8192] {
            let pool = Budget::new(4096).unwrap();
            let mut reader = pending(&pool, policy, true);
            let root = parent(&pool, &BLOCK);
            supply(&mut reader, &root, 7);
            let position = reader.position();
            let used = pool.used();
            let mut zero = WorkBudget::new(0);
            assert!(matches!(
                reader
                    .envelope
                    .step(&mut zero, &AtomicBool::new(false))
                    .unwrap(),
                ParseStep::Yield
            ));
            assert_eq!(
                (reader.position(), zero.spent(), pool.used()),
                (position, 0, used)
            );
            let (output, total) = leaf(&mut reader, quantum);
            assert_eq!(total, 15);
            assert_eq!(output.as_ref(), BLOCK);
            assert_eq!(reader.position(), position + 7);
        }
    }
}

#[test]
fn policy_persists_across_logical_reset_without_retaining_old_payload() {
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let mut reader = pending(&pool, PayloadOwnership::SharedBounded, true);
    let root = parent(&pool, &BLOCK);
    supply(&mut reader, &root, 7);
    let (output, _) = leaf(&mut reader, 8192);
    reader.envelope.reset().unwrap();
    assert_eq!(reader.payload_ownership(), PayloadOwnership::SharedBounded);
    assert_eq!(output.as_ref(), BLOCK);
    drop((reader, output, root));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn transport_still_precedes_cancel_zero_work_and_any_publication() {
    let pool = Budget::new(4096).unwrap();
    let mut reader = pending(&pool, PayloadOwnership::SharedBounded, true);
    let root = parent(&pool, &BLOCK);
    supply(&mut reader, &root, 7);
    let request = reader.request();
    reader
        .envelope
        .fail(
            request.source,
            request.token,
            triergon_core::SourceFailure::Transport,
        )
        .unwrap();
    let position = reader.position();
    assert!(matches!(
        reader
            .envelope
            .step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Core(triergon_core::Error::Transport))
    ));
    assert_eq!(reader.position(), position);
    assert_eq!(root.as_ref(), [b"pre".as_slice(), &BLOCK, b"tail"].concat());
}
