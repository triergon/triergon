mod common;
use common::{child, fixtures::CASES};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BoxView, BuildStatus, FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase, Limits,
    Sample, SampleStep, SampleTable, SourceSeekLimits, SourceSeekPlan, SourceSeekPlanner,
    SourceSeekRequest, SourceTrack, Trex,
};

fn regular(pool: &Budget, name: &str) -> (SampleTable, Vec<Range<u64>>) {
    let case = CASES.iter().find(|c| c.name == name).unwrap();
    let roots = common::roots(pool, case.bytes).unwrap();
    let moov = roots.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let trak = child(moov, b"trak").unwrap();
    let mdia = child(&trak, b"mdia").unwrap();
    let stbl = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
    let table = common::table(stbl, pool, 1).unwrap();
    let mut cursor = table.cursor();
    for expected in case.samples {
        let SampleStep::Sample(actual) = cursor.poll(&mut WorkBudget::new(1)).unwrap() else {
            panic!()
        };
        assert_eq!(actual.ordinal, expected.ordinal);
        assert_eq!(actual.range.start, expected.offset);
        assert_eq!(actual.dts_ticks, Some(expected.dts));
        assert_eq!(actual.cts_ticks, Some(expected.cts));
        assert_eq!(actual.description_index, expected.description);
        assert_eq!(actual.is_container_sync(), expected.sync);
    }
    let media = roots
        .iter()
        .filter(|b| b.header().kind == *b"mdat")
        .map(|b| b.header().payload_range().unwrap())
        .collect();
    (table, media)
}

fn plan<'a>(
    source: SourceTrack<'a>,
    request: SourceSeekRequest,
    media: &'a [Range<u64>],
    limits: SourceSeekLimits,
    quantum: u64,
) -> Result<SourceSeekPlan<'a>, Error> {
    let mut planner = SourceSeekPlanner::new(source, request, media, limits)?;
    let cancel = AtomicBool::new(false);
    for _ in 0..2048 {
        if planner.poll(&mut WorkBudget::new(quantum), &cancel)? == BuildStatus::Ready {
            return planner.finish();
        }
    }
    panic!("seek planning made no resumable progress within fixed bound");
}

fn samples(plan: &SourceSeekPlan<'_>, quantum: u64) -> Vec<Sample> {
    let mut cursor = plan.cursor();
    let mut output = Vec::new();
    let cancel = AtomicBool::new(false);
    for _ in 0..2048 {
        match cursor.poll(&mut WorkBudget::new(quantum), &cancel).unwrap() {
            SampleStep::Sample(sample) => output.push(sample),
            SampleStep::Yield => {}
            SampleStep::End => return output,
        }
    }
    panic!("seek sample iteration stalled");
}

#[test]
fn regular_seek_preserves_configuration_return_and_source_identity() {
    for quantum in [1, 4096] {
        let pool = Budget::new(1024 * 1024).unwrap();
        let (table, media) = regular(&pool, "r-description-change");
        let before = pool.used();
        let plan = plan(
            SourceTrack::new(1, Some(&table), &[]).unwrap(),
            SourceSeekRequest {
                target_ordinal: 6,
                preroll_samples: 1,
            },
            &media,
            SourceSeekLimits::default(),
            quantum,
        )
        .unwrap();
        let landing = plan.landing();
        assert_eq!((landing.track_id, landing.requested_ordinal), (1, 6));
        assert_eq!(
            (landing.sync_sample.ordinal, landing.decode_start.ordinal),
            (4, 3)
        );
        assert_eq!(
            (
                landing.available_preroll_samples,
                landing.missing_preroll_samples
            ),
            (1, 0)
        );
        assert_eq!(landing.validated_samples, 4);
        let actual = samples(&plan, quantum);
        assert_eq!(
            actual.iter().map(|s| s.ordinal).collect::<Vec<_>>(),
            [3, 4, 5, 6]
        );
        assert_eq!(
            actual
                .iter()
                .map(|s| s.description_index)
                .collect::<Vec<_>>(),
            [2, 2, 1, 1]
        );
        assert_eq!(
            actual.iter().map(|s| s.range.clone()).collect::<Vec<_>>(),
            [797..815, 815..833, 833..845, 845..857]
        );
        assert_eq!(
            actual.iter().map(|s| s.dts_ticks).collect::<Vec<_>>(),
            [Some(2000), Some(3000), Some(4000), Some(5000)]
        );
        assert!(!actual[0].is_container_sync());
        assert_eq!(
            pool.used(),
            before,
            "seek metadata must borrow existing charged indexes"
        );
    }
}

#[test]
fn no_sync_is_explicit_and_clipped_preroll_reports_missing_samples() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let (empty_sync, media) = regular(&pool, "r-stss-empty");
    let no_sync = plan(
        SourceTrack::new(1, Some(&empty_sync), &[]).unwrap(),
        SourceSeekRequest {
            target_ordinal: 3,
            preroll_samples: 0,
        },
        &media,
        SourceSeekLimits::default(),
        1,
    );
    assert!(matches!(
        no_sync,
        Err(Error::Unsupported("no preceding container sync sample"))
    ));
    let (table, media) = regular(&pool, "r-front-stco");
    let p = plan(
        SourceTrack::new(1, Some(&table), &[]).unwrap(),
        SourceSeekRequest {
            target_ordinal: 2,
            preroll_samples: 2,
        },
        &media,
        SourceSeekLimits::default(),
        1,
    )
    .unwrap();
    let landing = p.landing();
    assert_eq!(
        (landing.sync_sample.ordinal, landing.decode_start.ordinal),
        (1, 1)
    );
    assert_eq!(
        (
            landing.requested_preroll_samples,
            landing.available_preroll_samples,
            landing.missing_preroll_samples
        ),
        (2, 0, 2)
    );
}

fn box_bytes(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = u32::try_from(8 + payload.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(payload);
    b
}

fn fragment(
    pool: &Budget,
    first: u64,
    base: u64,
    description: u32,
    origin: Option<u64>,
    flags: &[u32],
) -> FragmentIndex {
    // Authored directly from field definitions: absolute base, trex overrides,
    // one trun with explicit flags, and optional unsigned64 tfdt. No media body.
    let mut header = vec![0, 0, 0, 0x3b];
    for bytes in [
        7u32.to_be_bytes().to_vec(),
        base.to_be_bytes().to_vec(),
        description.to_be_bytes().to_vec(),
        10u32.to_be_bytes().to_vec(),
        2u32.to_be_bytes().to_vec(),
        0x10000u32.to_be_bytes().to_vec(),
    ] {
        header.extend(bytes);
    }
    let mut body = box_bytes(b"tfhd", &header);
    if let Some(origin) = origin {
        let mut time = vec![1, 0, 0, 0];
        time.extend(origin.to_be_bytes());
        body.extend(box_bytes(b"tfdt", &time));
    }
    let mut run = vec![0, 0, 4, 0];
    run.extend(u32::try_from(flags.len()).unwrap().to_be_bytes());
    for flag in flags {
        run.extend(flag.to_be_bytes());
    }
    body.extend(box_bytes(b"trun", &run));
    let raw = box_bytes(b"traf", &body);
    let raw = BoxView::new(common::bytes(pool, &raw), 1024 + first * 128).unwrap();
    let context = FragmentContext {
        moof_start: 1024,
        implicit_base: ImplicitBase::FirstTraf,
        decode_origin: None,
        first_ordinal: first,
        description_count: 2,
    };
    let defaults = Trex {
        track_id: 7,
        description_index: 1,
        duration: 0,
        size: 0,
        flags: 0,
    };
    let mut builder =
        FragmentBuilder::new(raw, defaults, context, pool.clone(), Limits::default()).unwrap();
    for _ in 0..128 {
        if builder.poll(&mut WorkBudget::new(1)).unwrap() == BuildStatus::Ready {
            return builder.finish().unwrap();
        }
    }
    panic!("independent fragment fixture did not finish");
}

#[test]
fn fragmented_seek_prerolls_across_indexes_without_filling_unknown_source_clock() {
    for quantum in [1, 4096] {
        let pool = Budget::new(1024 * 1024).unwrap();
        let high = (1u64 << 32) + 256;
        let indexes = [
            fragment(&pool, 1, high, 1, Some(1u64 << 63), &[0, 0x10000, 0x10000]),
            fragment(&pool, 4, high - 64, 2, None, &[0x10000, 0, 0x10000]),
        ];
        // Confirm the canonical index facts before exercising the missing seek
        // surface. No predecessor clock or demux observation supplies origin.
        for (which, index) in indexes.iter().enumerate() {
            let mut cursor = index.cursor();
            for local in 0..3 {
                let SampleStep::Sample(sample) = cursor.poll(&mut WorkBudget::new(1)).unwrap()
                else {
                    panic!()
                };
                let expected = (which == 0).then_some((1i128 << 63) + local * 10);
                assert_eq!(sample.dts_ticks, expected);
                assert_eq!(sample.cts_ticks, expected);
                assert_eq!(
                    sample.ordinal,
                    u64::try_from(which * 3).unwrap() + u64::try_from(local).unwrap() + 1
                );
            }
        }
        let media_extent = high - 64..high + 6;
        let before = pool.used();
        let p = plan(
            SourceTrack::new(7, None, &indexes).unwrap(),
            SourceSeekRequest {
                target_ordinal: 6,
                preroll_samples: 2,
            },
            std::slice::from_ref(&media_extent),
            SourceSeekLimits::default(),
            quantum,
        )
        .unwrap();
        assert_eq!(
            (
                p.landing().sync_sample.ordinal,
                p.landing().decode_start.ordinal
            ),
            (5, 3)
        );
        let out = samples(&p, quantum);
        assert_eq!(
            out.iter().map(|s| s.ordinal).collect::<Vec<_>>(),
            [3, 4, 5, 6]
        );
        assert_eq!(
            out.iter().map(|s| s.description_index).collect::<Vec<_>>(),
            [1, 2, 2, 2]
        );
        assert_eq!(out[0].dts_ticks, Some((1i128 << 63) + 20));
        assert!(
            out[1..]
                .iter()
                .all(|s| s.dts_ticks.is_none() && s.cts_ticks.is_none())
        );
        assert!(
            out[0].range.start > out[1].range.start,
            "physical order must not replace decode order"
        );
        assert_eq!(out[3].fragment_flags, Some(0x10000));
        assert_eq!(pool.used(), before);
    }
}

#[test]
fn seek_checks_preroll_media_extents_before_publishing_a_landing() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let (table, _) = regular(&pool, "r-description-change");
    // Target6 and sync4 are present; the required preceding sample3 is outside.
    let media_extent = 815..857;
    let bad = plan(
        SourceTrack::new(1, Some(&table), &[]).unwrap(),
        SourceSeekRequest {
            target_ordinal: 6,
            preroll_samples: 1,
        },
        std::slice::from_ref(&media_extent),
        SourceSeekLimits::default(),
        1,
    );
    assert!(matches!(
        bad,
        Err(Error::Invalid("seek sample outside media extents"))
    ));
}

#[test]
fn seek_work_yields_cancel_is_sticky_and_range_limit_is_not_a_poll_limit() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let (table, media) = regular(&pool, "r-description-change");
    let source = SourceTrack::new(1, Some(&table), &[]).unwrap();
    let request = SourceSeekRequest {
        target_ordinal: 6,
        preroll_samples: 1,
    };
    let mut planner =
        SourceSeekPlanner::new(source, request, &media, SourceSeekLimits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..3 {
        assert_eq!(
            planner.poll(&mut WorkBudget::new(0), &cancel),
            Ok(BuildStatus::Yield)
        );
    }
    assert_eq!(
        planner.poll(&mut WorkBudget::new(1), &cancel),
        Ok(BuildStatus::Yield)
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        planner.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        planner.poll(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Cancelled)
    );
    let limited = SourceSeekLimits {
        max_range_samples: 3,
        ..SourceSeekLimits::default()
    };
    assert!(matches!(
        plan(source, request, &media, limited, 1),
        Err(Error::Limit {
            requested: 4,
            maximum: 3
        })
    ));
}

#[test]
fn zero_byte_sample_at_media_end_remains_a_separate_seek_target() {
    let pool = Budget::new(1024 * 1024).unwrap();
    // Complete one-sample stbl: a self-contained opaque sample entry, one
    // 10-tick stts run, one chunk, and an explicitly zero stsz entry at100.
    let entry = box_bytes(b"raw ", &[0, 0, 0, 0, 0, 0, 0, 1]);
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd.extend(entry);
    let mut body = box_bytes(b"stsd", &stsd);
    body.extend(box_bytes(
        b"stts",
        &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 10],
    ));
    body.extend(box_bytes(
        b"stsc",
        &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1],
    ));
    body.extend(box_bytes(
        b"stsz",
        &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0],
    ));
    body.extend(box_bytes(b"stco", &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 100]));
    let raw = BoxView::new(common::bytes(&pool, &box_bytes(b"stbl", &body)), 0).unwrap();
    let table = common::table(raw, &pool, 1).unwrap();
    assert_eq!(table.sample_count(), 1);
    let media_extent = 100..100;
    let p = plan(
        SourceTrack::new(1, Some(&table), &[]).unwrap(),
        SourceSeekRequest {
            target_ordinal: 1,
            preroll_samples: 0,
        },
        std::slice::from_ref(&media_extent),
        SourceSeekLimits::default(),
        1,
    )
    .unwrap();
    assert_eq!(p.landing().target.range, 100..100);
    assert_eq!(samples(&p, 1).last().unwrap().ordinal, 1);
}

#[test]
fn absent_sync_and_negative_cts_keep_their_original_source_meaning() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let (table, media) = regular(&pool, "r-stss-absent");
    let p = plan(
        SourceTrack::new(1, Some(&table), &[]).unwrap(),
        SourceSeekRequest {
            target_ordinal: 3,
            preroll_samples: 0,
        },
        &media,
        SourceSeekLimits::default(),
        1,
    )
    .unwrap();
    assert_eq!(p.landing().sync_sample.ordinal, 3);
    let (table, media) = regular(&pool, "r-negative-cts");
    let p = plan(
        SourceTrack::new(1, Some(&table), &[]).unwrap(),
        SourceSeekRequest {
            target_ordinal: 1,
            preroll_samples: 0,
        },
        &media,
        SourceSeekLimits::default(),
        1,
    )
    .unwrap();
    assert_eq!(p.landing().target.cts_ticks, Some(-1000));
    assert_eq!(
        p.landing().target.composition_offset.unwrap().raw_bits(),
        u32::from_be_bytes((-1000i32).to_be_bytes())
    );
}
