mod common;
use triergon_core::{Budget, Error, Time, WorkBudget};
use triergon_isobmff::{BoxView, IndexStep, Limits, SegmentIndex};

fn index_box(
    budget: &Budget,
    version: u8,
    start: u64,
    earliest: u64,
    first: u64,
    records: &[[u32; 3]],
) -> BoxView {
    let mut body = vec![version, 0, 0, 0];
    body.extend_from_slice(&7u32.to_be_bytes());
    body.extend_from_slice(&90000u32.to_be_bytes());
    if version == 0 {
        body.extend_from_slice(&u32::try_from(earliest).unwrap().to_be_bytes());
        body.extend_from_slice(&u32::try_from(first).unwrap().to_be_bytes());
    } else {
        body.extend_from_slice(&earliest.to_be_bytes());
        body.extend_from_slice(&first.to_be_bytes());
    }
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&u16::try_from(records.len()).unwrap().to_be_bytes());
    for record in records {
        for value in record {
            body.extend_from_slice(&value.to_be_bytes());
        }
    }
    let mut raw = u32::try_from(8 + body.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    raw.extend_from_slice(b"sidx");
    raw.extend_from_slice(&body);
    BoxView::new(common::bytes(budget, &raw), start).unwrap()
}

#[test]
fn normative_sidx_cursor_preserves_references_sap_and_exact_movie_times() {
    let budget = Budget::new(4096).unwrap();
    let raw = index_box(
        &budget,
        0,
        100,
        90000,
        32,
        &[[40, 45000, 0x90000000], [0x80000018, 90000, 0x20000007]],
    );
    let index = SegmentIndex::parse(raw.clone(), Limits::default()).unwrap();
    assert_eq!(index.reference_id(), 7);
    assert_eq!(index.timescale(), 90000);
    assert_eq!(index.first_offset(), 32);
    assert_eq!(index.raw().bytes().as_ref(), raw.bytes().as_ref());
    let before = budget.used();
    let mut cursor = index.cursor();
    assert!(matches!(
        cursor.poll(&mut WorkBudget::new(0)).unwrap(),
        IndexStep::Yield
    ));
    let IndexStep::Reference(first) = cursor.poll(&mut WorkBudget::new(1)).unwrap() else {
        panic!()
    };
    assert_eq!(first.range, 188..228);
    assert!(!first.references_index);
    assert_eq!(first.earliest_presentation, Time::new(1, 1).unwrap());
    assert_eq!(first.presentation_duration, Time::new(1, 2).unwrap());
    assert!(first.starts_with_sap);
    assert_eq!(first.sap_type, 1);
    let IndexStep::Reference(second) = cursor.poll(&mut WorkBudget::new(1)).unwrap() else {
        panic!()
    };
    assert_eq!(second.range, 228..252);
    assert!(second.references_index);
    assert_eq!(second.earliest_presentation, Time::new(3, 2).unwrap());
    assert_eq!(second.sap_type, 2);
    assert_eq!(second.sap_delta_ticks, 7);
    assert!(matches!(
        cursor.poll(&mut WorkBudget::new(1)).unwrap(),
        IndexStep::End
    ));
    assert_eq!(budget.used(), before);
}

#[test]
fn sidx_large_unsigned_time_and_sparse_offsets_do_not_narrow_to_i64_or_u32() {
    let budget = Budget::new(4096).unwrap();
    let raw = index_box(
        &budget,
        1,
        1u64 << 32,
        1u64 << 63,
        (1u64 << 32) + 9,
        &[[64, 3, 0x70000005]],
    );
    let end = raw.header().end().unwrap().unwrap();
    let index = SegmentIndex::parse(raw, Limits::default()).unwrap();
    let IndexStep::Reference(reference) = index.cursor().poll(&mut WorkBudget::new(1)).unwrap()
    else {
        panic!()
    };
    assert_eq!(reference.range.start, end + (1u64 << 32) + 9);
    assert_eq!(
        reference.earliest_presentation,
        Time::new(1i128 << 63, 90000).unwrap()
    );
    assert_eq!(reference.sap_type, 7); // Reserved, preserved; no recovery claim.
    assert_eq!(reference.raw_sap_word, 0x70000005);
}

#[test]
fn sidx_limits_and_range_overflow_are_explicit_without_partial_cursor_commit() {
    let budget = Budget::new(4096).unwrap();
    let raw = index_box(&budget, 0, u64::MAX - 64, 0, 0, &[[100, 1, 0]]);
    let index = SegmentIndex::parse(raw.clone(), Limits::default()).unwrap();
    let mut cursor = index.cursor();
    for _ in 0..2 {
        assert!(matches!(
            cursor.poll(&mut WorkBudget::new(1)),
            Err(Error::Overflow)
        ));
    }
    assert!(matches!(
        SegmentIndex::parse(
            raw,
            Limits {
                max_records: 0,
                ..Limits::default()
            }
        ),
        Err(Error::Limit { .. })
    ));
}
