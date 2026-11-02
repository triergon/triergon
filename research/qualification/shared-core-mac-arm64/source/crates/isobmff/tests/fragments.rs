mod common;
use common::{child, fixtures::CASES};
use safe_media_core::{Budget, Error, WorkBudget};
use safe_media_isobmff::{
    BuildStatus, FragmentBuilder, FragmentContext, ImplicitBase, Limits, SampleStep, Trex,
};

#[test]
fn sourced_absolute_base_fragments_match_independent_source_facts() {
    for case in CASES.iter().filter(|c| c.fragmented && !c.gap) {
        let budget = Budget::new(8 * 1024 * 1024).unwrap();
        let top = common::roots(&budget, case.bytes).unwrap();
        let moov = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
        let mvex = child(moov, b"mvex").unwrap();
        let trex = Trex::parse(&child(&mvex, b"trex").unwrap()).unwrap();
        let mut ordinal = 1u64;
        for moof in top.iter().filter(|b| b.header().kind == *b"moof") {
            let traf = child(moof, b"traf").unwrap();
            let wanted: Vec<_> = case
                .samples
                .iter()
                .filter(|e| e.ordinal >= ordinal)
                .take(3)
                .collect();
            let context = FragmentContext {
                moof_start: moof.header().start,
                implicit_base: ImplicitBase::FirstTraf,
                decode_origin: None,
                first_ordinal: ordinal,
                description_count: 1,
            };
            let mut builder = FragmentBuilder::new(
                traf.clone(),
                trex,
                context,
                budget.clone(),
                Limits::default(),
            )
            .unwrap();
            while builder.poll(&mut WorkBudget::new(1)).unwrap() != BuildStatus::Ready {}
            let index = builder.finish().unwrap();
            // ETSI TS 126 244 V18.0.0 §13.5 supplies the independent tfdt basis.
            assert_eq!(
                child(index.raw(), b"tfdt").unwrap().bytes().as_ref(),
                child(&traf, b"tfdt").unwrap().bytes().as_ref()
            );
            let mut cursor = index.cursor();
            for expected in wanted {
                let actual = loop {
                    match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
                        SampleStep::Sample(value) => break value,
                        SampleStep::Yield => {}
                        SampleStep::End => panic!(),
                    }
                };
                assert_eq!(actual.ordinal, expected.ordinal);
                assert_eq!(actual.range.start, expected.offset);
                assert_eq!(actual.dts_ticks, Some(expected.dts));
                assert_eq!(actual.cts_ticks, Some(expected.cts));
                assert_eq!(actual.description_index, expected.description);
                assert_eq!(actual.is_container_sync(), expected.sync);
                assert_eq!(
                    &case.bytes[actual.range.start as usize..actual.range.end as usize],
                    expected.payload
                );
                ordinal += 1;
            }
            loop {
                match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
                    SampleStep::End => break,
                    SampleStep::Yield => {}
                    _ => panic!(),
                }
            }
        }
        assert_eq!(ordinal - 1, case.samples.len() as u64);
    }
}

#[test]
fn modern_fragment_flags_remain_an_explicit_specification_gap() {
    let case = CASES.iter().find(|c| c.name == "f-explicit").unwrap();
    let budget = Budget::new(1024 * 1024).unwrap();
    let top = common::roots(&budget, case.bytes).unwrap();
    let moov = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let trex = Trex::parse(&child(&child(moov, b"mvex").unwrap(), b"trex").unwrap()).unwrap();
    let moof = top.iter().find(|b| b.header().kind == *b"moof").unwrap();
    let ctx = FragmentContext {
        moof_start: moof.header().start,
        implicit_base: ImplicitBase::FirstTraf,
        decode_origin: None,
        first_ordinal: 1,
        description_count: 1,
    };
    let mut builder = FragmentBuilder::new(
        child(moof, b"traf").unwrap(),
        trex,
        ctx,
        budget,
        Limits::default(),
    )
    .unwrap();
    let outcome = loop {
        match builder.poll(&mut WorkBudget::new(1)) {
            Ok(BuildStatus::Yield) => {}
            result => break result,
        }
    };
    assert!(matches!(outcome, Err(Error::Unsupported(_))));
}

fn original_traf(
    budget: &Budget,
    track_id: u32,
    offsets: &[Option<i32>],
) -> safe_media_isobmff::BoxView {
    let mut children = Vec::new();
    children.extend_from_slice(&16u32.to_be_bytes());
    children.extend_from_slice(b"tfhd");
    children.extend_from_slice(&0u32.to_be_bytes());
    children.extend_from_slice(&track_id.to_be_bytes());
    for offset in offsets {
        children.extend_from_slice(&(if offset.is_some() { 20u32 } else { 16 }).to_be_bytes());
        children.extend_from_slice(b"trun");
        children.extend_from_slice(&u32::from(offset.is_some()).to_be_bytes());
        children.extend_from_slice(&1u32.to_be_bytes());
        if let Some(offset) = offset {
            children.extend_from_slice(&offset.to_be_bytes());
        }
    }
    let mut raw = u32::try_from(8 + children.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    raw.extend_from_slice(b"traf");
    raw.extend_from_slice(&children);
    safe_media_isobmff::BoxView::new(common::bytes(budget, &raw), 0).unwrap()
}

fn original_index(
    budget: &Budget,
    track_id: u32,
    offsets: &[Option<i32>],
    implicit_base: ImplicitBase,
) -> safe_media_isobmff::FragmentIndex {
    let mut builder = FragmentBuilder::new(
        original_traf(budget, track_id, offsets),
        Trex {
            track_id,
            description_index: 1,
            duration: 2,
            size: 4,
            flags: 0x02000000,
        },
        FragmentContext {
            moof_start: 1u64 << 32,
            implicit_base,
            decode_origin: None,
            first_ordinal: 1,
            description_count: 1,
        },
        budget.clone(),
        Limits::default(),
    )
    .unwrap();
    while builder.poll(&mut WorkBudget::new(1)).unwrap() != BuildStatus::Ready {}
    builder.finish().unwrap()
}

#[test]
fn legacy_implicit_traf_chaining_uses_source_bytes_and_leaves_missing_time_unknown() {
    let budget = Budget::new(4096).unwrap();
    let first = original_index(&budget, 1, &[Some(128)], ImplicitBase::FirstTraf);
    let end = first.implicit_data_end().unwrap();
    assert_eq!(end, (1u64 << 32) + 132);
    let second = original_index(&budget, 2, &[None], ImplicitBase::PreviousTraf(end));
    assert_eq!(second.track_id(), 2);
    let SampleStep::Sample(sample) = second.cursor().poll(&mut WorkBudget::new(1)).unwrap() else {
        panic!()
    };
    assert_eq!(sample.range, end..end + 4);
    assert_eq!(sample.dts_ticks, None);
    assert_eq!(sample.cts_ticks, None);
}

#[test]
fn backward_fragment_run_is_preserved_but_not_used_as_an_implicit_predecessor() {
    let budget = Budget::new(4096).unwrap();
    let index = original_index(&budget, 1, &[Some(128), Some(64)], ImplicitBase::FirstTraf);
    assert_eq!(index.data_end(), (1u64 << 32) + 68);
    assert!(matches!(
        index.implicit_data_end(),
        Err(Error::Unsupported(_))
    ));
}
