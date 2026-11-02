mod common;
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BoxView, FragmentRandomAccessStep, Limits, MovieFragmentRandomAccessTail,
    TrackFragmentRandomAccess,
};

fn raw(pool: &Budget, kind: &[u8; 4], body: &[u8], start: u64) -> BoxView {
    let mut b = u32::try_from(body.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    b.extend(kind);
    b.extend(body);
    BoxView::new(common::bytes(pool, &b), start).unwrap()
}

fn tfra(pool: &Budget, version: u8, widths: [u8; 3], entries: &[(u64, u64, [u32; 3])]) -> BoxView {
    let mut b = vec![version, 0, 0, 0];
    b.extend(7u32.to_be_bytes());
    let descriptor = (u32::from(widths[0] - 1) << 4)
        | (u32::from(widths[1] - 1) << 2)
        | u32::from(widths[2] - 1);
    b.extend(descriptor.to_be_bytes());
    b.extend(u32::try_from(entries.len()).unwrap().to_be_bytes());
    for (time, offset, numbers) in entries {
        if version == 0 {
            b.extend(u32::try_from(*time).unwrap().to_be_bytes());
            b.extend(u32::try_from(*offset).unwrap().to_be_bytes());
        } else {
            b.extend(time.to_be_bytes());
            b.extend(offset.to_be_bytes());
        }
        for (number, width) in numbers.iter().zip(widths) {
            b.extend(&number.to_be_bytes()[4 - usize::from(width)..]);
        }
    }
    raw(pool, b"tfra", &b, 64)
}

#[test]
fn primary_tfra_all_number_widths_preserve_source_bits_order_and_sparse_offsets() {
    let pool = Budget::new(64 * 1024).unwrap();
    for version in [0, 1] {
        for a in 1..=4 {
            for b in 1..=4 {
                for c in 1..=4 {
                    let high = if version == 0 {
                        u64::from(u32::MAX)
                    } else {
                        (1u64 << 63) + 7
                    };
                    let offset = if version == 0 {
                        4000
                    } else {
                        (1u64 << 32) + 42
                    };
                    let numbers = [a, b, c].map(|width| u32::MAX >> (u32::from(4 - width) * 8));
                    let input = tfra(
                        &pool,
                        version,
                        [a, b, c],
                        &[(high, offset, numbers), (0, 128, [4, 3, 2])],
                    );
                    let index =
                        TrackFragmentRandomAccess::parse(input.clone(), Limits::default()).unwrap();
                    assert_eq!(index.raw().bytes().as_ref(), input.bytes().as_ref());
                    assert_eq!(
                        (index.track_id(), index.entry_count(), index.version()),
                        (7, 2, version)
                    );
                    assert_eq!(index.number_widths(), [a, b, c]);
                    let before = pool.used();
                    let mut cursor = index.cursor();
                    assert!(matches!(
                        cursor.poll(&mut WorkBudget::new(0)).unwrap(),
                        FragmentRandomAccessStep::Yield
                    ));
                    let FragmentRandomAccessStep::Entry(first) =
                        cursor.poll(&mut WorkBudget::new(1)).unwrap()
                    else {
                        panic!()
                    };
                    assert_eq!(
                        (first.ordinal, first.time_ticks, first.moof_offset),
                        (1, high, offset)
                    );
                    assert_eq!(
                        [first.traf_number, first.trun_number, first.sample_number],
                        numbers
                    );
                    let FragmentRandomAccessStep::Entry(second) =
                        cursor.poll(&mut WorkBudget::new(1)).unwrap()
                    else {
                        panic!()
                    };
                    assert_eq!((second.time_ticks, second.moof_offset), (0, 128));
                    assert!(matches!(
                        cursor.poll(&mut WorkBudget::new(0)).unwrap(),
                        FragmentRandomAccessStep::End
                    ));
                    assert_eq!(pool.used(), before);
                }
            }
        }
    }
}

#[test]
fn tfra_empty_truncated_reserved_and_zero_numbers_never_grant_recovery() {
    let pool = Budget::new(4096).unwrap();
    let empty =
        TrackFragmentRandomAccess::parse(tfra(&pool, 0, [1; 3], &[]), Limits::default()).unwrap();
    assert_eq!(empty.entry_count(), 0);
    assert!(matches!(
        empty.cursor().poll(&mut WorkBudget::new(1)).unwrap(),
        FragmentRandomAccessStep::End
    ));
    // Parse validates bounded layout. Invalid 1-based record coordinates fail
    // when polled and cannot be skipped by retrying the same cursor.
    let zero = TrackFragmentRandomAccess::parse(
        tfra(&pool, 0, [1; 3], &[(0, 64, [0, 1, 1])]),
        Limits::default(),
    )
    .unwrap();
    let mut cursor = zero.cursor();
    for _ in 0..2 {
        assert!(matches!(
            cursor.poll(&mut WorkBudget::new(1)),
            Err(Error::Invalid("zero tfra coordinate"))
        ));
    }
    let input = tfra(&pool, 1, [1, 2, 4], &[(5, 4096, [1, 256, 65536])]);
    let bytes = input.bytes().as_ref();
    let mut body = input.payload().to_vec();
    body[8] = 1;
    assert!(matches!(
        TrackFragmentRandomAccess::parse(raw(&pool, b"tfra", &body, 0), Limits::default()),
        Err(Error::Invalid(_))
    ));
    let mut body = input.payload().to_vec();
    body.pop();
    assert!(matches!(
        TrackFragmentRandomAccess::parse(raw(&pool, b"tfra", &body, 0), Limits::default()),
        Err(Error::Truncated)
    ));
    assert_eq!(bytes, input.bytes().as_ref());
    assert!(matches!(
        TrackFragmentRandomAccess::parse(
            input,
            Limits {
                max_records: 0,
                ..Limits::default()
            }
        ),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn mfro_produces_a_checked_sparse_parent_range_and_verifies_actual_parent_bounds() {
    let pool = Budget::new(4096).unwrap();
    let end = (1u64 << 32) + 512;
    let mut body = vec![0; 4];
    body.extend(24u32.to_be_bytes());
    let tail = raw(&pool, b"mfro", &body, end - 16);
    let index = MovieFragmentRandomAccessTail::parse(tail.clone(), end).unwrap();
    assert_eq!(index.parent_range(), end - 24..end);
    assert_eq!(index.raw().bytes().as_ref(), tail.bytes().as_ref());
    let parent = raw(&pool, b"mfra", tail.bytes().as_ref(), end - 24);
    index.require_mfra(&parent).unwrap();
    let wrong = raw(&pool, b"free", tail.bytes().as_ref(), end - 24);
    assert!(matches!(index.require_mfra(&wrong), Err(Error::Invalid(_))));
    assert!(MovieFragmentRandomAccessTail::parse(tail, end + 1).is_err());
    let mut short = vec![0; 4];
    short.extend(1u32.to_be_bytes());
    assert!(
        MovieFragmentRandomAccessTail::parse(raw(&pool, b"mfro", &short, end - 16), end).is_err()
    );
}
