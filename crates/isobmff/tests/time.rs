mod common;
use common::child;
use triergon_core::{Budget, Time, TimeBase, WorkBudget};
use triergon_isobmff::{EditList, Limits, MovieHeader, SampleStep, TrackHeader};

#[test]
fn prime_scale_edit_mapping_keeps_the_independent_fraction_exact() {
    let case = common::fixtures::CASES
        .iter()
        .find(|c| c.name == "r-prime-clock-edit")
        .unwrap();
    let budget = Budget::new(1024 * 1024).unwrap();
    let top = common::roots(&budget, case.bytes).unwrap();
    let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let movie_header = MovieHeader::parse(&child(movie, b"mvhd").unwrap()).unwrap();
    let track = child(movie, b"trak").unwrap();
    let mdia = child(&track, b"mdia").unwrap();
    let header = TrackHeader::parse(
        &child(&track, b"tkhd").unwrap(),
        &child(&mdia, b"mdhd").unwrap(),
        &child(&mdia, b"hdlr").unwrap(),
    )
    .unwrap();
    let table = common::table(
        child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap(),
        &budget,
        1,
    )
    .unwrap();
    let mut locate = table.locate(2).unwrap();
    let sample = loop {
        match locate.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Sample(s) => break s,
            SampleStep::Yield => {}
            _ => panic!(),
        }
    };
    let times = sample.source_times(header.time_base().unwrap()).unwrap();
    assert_eq!(times.dts.unwrap().numerator(), 1);
    assert_eq!(times.dts.unwrap().denominator(), 1_000_003);
    let edits = EditList::parse(
        child(&child(&track, b"edts").unwrap(), b"elst").unwrap(),
        Limits::default(),
    )
    .unwrap();
    let gap = edits.entry(0).unwrap();
    assert!(gap.is_empty());
    let start = gap.duration(movie_header.time_base().unwrap()).unwrap();
    let edit = edits.entry(1).unwrap();
    let mapped = edit
        .map_rate_one(times.cts.unwrap(), start, header.time_base().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        (mapped.numerator(), mapped.denominator()),
        (1, 1_000_000_007)
    );
    let tail = times
        .decode_delta
        .checked_sub(edit.duration(movie_header.time_base().unwrap()).unwrap())
        .unwrap();
    // Independent Fraction oracle: no nanosecond-rounding or u32 denominator.
    assert_eq!(
        (tail.numerator(), tail.denominator()),
        (337_666, 1_000_003_007_000_021)
    );
}

#[test]
fn empty_and_non_unit_edit_rates_do_not_fabricate_point_mappings() {
    use triergon_core::Error;
    use triergon_isobmff::EditEntry;
    let base = TimeBase::new(1, 48_000).unwrap();
    let zero = Time::new(0, 1).unwrap();
    let empty = EditEntry {
        segment_duration: 1000,
        media_time: -1,
        rate_integer: 1,
        rate_fraction: 0,
    };
    assert_eq!(empty.map_rate_one(zero, zero, base).unwrap(), None);
    let dwell = EditEntry {
        media_time: 0,
        rate_integer: 0,
        ..empty
    };
    assert!(matches!(
        dwell.map_rate_one(zero, zero, base),
        Err(Error::Unsupported(_))
    ));
}
