mod common;
use common::{child, fixtures::CASES};
use safe_media_core::{Budget, Error, WorkBudget};
use safe_media_isobmff::{EditList, MovieHeader, SampleStep, TrackHeader};

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
            let edits = EditList::parse(
                child(&edts, b"elst")?,
                safe_media_isobmff::Limits::default(),
            )?;
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
        } else if case.gap {
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
    assert!(indexed >= 20 && unsupported >= 5 && malformed >= 8);
}

#[test]
fn sparse_co64_offsets_never_narrow_to_host_slice_indices() {
    use common::fixtures::{SPARSE_LENGTH, SPARSE_OFFSET, SPARSE_PREFIX};
    use safe_media_isobmff::{BoxHeader, BoxView, HeaderRead};
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
