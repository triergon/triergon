mod common;
use common::{child, fixtures::CASES};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
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
fn selected_draft_signed_trun_preserves_frozen_source_and_raw_bits() {
    // Experimental field support from FDAM3 and MPEG contribution Original.
    let case = CASES.iter().find(|c| c.name == "f-ctts-v1").unwrap();
    let budget = Budget::new(1024 * 1024).unwrap();
    let top = common::roots(&budget, case.bytes).unwrap();
    let moov = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let trex = Trex::parse(&child(&child(moov, b"mvex").unwrap(), b"trex").unwrap()).unwrap();
    let mut ordinal = 1;
    for moof in top.iter().filter(|b| b.header().kind == *b"moof") {
        let raw = child(moof, b"traf").unwrap();
        let mut builder = FragmentBuilder::new(
            raw.clone(),
            trex,
            FragmentContext {
                moof_start: moof.header().start,
                implicit_base: ImplicitBase::FirstTraf,
                decode_origin: None,
                first_ordinal: ordinal,
                description_count: 1,
            },
            budget.clone(),
            Limits::default(),
        )
        .unwrap();
        while builder.poll(&mut WorkBudget::new(1)).unwrap() != BuildStatus::Ready {}
        let index = builder.finish().unwrap();
        assert_eq!(index.raw().bytes().as_ref(), raw.bytes().as_ref());
        let mut cursor = index.cursor();
        let first = ordinal;
        for wanted in case.samples.iter().filter(|s| s.ordinal >= first).take(3) {
            let source = loop {
                match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
                    SampleStep::Yield => {}
                    SampleStep::Sample(value) => break value,
                    SampleStep::End => panic!("missing frozen signed-trun sample"),
                }
            };
            assert_eq!(source.ordinal, wanted.ordinal);
            assert_eq!(source.range.start, wanted.offset);
            assert_eq!(source.dts_ticks, Some(wanted.dts));
            assert_eq!(source.cts_ticks, Some(wanted.cts));
            assert_eq!(source.duration_ticks, wanted.delta);
            assert_eq!(source.description_index, wanted.description);
            assert_eq!(source.is_container_sync(), wanted.sync);
            if let Some(offset) = source.composition_offset {
                let expected = i32::try_from(wanted.cts - wanted.dts).unwrap();
                assert_eq!(offset.version(), 1);
                assert_eq!(
                    offset.raw_bits(),
                    u32::from_be_bytes(expected.to_be_bytes())
                );
                assert_eq!(offset.signed_value(), Some(expected));
            } else {
                assert_eq!(wanted.cts, wanted.dts);
            }
            assert_eq!(
                &case.bytes[source.range.start as usize..source.range.end as usize],
                wanted.payload
            );
            ordinal += 1;
        }
        loop {
            match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
                SampleStep::Yield => {}
                SampleStep::End => break,
                _ => panic!("extra signed-trun sample"),
            }
        }
    }
    assert_eq!(ordinal - 1, case.samples.len() as u64);
}

#[test]
fn required_moof_relative_fragment_preserves_frozen_source_ranges_and_tfdt() {
    // This tests extracted fragment syntax, not whole-file brand conformance.
    let case = CASES.iter().find(|c| c.name == "f-explicit").unwrap();
    for quantum in [1, 4096] {
        let budget = Budget::new(1024 * 1024).unwrap();
        let top = common::roots(&budget, case.bytes).unwrap();
        let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
        let trex = Trex::parse(&child(&child(movie, b"mvex").unwrap(), b"trex").unwrap()).unwrap();
        let mut ordinal = 1;
        for moof in top.iter().filter(|b| b.header().kind == *b"moof") {
            let traf = child(moof, b"traf").unwrap();
            let expected_time =
                triergon_isobmff::FragmentDecodeTime::parse(child(&traf, b"tfdt").unwrap())
                    .unwrap();
            let mut builder = FragmentBuilder::new(
                traf.clone(),
                trex,
                FragmentContext {
                    moof_start: moof.header().start,
                    implicit_base: ImplicitBase::FirstTraf,
                    decode_origin: None,
                    first_ordinal: ordinal,
                    description_count: 1,
                },
                budget.clone(),
                Limits::default(),
            )
            .unwrap();
            let mut ready = false;
            for _ in 0..10_000 {
                let result = builder.poll(&mut WorkBudget::new(quantum));
                assert!(
                    result.is_ok(),
                    "required iso6 moof-relative fragment was refused; quantum={quantum}, got {result:?}"
                );
                if result.unwrap() == BuildStatus::Ready {
                    ready = true;
                    break;
                }
            }
            assert!(ready, "bounded modern-fragment build must make progress");
            let index = builder.finish().unwrap();
            assert_eq!(index.sample_count(), 3);
            assert_eq!(index.raw().bytes().as_ref(), traf.bytes().as_ref());
            let decode_time = index.decode_time().unwrap();
            assert_eq!(decode_time.version(), expected_time.version());
            assert_eq!(decode_time.ticks(), expected_time.ticks());
            let mut cursor = index.cursor();
            let first = ordinal;
            for expected in case.samples.iter().filter(|s| s.ordinal >= first).take(3) {
                let mut actual = None;
                for _ in 0..10_000 {
                    match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
                        SampleStep::Yield => {}
                        SampleStep::Sample(sample) => {
                            actual = Some(sample);
                            break;
                        }
                        SampleStep::End => panic!("missing frozen source sample"),
                    }
                }
                let actual = actual.expect("bounded sample cursor must make progress");
                assert_eq!(actual.ordinal, expected.ordinal);
                assert_eq!(actual.range.start, expected.offset);
                assert_eq!(
                    actual.range.end - actual.range.start,
                    expected.payload.len() as u64
                );
                assert_eq!(actual.dts_ticks, Some(expected.dts));
                assert_eq!(actual.cts_ticks, Some(expected.cts));
                assert_eq!(actual.duration_ticks, expected.delta);
                assert_eq!(actual.description_index, expected.description);
                assert_eq!(actual.is_container_sync(), expected.sync);
                assert_eq!(
                    &case.bytes[actual.range.start as usize..actual.range.end as usize],
                    expected.payload
                );
                ordinal += 1;
            }
            let mut ended = false;
            for _ in 0..10_000 {
                match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
                    SampleStep::Yield => {}
                    SampleStep::End => {
                        ended = true;
                        break;
                    }
                    SampleStep::Sample(_) => panic!("unexpected extra source sample"),
                }
            }
            assert!(ended, "bounded sample cursor must reach its declared end");
        }
        assert_eq!(ordinal - 1, case.samples.len() as u64);
    }
}

fn original_traf(
    budget: &Budget,
    track_id: u32,
    offsets: &[Option<i32>],
) -> triergon_isobmff::BoxView {
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
    triergon_isobmff::BoxView::new(common::bytes(budget, &raw), 0).unwrap()
}

fn original_index(
    budget: &Budget,
    track_id: u32,
    offsets: &[Option<i32>],
    implicit_base: ImplicitBase,
) -> triergon_isobmff::FragmentIndex {
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

#[test]
fn original_modern_fragment_profiles_preserve_facts_and_unknown_clocks() {
    struct TrackState {
        id: u32,
        next: u64,
        descriptions: u32,
        defaults: Trex,
    }
    let mut exercised = 0;
    for case in CASES
        .iter()
        .filter(|c| c.fragmented && c.gap && c.name != "f-ctts-v1")
    {
        for quantum in [1, 4096] {
            let budget = Budget::new(8 * 1024 * 1024).unwrap();
            let top = common::roots(&budget, case.bytes).unwrap();
            let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
            let defaults: Vec<_> = common::children(&child(movie, b"mvex").unwrap(), 0)
                .unwrap()
                .iter()
                .filter(|b| b.header().kind == *b"trex")
                .map(|b| Trex::parse(b).unwrap())
                .collect();
            let mut tracks = Vec::new();
            for track in common::children(movie, 0)
                .unwrap()
                .iter()
                .filter(|b| b.header().kind == *b"trak")
            {
                let media = child(track, b"mdia").unwrap();
                let header = triergon_isobmff::TrackHeader::parse(
                    &child(track, b"tkhd").unwrap(),
                    &child(&media, b"mdhd").unwrap(),
                    &child(&media, b"hdlr").unwrap(),
                )
                .unwrap();
                let table = common::table(
                    child(&child(&media, b"minf").unwrap(), b"stbl").unwrap(),
                    &budget,
                    quantum,
                )
                .unwrap();
                tracks.push(TrackState {
                    id: header.track_id,
                    next: 1,
                    descriptions: u32::try_from(table.descriptions().len()).unwrap(),
                    defaults: *defaults
                        .iter()
                        .find(|t| t.track_id == header.track_id)
                        .unwrap(),
                });
            }
            for moof in top.iter().filter(|b| b.header().kind == *b"moof") {
                for traf in common::children(moof, 0)
                    .unwrap()
                    .iter()
                    .filter(|b| b.header().kind == *b"traf")
                {
                    let tfhd = child(traf, b"tfhd").unwrap();
                    let id = u32::from_be_bytes(tfhd.payload()[4..8].try_into().unwrap());
                    let state = tracks.iter_mut().find(|t| t.id == id).unwrap();
                    let mut builder = FragmentBuilder::new(
                        traf.clone(),
                        state.defaults,
                        FragmentContext {
                            moof_start: moof.header().start,
                            implicit_base: ImplicitBase::FirstTraf,
                            decode_origin: None,
                            first_ordinal: state.next,
                            description_count: state.descriptions,
                        },
                        budget.clone(),
                        Limits::default(),
                    )
                    .unwrap();
                    let mut ready = false;
                    for _ in 0..10_000 {
                        if builder.poll(&mut WorkBudget::new(quantum)).unwrap()
                            == BuildStatus::Ready
                        {
                            ready = true;
                            break;
                        }
                    }
                    assert!(ready, "bounded original profile stalled: {}", case.name);
                    let index = builder.finish().unwrap();
                    assert_eq!(index.raw().bytes().as_ref(), traf.bytes().as_ref());
                    let first = state.next;
                    let wanted: Vec<_> = case
                        .samples
                        .iter()
                        .filter(|s| s.track == id && s.ordinal >= first)
                        .take(usize::try_from(index.sample_count()).unwrap())
                        .collect();
                    assert_eq!(wanted.len() as u64, index.sample_count());
                    let mut cursor = index.cursor();
                    for expected in wanted {
                        let mut found = None;
                        for _ in 0..10_000 {
                            match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
                                SampleStep::Yield => {}
                                SampleStep::Sample(value) => {
                                    found = Some(value);
                                    break;
                                }
                                SampleStep::End => panic!("missing original source sample"),
                            }
                        }
                        let sample = found.expect("bounded original fragment cursor stalled");
                        assert_eq!(sample.ordinal, expected.ordinal);
                        assert_eq!(sample.range.start, expected.offset);
                        assert_eq!(
                            sample.range.end - sample.range.start,
                            expected.payload.len() as u64
                        );
                        assert_eq!(sample.duration_ticks, expected.delta);
                        assert_eq!(sample.description_index, expected.description);
                        assert_eq!(sample.is_container_sync(), expected.sync);
                        if index.decode_time().is_some() {
                            assert_eq!(sample.dts_ticks, Some(expected.dts), "{}", case.name);
                            assert_eq!(sample.cts_ticks, Some(expected.cts), "{}", case.name);
                        } else {
                            // No clock is inferred from the preceding fragment or
                            // the authored intent when this traf omits tfdt.
                            assert_eq!(sample.dts_ticks, None, "{}", case.name);
                            assert_eq!(sample.cts_ticks, None, "{}", case.name);
                        }
                        assert_eq!(
                            &case.bytes[sample.range.start as usize..sample.range.end as usize],
                            expected.payload
                        );
                        assert!(top.iter().filter(|b| b.header().kind == *b"mdat").any(|b| {
                            sample
                                .require_within(b.header().payload_range().unwrap())
                                .is_ok()
                        }));
                        state.next += 1;
                    }
                    let mut ended = false;
                    for _ in 0..10_000 {
                        match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
                            SampleStep::Yield => {}
                            SampleStep::End => {
                                ended = true;
                                break;
                            }
                            SampleStep::Sample(_) => panic!("extra original source sample"),
                        }
                    }
                    assert!(ended, "bounded original fragment cursor did not finish");
                }
            }
            for track in tracks {
                assert_eq!(
                    track.next - 1,
                    case.samples.iter().filter(|s| s.track == track.id).count() as u64
                );
            }
        }
        exercised += 1;
    }
    assert_eq!(exercised, 13);
}
