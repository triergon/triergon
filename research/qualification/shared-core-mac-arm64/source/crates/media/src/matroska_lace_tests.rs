//! Authored storage and failure guards for encoded lace input spans.
use super::*;
use std::io::Cursor;

const ABC: [u8; 14] = [120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];

fn e(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (data.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(data);
    out
}
fn lace_block(mode: u8, last: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0x81, 0, 0, 0x80 | (mode << 1), 2];
    match mode {
        1 => bytes.extend([14, 14]),
        2 => (),
        3 => bytes.extend([0x8e, 0xbf]), // first size 14, second size delta 0
        _ => panic!("authored lace mode"),
    }
    bytes.extend(ABC);
    bytes.extend(ABC);
    bytes.extend(last);
    bytes
}
fn group(block: &[u8]) -> Vec<u8> {
    // Source offsets include an opaque child before Block and a final child.
    [e(0xec, &[0xa1, 0xff, 0, 1]), e(0xa1, block), e(0x9b, &[3])].concat()
}
fn fixture(block: Option<(&[u8], bool)>) -> Vec<u8> {
    let track = [
        e(0xd7, &[1]),
        e(0x73c5, &[1]),
        e(0x83, &[2]),
        e(0x86, b"A_PCM/INT/LIT"),
        e(0x6d80, &e(0x6240, &e(0x5034, &e(0x4254, &[0])))),
    ]
    .concat();
    let mut cluster = e(0xe7, &[0]);
    if let Some((bytes, simple)) = block {
        cluster.extend(e(if simple { 0xa3 } else { 0xa0 }, bytes));
    }
    [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(
            0x18538067,
            &[e(0x1654ae6b, &e(0xae, &track)), e(CLUSTER, &cluster)].concat(),
        ),
    ]
    .concat()
}
fn prepared(pool: &Budget) -> Demuxer<Cursor<Vec<u8>>> {
    let mut reader = Demuxer::open_in(Cursor::new(fixture(None)), Limits::default(), pool).unwrap();
    // Consume the authored Timestamp and finish the empty Cluster before
    // installing a separately charged Block as a pending operation.
    assert!(
        reader
            .next_packet(&AtomicBool::new(false))
            .unwrap()
            .is_none()
    );
    reader.pending.try_reserve_exact(2).unwrap();
    reader
}
fn output_charge(pool: &Budget) -> usize {
    let before = pool.used();
    let output = SharedBytes::copy_from_slice(pool, b"abc").unwrap();
    let charge = pool.used() - before;
    drop(output);
    assert_eq!(pool.used(), before);
    charge
}

#[test]
fn charged_laced_source_needs_only_the_owned_decoded_outputs_remaining_capacity() {
    for mode in [1, 2, 3] {
        for simple in [false, true] {
            let pool = Budget::new(64 * 1024).unwrap();
            let baseline = pool.used();
            let mut reader = prepared(&pool);
            let block = lace_block(mode, &ABC);
            let raw = if simple { block } else { group(&block) };
            let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
            let charge = output_charge(&pool);
            reader.retry_block = Some((source.clone(), simple));
            let occupied = pool.reserve(pool.available() - 3 * charge).unwrap();
            let first = reader
                .next_packet(&AtomicBool::new(false))
                .unwrap()
                .unwrap();
            let second = reader
                .next_packet(&AtomicBool::new(false))
                .unwrap()
                .unwrap();
            let third = reader
                .next_packet(&AtomicBool::new(false))
                .unwrap()
                .unwrap();
            for packet in [&first, &second, &third] {
                assert_eq!(packet.data.as_ref(), b"abc");
                assert_eq!(packet.data.retained_capacity(), 3);
                assert!(!packet.data.storage.shares_storage_with(&source));
            }
            assert_eq!(pool.available(), 0);
            let alias = first.data.clone();
            drop((reader, source, occupied, first, second, third));
            assert_eq!(alias.as_ref(), b"abc");
            assert!(pool.used() > baseline);
            drop(alias);
            assert_eq!(pool.used(), baseline);
        }
    }
}

#[test]
fn laced_output_backpressure_discards_no_source_and_publishes_no_partial_batch() {
    let pool = Budget::new(64 * 1024).unwrap();
    let mut reader = prepared(&pool);
    let source = SharedBytes::copy_from_slice(&pool, &group(&lace_block(3, &ABC))).unwrap();
    let position = reader.input.position;
    let charge = output_charge(&pool);
    reader.retry_block = Some((source.clone(), false));
    let occupied = pool.reserve(pool.available() - (3 * charge - 1)).unwrap();
    assert!(matches!(
        reader.next_packet(&AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert!(reader.pending.pop_front().is_none());
    assert!(
        reader
            .retry_block
            .as_ref()
            .unwrap()
            .0
            .shares_storage_with(&source)
    );
    assert_eq!(reader.input.position, position);
    drop(occupied);
    for _ in 0..3 {
        assert_eq!(
            reader
                .next_packet(&AtomicBool::new(false))
                .unwrap()
                .unwrap()
                .data
                .as_ref(),
            b"abc"
        );
    }
    assert_eq!(reader.input.position, position);
}

#[test]
fn later_encoded_lace_checksum_or_truncation_cannot_publish_earlier_frames() {
    let mut corrupt = ABC;
    corrupt[13] ^= 1;
    for mode in [1, 2, 3] {
        for simple in [false, true] {
            let block = lace_block(mode, &corrupt);
            let raw = if simple { block } else { group(&block) };
            let mut reader = Demuxer::open(
                Cursor::new(fixture(Some((&raw, simple)))),
                Limits::default(),
            )
            .unwrap();
            assert!(matches!(
                reader.next_packet(&AtomicBool::new(false)),
                Err(Error::Invalid("Adler-32 mismatch"))
            ));
            assert!(reader.next_packet(&AtomicBool::new(false)).is_err());
        }
    }
    for mode in [1, 3] {
        let block = lace_block(mode, &ABC[..5]);
        let mut reader = Demuxer::open(
            Cursor::new(fixture(Some((&block, true)))),
            Limits::default(),
        )
        .unwrap();
        assert!(matches!(
            reader.next_packet(&AtomicBool::new(false)),
            Err(Error::Truncated)
        ));
        assert!(reader.next_packet(&AtomicBool::new(false)).is_err());
    }
}

#[test]
fn cancellation_before_a_pending_lace_leaves_retained_output_valid_and_reader_failed() {
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let block = lace_block(2, &ABC);
    let mut reader = Demuxer::open_in(
        Cursor::new(fixture(Some((&block, true)))),
        Limits::default(),
        &pool,
    )
    .unwrap();
    let first = reader
        .next_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert!(matches!(
        reader.next_packet(&AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(reader.next_packet(&AtomicBool::new(false)).is_err());
    drop(reader);
    assert_eq!(first.data.as_ref(), b"abc");
    drop(first);
    assert_eq!(pool.used(), baseline);
}
