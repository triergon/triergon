use crate::common;
use std::ops::Range;
use triergon_core::{Budget, WorkBudget};
use triergon_isobmff::{
    BoxView, BuildStatus, EditList, FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase,
    Limits, SampleStep, SampleTable, Trex,
};

pub fn regular(pool: &Budget, name: &str) -> (SampleTable, Vec<Range<u64>>) {
    let case = common::fixtures::CASES
        .iter()
        .find(|c| c.name == name)
        .unwrap();
    let top = common::roots(pool, case.bytes).unwrap();
    let moov = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let trak = common::child(moov, b"trak").unwrap();
    let mdia = common::child(&trak, b"mdia").unwrap();
    let minf = common::child(&mdia, b"minf").unwrap();
    let table = common::table(common::child(&minf, b"stbl").unwrap(), pool, 1).unwrap();
    let mut cursor = table.cursor();
    for expected in case.samples {
        let SampleStep::Sample(actual) = cursor.poll(&mut WorkBudget::new(1)).unwrap() else {
            panic!()
        };
        assert_eq!(actual.ordinal, expected.ordinal);
        assert_eq!(actual.dts_ticks, Some(expected.dts));
        assert_eq!(actual.cts_ticks, Some(expected.cts));
        assert_eq!(actual.range.start, expected.offset);
        assert_eq!(actual.description_index, expected.description);
    }
    let media = top
        .iter()
        .filter(|b| b.header().kind == *b"mdat")
        .map(|b| b.header().payload_range().unwrap())
        .collect();
    (table, media)
}

fn bytes(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(body.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    out.extend(kind);
    out.extend(body);
    out
}

pub fn edits(pool: &Budget, entries: &[(u64, i64, i16, i16)]) -> EditList {
    let mut body = vec![1, 0, 0, 0];
    body.extend(u32::try_from(entries.len()).unwrap().to_be_bytes());
    for (duration, time, rate, fraction) in entries {
        body.extend(duration.to_be_bytes());
        body.extend(time.to_be_bytes());
        body.extend(rate.to_be_bytes());
        body.extend(fraction.to_be_bytes());
    }
    let raw = BoxView::new(common::bytes(pool, &bytes(b"elst", &body)), 0).unwrap();
    let parsed = EditList::parse(raw.clone(), Limits::default()).unwrap();
    assert_eq!(parsed.raw().bytes().as_ref(), raw.bytes().as_ref());
    for (i, expected) in entries.iter().enumerate() {
        let actual = parsed.entry(u32::try_from(i).unwrap()).unwrap();
        assert_eq!(
            (
                actual.segment_duration,
                actual.media_time,
                actual.rate_integer,
                actual.rate_fraction
            ),
            *expected
        );
    }
    parsed
}

pub fn fragment(
    pool: &Budget,
    first: u64,
    base: u64,
    description: u32,
    origin: Option<u64>,
    flags: &[u32],
) -> FragmentIndex {
    fragment_runs(pool, first, base, description, origin, &[flags])
}

pub fn fragment_runs(
    pool: &Budget,
    first: u64,
    base: u64,
    description: u32,
    origin: Option<u64>,
    runs: &[&[u32]],
) -> FragmentIndex {
    let mut header = vec![0, 0, 0, 0x3b];
    header.extend(7u32.to_be_bytes());
    header.extend(base.to_be_bytes());
    header.extend(description.to_be_bytes());
    header.extend(10u32.to_be_bytes());
    header.extend(2u32.to_be_bytes());
    header.extend(0x10000u32.to_be_bytes());
    let mut body = bytes(b"tfhd", &header);
    if let Some(time) = origin {
        let mut b = vec![1, 0, 0, 0];
        b.extend(time.to_be_bytes());
        body.extend(bytes(b"tfdt", &b));
    }
    for &flags in runs {
        let mut run = vec![0, 0, 4, 0];
        run.extend(u32::try_from(flags.len()).unwrap().to_be_bytes());
        for flag in flags {
            run.extend(flag.to_be_bytes());
        }
        body.extend(bytes(b"trun", &run));
    }
    let raw = BoxView::new(
        common::bytes(pool, &bytes(b"traf", &body)),
        512 + first * 128,
    )
    .unwrap();
    let mut builder = FragmentBuilder::new(
        raw,
        Trex {
            track_id: 7,
            description_index: 1,
            duration: 0,
            size: 0,
            flags: 0,
        },
        FragmentContext {
            moof_start: 512,
            implicit_base: ImplicitBase::FirstTraf,
            decode_origin: None,
            first_ordinal: first,
            description_count: 2,
        },
        pool.clone(),
        Limits::default(),
    )
    .unwrap();
    for _ in 0..128 {
        if builder.poll(&mut WorkBudget::new(1)).unwrap() == BuildStatus::Ready {
            return builder.finish().unwrap();
        }
    }
    panic!("independently authored fragment fixture stalled");
}
