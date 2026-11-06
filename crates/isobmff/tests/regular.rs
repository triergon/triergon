mod common;
use common::{child, fixtures::CASES};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    EditList, Limits, MovieHeader, SampleStep, SampleTable, SampleTableBuilder, SampleTablePolicy,
    SyncSamplePolicy, TrackHeader,
};

fn sync_table(bytes: &[u8], policy: SyncSamplePolicy, quantum: u64) -> Result<SampleTable, Error> {
    let budget = Budget::new(1 << 20)?;
    let top = common::roots(&budget, bytes)?;
    let moov = top
        .iter()
        .find(|b| b.header().kind == *b"moov")
        .ok_or(Error::Invalid("missing movie"))?;
    let media = child(&child(moov, b"trak")?, b"mdia")?;
    let stbl = child(&child(&media, b"minf")?, b"stbl")?;
    let mut builder = SampleTableBuilder::with_policy(
        stbl.clone(),
        budget,
        Limits::default(),
        SampleTablePolicy {
            sync_samples: policy,
            ..SampleTablePolicy::default()
        },
    )?;
    for _ in 0..1000 {
        if builder.poll(&mut WorkBudget::new(quantum))? == triergon_isobmff::BuildStatus::Ready {
            let table = builder.finish()?;
            assert_eq!(table.raw().bytes().as_ref(), stbl.bytes().as_ref());
            return Ok(table);
        }
    }
    panic!("sample table did not complete")
}

#[test]
fn bounded_sync_keeps_only_the_reachable_prefix() {
    let unreachable = include_bytes!("fixtures/bad-stss-out-of-range.mov");
    let mut reachable = unreachable.to_vec();
    let start = reachable
        .windows(4)
        .position(|part| part == b"stss")
        .unwrap()
        + 12;
    assert_eq!(&reachable[start..start + 8], &[0, 0, 0, 100, 0, 0, 0, 4]);
    reachable[start..start + 4].copy_from_slice(&1u32.to_be_bytes());
    reachable[start + 4..start + 8].copy_from_slice(&100u32.to_be_bytes());
    let error = Error::Invalid("sync sample order/range");
    for quantum in [1, 4096] {
        for (bytes, unused, first_sync) in [
            (unreachable.as_slice(), 2, None),
            (reachable.as_slice(), 1, Some(1)),
        ] {
            assert_eq!(
                sync_table(bytes, SyncSamplePolicy::Strict, quantum).unwrap_err(),
                error
            );
            let table = sync_table(bytes, SyncSamplePolicy::BoundedPrefix, quantum).unwrap();
            assert_eq!(table.sync_entry_count(), Some(2));
            assert_eq!(table.unused_sync_entries(), unused);
            assert_eq!(
                table.previous_sync(table.sample_count()).unwrap(),
                first_sync
            );
            let mut cursor = table.cursor();
            for ordinal in 1..=table.sample_count() {
                let SampleStep::Sample(sample) = cursor.poll(&mut WorkBudget::new(1)).unwrap()
                else {
                    panic!("missing sample {ordinal}");
                };
                assert_eq!(sample.ordinal, ordinal);
                assert_eq!(sample.is_container_sync(), Some(ordinal) == first_sync);
            }
        }
    }
    reachable[start..start + 4].copy_from_slice(&4u32.to_be_bytes());
    reachable[start + 4..start + 8].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(
        sync_table(&reachable, SyncSamplePolicy::BoundedPrefix, 1).unwrap_err(),
        error
    );
}

fn exercise(case: &common::fixtures::Case) -> Result<(), Error> {
    let budget = Budget::new(8 * 1024 * 1024)?;
    let top = common::roots(&budget, case.bytes)?;
    let moov = top
        .iter()
        .find(|b| b.header().kind == *b"moov")
        .ok_or(Error::Invalid("missing movie"))?;
    let movie = MovieHeader::parse(&child(moov, b"mvhd")?)?;
    assert!(movie.timescale > 0);
    let media: Vec<_> = top
        .iter()
        .filter(|b| b.header().kind == *b"mdat")
        .map(|b| b.header().payload_range().unwrap())
        .collect();
    for track in common::children(moov, 0)?
        .iter()
        .filter(|b| b.header().kind == *b"trak")
    {
        let mdia = child(track, b"mdia")?;
        let header = TrackHeader::parse(
            &child(track, b"tkhd")?,
            &child(&mdia, b"mdhd")?,
            &child(&mdia, b"hdlr")?,
        )?;
        let stbl = child(&child(&mdia, b"minf")?, b"stbl")?;
        let index = common::table(stbl, &budget, 1)?;
        if case.malformed {
            let mut cursor = index.cursor();
            loop {
                match cursor.poll(&mut WorkBudget::new(1))? {
                    SampleStep::Sample(s) => {
                        if !media.iter().any(|r| s.require_within(r.clone()).is_ok()) {
                            return Err(Error::Invalid("sample outside source mdat"));
                        }
                    }
                    SampleStep::End => break,
                    SampleStep::Yield => {}
                }
            }
            continue;
        }
        let wanted: Vec<_> = case
            .samples
            .iter()
            .filter(|s| s.track == header.track_id)
            .collect();
        assert_eq!(index.sample_count(), wanted.len() as u64, "{}", case.name);
        let mut cursor = index.cursor();
        assert!(matches!(
            cursor.poll(&mut WorkBudget::new(0))?,
            SampleStep::Yield | SampleStep::End
        ));
        for expected in wanted {
            let actual = match cursor.poll(&mut WorkBudget::new(1))? {
                SampleStep::Sample(s) => s,
                other => panic!("{other:?}"),
            };
            assert_eq!(actual.ordinal, expected.ordinal, "{}", case.name);
            assert_eq!(
                actual.description_index, expected.description,
                "{}",
                case.name
            );
            assert_eq!(actual.range.start, expected.offset, "{}", case.name);
            assert_eq!(actual.dts_ticks, Some(expected.dts), "{}", case.name);
            assert_eq!(actual.cts_ticks, Some(expected.cts), "{}", case.name);
            assert_eq!(actual.duration_ticks, expected.delta, "{}", case.name);
            assert_eq!(actual.is_container_sync(), expected.sync, "{}", case.name);
            assert!(
                media
                    .iter()
                    .any(|r| actual.require_within(r.clone()).is_ok())
            );
            assert_eq!(
                &case.bytes[actual.range.start as usize..actual.range.end as usize],
                expected.payload
            );
            let mut locate = index.locate(expected.ordinal)?;
            let located = loop {
                match locate.poll(&mut WorkBudget::new(1))? {
                    SampleStep::Sample(s) => break s,
                    SampleStep::Yield => {}
                    SampleStep::End => panic!("missing located sample"),
                }
            };
            assert_eq!(actual, located);
        }
        assert!(matches!(
            cursor.poll(&mut WorkBudget::new(1))?,
            SampleStep::End
        ));
        assert!(index.locate(0).is_err());
        assert!(index.locate(index.sample_count() + 1).is_err());
        if let Ok(edts) = child(track, b"edts") {
            let edits =
                EditList::parse(child(&edts, b"elst")?, triergon_isobmff::Limits::default())?;
            assert!(!edits.is_empty());
            assert!(edits.entry(edits.len()).is_err());
        }
    }
    Ok(())
}

#[test]
fn original_regular_corpus_and_explicit_specification_gaps() {
    let mut indexed = 0;
    let mut unsupported = 0;
    let mut malformed = 0;
    for case in CASES.iter().filter(|c| !c.fragmented) {
        let result = exercise(case);
        if case.malformed {
            assert!(result.is_err(), "{} was accepted", case.name);
            malformed += 1;
        } else if case.gap
            && !matches!(
                case.name,
                "r-ctts-v1" | "r-negative-cts" | "r-stz2-4" | "r-stz2-8" | "r-stz2-16"
            )
        {
            assert!(
                matches!(result, Err(Error::Unsupported(_))),
                "{}: {result:?}",
                case.name
            );
            unsupported += 1;
        } else {
            result.unwrap_or_else(|e| panic!("{}: {e}", case.name));
            indexed += 1;
        }
    }
    assert_eq!((indexed, unsupported, malformed), (30, 0, 10));
}

#[test]
fn required_signed_ctts_v1_preserves_raw_bits_and_negative_source_time() {
    // Published ISO/IEC 14496-12:2008/Amd.1:2009 §§8.6.1.3.1-.3.
    // Native clocks stay separate.
    for name in ["r-ctts-v1", "r-negative-cts"] {
        let case = CASES.iter().find(|c| c.name == name).unwrap();
        for quantum in [1, 4096] {
            let budget = Budget::new(1024 * 1024).unwrap();
            let top = common::roots(&budget, case.bytes).unwrap();
            let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
            let track = child(movie, b"trak").unwrap();
            let mdia = child(&track, b"mdia").unwrap();
            let header = TrackHeader::parse(
                &child(&track, b"tkhd").unwrap(),
                &child(&mdia, b"mdhd").unwrap(),
                &child(&mdia, b"hdlr").unwrap(),
            )
            .unwrap();
            let stbl = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
            let table_result = common::table(stbl.clone(), &budget, quantum);
            assert!(
                table_result.is_ok(),
                "required signed ctts v1 refused: {name}, quantum={quantum}, got {:?}",
                table_result.as_ref().err()
            );
            let table = table_result.unwrap();
            assert_eq!(table.raw().bytes().as_ref(), stbl.bytes().as_ref());
            let mut cursor = table.cursor();
            for expected in case.samples {
                let mut actual = None;
                for _ in 0..10_000 {
                    match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
                        SampleStep::Yield => {}
                        SampleStep::Sample(value) => {
                            actual = Some(value);
                            break;
                        }
                        SampleStep::End => panic!("missing required source sample"),
                    }
                }
                let actual = actual.expect("bounded signed source cursor must make progress");
                assert_eq!(actual.ordinal, expected.ordinal);
                assert_eq!(actual.range.start, expected.offset);
                assert_eq!(actual.dts_ticks, Some(expected.dts));
                assert_eq!(actual.cts_ticks, Some(expected.cts));
                assert_eq!(actual.duration_ticks, expected.delta);
                assert_eq!(actual.description_index, expected.description);
                assert_eq!(actual.is_container_sync(), expected.sync);
                let signed = i32::try_from(expected.cts - expected.dts).unwrap();
                let wire = actual
                    .composition_offset
                    .expect("present ctts retains explicit zero too");
                assert_eq!(wire.version(), 1);
                assert_eq!(wire.raw_bits(), u32::from_be_bytes(signed.to_be_bytes()));
                assert_eq!(wire.signed_value(), Some(signed));
                assert_eq!(wire.ticks(), i128::from(signed));
                if expected.cts < 0 {
                    assert!(
                        actual
                            .source_times(header.time_base().unwrap())
                            .unwrap()
                            .cts
                            .unwrap()
                            .numerator()
                            < 0
                    );
                }
                assert_eq!(
                    &case.bytes[actual.range.start as usize..actual.range.end as usize],
                    expected.payload
                );
                let mut locator = table.locate(expected.ordinal).unwrap();
                let mut located = None;
                for _ in 0..10_000 {
                    match locator.poll(&mut WorkBudget::new(quantum)).unwrap() {
                        SampleStep::Yield => {}
                        SampleStep::Sample(value) => {
                            located = Some(value);
                            break;
                        }
                        SampleStep::End => panic!("missing located signed sample"),
                    }
                }
                assert_eq!(located, Some(actual));
            }
            let mut ended = false;
            for _ in 0..10_000 {
                match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
                    SampleStep::Yield => {}
                    SampleStep::End => {
                        ended = true;
                        break;
                    }
                    SampleStep::Sample(_) => panic!("unexpected extra signed source sample"),
                }
            }
            assert!(ended);
        }
    }
}

#[test]
fn sparse_co64_offsets_never_narrow_to_host_slice_indices() {
    use common::fixtures::{SPARSE_LENGTH, SPARSE_OFFSET, SPARSE_PREFIX};
    use triergon_isobmff::{BoxHeader, BoxView, HeaderRead};
    let budget = Budget::new(1024 * 1024).unwrap();
    let storage = common::bytes(&budget, SPARSE_PREFIX);
    let mut offset = 0usize;
    let mut moov = None;
    while offset + 8 <= storage.len() {
        let HeaderRead::Ready(h) =
            BoxHeader::parse(&storage[offset..], offset as u64, Some(SPARSE_LENGTH), true).unwrap()
        else {
            panic!()
        };
        if h.kind == *b"mdat" {
            assert_eq!(h.end().unwrap(), Some(SPARSE_LENGTH));
            break;
        }
        let end = offset + usize::try_from(h.size.unwrap()).unwrap();
        if h.kind == *b"moov" {
            moov = Some(BoxView::new(storage.slice(offset..end).unwrap(), offset as u64).unwrap());
        }
        offset = end;
    }
    let track = child(&moov.unwrap(), b"trak").unwrap();
    let stbl = child(
        &child(&child(&track, b"mdia").unwrap(), b"minf").unwrap(),
        b"stbl",
    )
    .unwrap();
    let index = common::table(stbl, &budget, 1).unwrap();
    let mut locate = index.locate(6).unwrap();
    let sample = loop {
        match locate.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Sample(s) => break s,
            SampleStep::Yield => {}
            _ => panic!(),
        }
    };
    assert_eq!(sample.range, SPARSE_OFFSET + 60..SPARSE_LENGTH);
    sample.require_within(SPARSE_OFFSET..SPARSE_LENGTH).unwrap();
    assert!(sample.require_within(0..SPARSE_OFFSET).is_err());
}
