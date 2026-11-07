mod common;
use triergon_core::{Budget, Error, TimeBase, WorkBudget};
use triergon_isobmff::{
    BoxView, EditList, Limits, MovieHeader, Sample, SampleStep, SampleTable, TrackHeader,
    compat::mov::{
        edits::{EditCursor, EditLimits},
        packets::CursorStatus,
    },
};

struct Wanted {
    input: &'static [u8],
    samples: &'static [u64],
    dts: &'static [i64],
    durations: &'static [i64],
    flags: &'static [u32],
}
const PROBES: &[Wanted] = &[
    Wanted {
        input: include_bytes!("fixtures/edit-single-one.mov"),
        samples: &[1],
        dts: &[0],
        durations: &[1000],
        flags: &[1],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-single-two.mov"),
        samples: &[1, 2, 3, 4],
        dts: &[0, 1000, 2000, 3000],
        durations: &[1000; 4],
        flags: &[1, 0, 4, 5],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-single-trim.mov"),
        samples: &[1, 2, 3, 4],
        dts: &[-1000, 0, 1000, 2000],
        durations: &[1000; 4],
        flags: &[5, 0, 4, 5],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-single-late.mov"),
        samples: &[4],
        dts: &[0],
        durations: &[1000],
        flags: &[1],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-two-repeat-one.mov"),
        samples: &[1, 2, 3, 4, 1],
        dts: &[0, 1000, 2000, 3000, 2000],
        durations: &[1000; 5],
        flags: &[1, 0, 4, 5, 1],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-two-repeat-two.mov"),
        samples: &[1, 2, 3, 4, 1, 2, 3, 4],
        dts: &[0, 1000, 2000, 3000, 2000, 3000, 4000, 5000],
        durations: &[1000, 1000, 1000, 1000, 1000, 1000, 1000, 0],
        flags: &[1, 0, 4, 5, 1, 0, 4, 5],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-three-repeat-one.mov"),
        samples: &[1, 2, 3, 4, 4, 5, 6, 1],
        dts: &[0, 1000, 2000, 3000, 2000, 3000, 4000, 4000],
        durations: &[1000, 1000, 1000, 1000, 1000, 1000, 0, 1000],
        flags: &[1, 0, 4, 5, 1, 0, 4, 1],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-three-repeat-two.mov"),
        samples: &[1, 2, 3, 4, 4, 5, 6, 1, 2, 3, 4],
        dts: &[
            0, 1000, 2000, 3000, 2000, 3000, 4000, 4000, 5000, 6000, 7000,
        ],
        durations: &[1000, 1000, 1000, 1000, 1000, 1000, 0, 1000, 1000, 1000, 0],
        flags: &[1, 0, 4, 5, 1, 0, 4, 1, 0, 4, 5],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-three-repeat-three.mov"),
        samples: &[1, 2, 3, 4, 4, 5, 6, 1, 2, 3, 4],
        dts: &[
            0, 1000, 2000, 3000, 2000, 3000, 4000, 4000, 5000, 6000, 7000,
        ],
        durations: &[1000, 1000, 1000, 1000, 1000, 1000, 0, 1000, 1000, 1000, 0],
        flags: &[1, 0, 4, 5, 1, 0, 4, 1, 0, 0, 5],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-three-repeat-late.mov"),
        samples: &[1, 2, 3, 4, 4, 5, 6, 4],
        dts: &[0, 1000, 2000, 3000, 2000, 3000, 4000, 4000],
        durations: &[1000, 1000, 1000, 1000, 1000, 1000, 0, 1000],
        flags: &[1, 0, 4, 5, 1, 0, 4, 1],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-four-repeat-one.mov"),
        samples: &[1, 1, 1, 1],
        dts: &[0, 1000, 2000, 3000],
        durations: &[1000; 4],
        flags: &[1; 4],
    },
    Wanted {
        input: include_bytes!("fixtures/edit-leading-empty-one.mov"),
        samples: &[1],
        dts: &[500],
        durations: &[1000],
        flags: &[1],
    },
];

fn source(input: &[u8]) -> (SampleTable, EditList, TimeBase, TimeBase) {
    let budget = Budget::new(1024 * 1024).unwrap();
    let roots = common::roots(&budget, input).unwrap();
    let movie = roots.iter().find(|r| r.header().kind == *b"moov").unwrap();
    let movie_header = MovieHeader::parse(&common::child(movie, b"mvhd").unwrap()).unwrap();
    let track = common::child(movie, b"trak").unwrap();
    let media = common::child(&track, b"mdia").unwrap();
    let header = TrackHeader::parse(
        &common::child(&track, b"tkhd").unwrap(),
        &common::child(&media, b"mdhd").unwrap(),
        &common::child(&media, b"hdlr").unwrap(),
    )
    .unwrap();
    let table = common::table(
        common::child(&common::child(&media, b"minf").unwrap(), b"stbl").unwrap(),
        &budget,
        1,
    )
    .unwrap();
    let edits = EditList::parse(
        common::child(&common::child(&track, b"edts").unwrap(), b"elst").unwrap(),
        Limits::default(),
    )
    .unwrap();
    (
        table,
        edits,
        header.time_base().unwrap(),
        movie_header.time_base().unwrap(),
    )
}
fn unique_samples(table: &SampleTable) -> Vec<Sample> {
    let mut cursor = table.cursor();
    let mut result = Vec::new();
    loop {
        match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Sample(sample) => result.push(sample),
            SampleStep::Yield => {}
            SampleStep::End => return result,
        }
    }
}
fn check(wanted: &Wanted, quantum: u64) {
    let (table, edits, media, movie) = source(wanted.input);
    let originals = unique_samples(&table);
    let raw_edits = edits.raw().bytes().as_ref().to_vec();
    let mut cursor = EditCursor::new(&table, &edits, media, movie, EditLimits::default()).unwrap();
    let mut ordinal = 0usize;
    loop {
        match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
            CursorStatus::Yield => {}
            CursorStatus::End => break,
            CursorStatus::Ready => {
                let occurrence = cursor.take_occurrence().unwrap();
                assert_eq!(occurrence.sequence, ordinal as u64 + 1);
                assert_eq!(occurrence.source.ordinal, wanted.samples[ordinal]);
                assert_eq!(occurrence.demux.dts, Some(wanted.dts[ordinal]));
                assert_eq!(occurrence.demux.pts, Some(wanted.dts[ordinal]));
                assert_eq!(occurrence.demux.duration, wanted.durations[ordinal]);
                assert_eq!(occurrence.demux.flags, wanted.flags[ordinal]);
                assert_eq!(
                    occurrence.source,
                    originals[wanted.samples[ordinal] as usize - 1]
                );
                assert_eq!(occurrence.demux.source_range, occurrence.source.range);
                ordinal += 1;
            }
        }
    }
    assert_eq!(ordinal, wanted.samples.len());
    assert_eq!(unique_samples(&table), originals);
    assert_eq!(edits.raw().bytes().as_ref(), raw_edits);
}

#[test]
fn all_twelve_demux_probe_occurrences_preserve_unique_source_samples_at_both_work_schedules() {
    for wanted in PROBES {
        for quantum in [1, 4096] {
            check(wanted, quantum);
        }
    }
}

#[test]
fn original_four_edit_cases_are_separate_holdouts_including_exact_prime_clocks() {
    let definitions: &[Wanted] = &[
        Wanted {
            input: include_bytes!("fixtures/r-edits-empty-trim.mov"),
            samples: &[1, 2, 3, 4, 5, 6],
            dts: &[22976, 24000, 25024, 26048, 27072, 28096],
            durations: &[1024; 6],
            flags: &[5, 0, 0, 1, 0, 4],
        },
        Wanted {
            input: include_bytes!("fixtures/r-edits-repeat.mov"),
            samples: &[1, 2, 3, 4, 4, 5, 6, 1],
            dts: &[0, 1000, 2000, 3000, 2000, 3000, 4000, 4000],
            durations: &[1000, 1000, 1000, 1000, 1000, 1000, 0, 1000],
            flags: &[1, 0, 4, 5, 1, 0, 4, 1],
        },
        Wanted {
            input: include_bytes!("fixtures/r-elst-v1.mov"),
            samples: &[1, 2, 3, 4],
            dts: &[-1000, 0, 1000, 2000],
            durations: &[1000; 4],
            flags: &[5, 0, 4, 5],
        },
        Wanted {
            input: include_bytes!("fixtures/r-prime-clock-edit.mov"),
            samples: &[1, 2, 3, 4],
            dts: &[-1, 0, 333667, 333669],
            durations: &[1, 333667, 2, 333668],
            flags: &[5, 0, 4, 5],
        },
    ];
    for wanted in definitions {
        for quantum in [1, 4096] {
            check(wanted, quantum);
        }
    }
}

#[test]
fn ready_occurrence_waits_for_the_caller_without_advancing_or_spending_work() {
    let (table, edits, media, movie) = source(PROBES[1].input);
    let mut cursor = EditCursor::new(&table, &edits, media, movie, EditLimits::default()).unwrap();
    while cursor.poll(&mut WorkBudget::new(1)).unwrap() != CursorStatus::Ready {}
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(0)).unwrap(),
        CursorStatus::Ready
    );
    assert_eq!(cursor.take_occurrence().unwrap().sequence, 1);
    assert!(cursor.take_occurrence().is_err());
}

#[test]
fn occurrence_and_cumulative_source_scan_limits_are_explicit() {
    for limits in [
        EditLimits {
            max_occurrences: 1,
            max_scanned_samples: 1000,
        },
        EditLimits {
            max_occurrences: 1000,
            max_scanned_samples: 1,
        },
    ] {
        let (table, edits, media, movie) = source(PROBES[5].input);
        let mut cursor = EditCursor::new(&table, &edits, media, movie, limits).unwrap();
        loop {
            match cursor.poll(&mut WorkBudget::new(1)) {
                Ok(CursorStatus::Yield) => {}
                Ok(CursorStatus::Ready) => {
                    cursor.take_occurrence().unwrap();
                }
                Err(Error::Limit { .. }) => break,
                value => panic!("unexpected limit result: {value:?}"),
            }
        }
    }
}

fn authored_edits(count: u32, duration: u32, rate: i16) -> EditList {
    let budget = Budget::new(4096).unwrap();
    let mut raw = (16 + count * 12).to_be_bytes().to_vec();
    raw.extend_from_slice(b"elst");
    raw.extend_from_slice(&0u32.to_be_bytes());
    raw.extend_from_slice(&count.to_be_bytes());
    for _ in 0..count {
        raw.extend_from_slice(&duration.to_be_bytes());
        raw.extend_from_slice(&0i32.to_be_bytes());
        raw.extend_from_slice(&rate.to_be_bytes());
        raw.extend_from_slice(&0i16.to_be_bytes());
    }
    EditList::parse(
        BoxView::new(common::bytes(&budget, &raw), 0).unwrap(),
        Limits::default(),
    )
    .unwrap()
}

#[test]
fn non_unit_rates_stay_unsupported_and_required_fractional_demux_edits_preserve_source() {
    let (table, _, media, movie) = source(PROBES[0].input);
    let rate = authored_edits(1, 1000, 2);
    let mut cursor = EditCursor::new(&table, &rate, media, movie, EditLimits::default()).unwrap();
    assert!(matches!(
        cursor.poll(&mut WorkBudget::new(4096)),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(rate.entry(0).unwrap().rate_integer, 2);
    let fractional = authored_edits(7, 1, 1);
    let original = unique_samples(&table);
    let mut cursor = EditCursor::new(
        &table,
        &fractional,
        media,
        TimeBase::new(1, 3).unwrap(),
        EditLimits::default(),
    )
    .unwrap();
    let mut count = 0usize;
    let expected = [0, 333, 666, 999, 1332, 1665, 1998];
    loop {
        match cursor.poll(&mut WorkBudget::new(1)) {
            Ok(CursorStatus::Ready) => {
                let occurrence = cursor.take_occurrence().unwrap();
                assert_eq!(occurrence.source, original[0]);
                assert_eq!(occurrence.demux.dts, Some(expected[count]));
                assert_eq!(occurrence.demux.pts, Some(expected[count]));
                assert_eq!(
                    occurrence.demux.duration,
                    if count < 6 { 1000 } else { 333 }
                );
                count += 1;
            }
            Ok(CursorStatus::Yield) => {}
            Ok(CursorStatus::End) => break,
            result => panic!("unreviewed terminal result: {result:?}"),
        }
    }
    assert_eq!(count, 7);
    assert_eq!(unique_samples(&table), original);
}
