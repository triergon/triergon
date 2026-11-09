mod common;
#[path = "support/movie.rs"]
mod fixture;
#[path = "support/recovery.rs"]
mod wire;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, Time, TimeBase, WorkBudget};
use triergon_isobmff::{
    BuildStatus, CompositionTarget, EditList, MovieEditLocator, MoviePoint, MovieSeekClocks,
    MovieSeekLimits, MovieSeekResult, MovieSeekStatus, MovieSourceSeekContext,
    MovieSourceSeekPlanner, RecoveryDescriptions, RecoveryKind, RecoveryLimits,
    RecoveryMappingOrigin, RecoveryMappings, RecoveryScope, RecoverySyntax, SampleStep,
    SourceCompositionLookup, SourceTrack,
};

#[test]
fn complete_fragment_edit_offsets_the_packet_and_resume_clocks() {
    use triergon_isobmff::compat::mov::packets::{
        CursorStatus, DemuxProfile, DemuxTrack, PacketCursor, PacketLimits, ResumePoint,
    };
    use triergon_isobmff::compat::mov::seek::{SeekJob, SeekLimits};
    let pool = Budget::new(65536).unwrap();
    let boxes = common::roots(
        &pool,
        include_bytes!("fixtures/native-open-v6/tfdt-control-08.mp4"),
    )
    .unwrap();
    let movie = boxes
        .iter()
        .find(|raw| raw.header().kind == *b"moov")
        .unwrap();
    let track = common::child(movie, b"trak").unwrap();
    let media = common::child(&track, b"mdia").unwrap();
    let info = common::child(&media, b"minf").unwrap();
    let table = common::table(common::child(&info, b"stbl").unwrap(), &pool, 1).unwrap();
    assert_eq!(table.sample_count(), 0);
    let fragments = [fixture::fragment(
        &pool,
        1,
        1000,
        1,
        Some(0),
        &[0, 0x10000, 0],
    )];
    let edits = fixture::edits(&pool, &[(0, 4, 1, 0)]);
    let base = TimeBase::new(1, 1000).unwrap();
    let tracks = [DemuxTrack {
        track_id: 7,
        table: &table,
        fragments: &fragments,
        media_base: base,
        edits: Some((&edits, base)),
    }];
    let extent = 1000..1006;
    let cancel = AtomicBool::new(false);
    for start in [0, 1] {
        let mut cursor = PacketCursor::new_complete_source_at(
            &pool,
            &tracks,
            std::slice::from_ref(&extent),
            PacketLimits::default(),
            DemuxProfile::NoParseNoFillin,
            &[ResumePoint {
                track_id: 7,
                occurrence: start,
            }],
        )
        .unwrap();
        cursor.enable_range_seek_checkpoints();
        let mut rows = Vec::new();
        for _ in 0..512 {
            match cursor.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
                CursorStatus::Yield => {}
                CursorStatus::Ready => {
                    let packet = cursor.take_occurrence().unwrap();
                    rows.push((packet.source.dts_ticks, packet.demux.pts, packet.demux.dts));
                    assert_eq!(packet.demux.duration, 10);
                    assert_eq!(packet.edit.unwrap().edit_ordinal, 1);
                }
                CursorStatus::End => break,
            }
        }
        let expected = [
            (Some(0), Some(-4), Some(-4)),
            (Some(10), Some(6), Some(6)),
            (Some(20), Some(16), Some(16)),
        ];
        assert_eq!(rows, expected[usize::try_from(start).unwrap()..]);
        if start == 0 {
            cursor.rewind_at_end().unwrap();
            for (target, selected) in [(19, -4), (20, 16)] {
                let mut checked = cursor
                    .prepare_checked_range_seek(7, target, SeekLimits::default())
                    .unwrap()
                    .expect("sync-starting fragment uses checked restarts");
                assert!((0..512).any(|_| {
                    checked.poll(&mut WorkBudget::new(1), &cancel).unwrap() == BuildStatus::Ready
                }));
                let (mut replay, landing) = checked.finish().unwrap();
                assert_eq!(landing.requested_timestamp, target);
                assert_eq!(landing.selected.timestamp, selected);
                assert!((0..512).any(|_| {
                    replay.poll(&mut WorkBudget::new(1), &cancel).unwrap() == CursorStatus::Ready
                }));
                assert_eq!(replay.take_occurrence().unwrap().demux.dts, Some(selected));
            }
        }
    }
    for (target, selected) in [(19, -4), (20, 16)] {
        let mut seek = SeekJob::new_complete_source(
            &pool,
            tracks[0],
            std::slice::from_ref(&extent),
            target,
            SeekLimits::default(),
            DemuxProfile::NoParseNoFillin,
        )
        .unwrap();
        assert!((0..512).any(|_| {
            seek.poll(&mut WorkBudget::new(1), &cancel).unwrap() == BuildStatus::Ready
        }));
        let seek = seek.finish().unwrap();
        assert_eq!(seek.landing().requested_timestamp, target);
        assert_eq!(seek.landing().selected.timestamp, selected);
    }
}

fn time(n: i128, d: u128) -> Time {
    Time::new(n, d).unwrap()
}
fn clocks(movie: u64, media: u64, duration: Option<Time>) -> MovieSeekClocks {
    MovieSeekClocks {
        movie_base: TimeBase::new(1, movie).unwrap(),
        media_base: TimeBase::new(1, media).unwrap(),
        media_duration: duration,
    }
}
fn point(
    edits: Option<&EditList>,
    clocks: MovieSeekClocks,
    at: Time,
    quantum: u64,
) -> Result<MoviePoint, Error> {
    let mut locator = MovieEditLocator::new(edits, clocks, at, 4096)?;
    for _ in 0..128 {
        if locator.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))?
            == BuildStatus::Ready
        {
            return locator.finish();
        }
    }
    panic!("exact edit lookup stalled within fixed poll bound");
}
fn composition(
    source: SourceTrack<'_>,
    at: Time,
    base: TimeBase,
    maximum: u64,
) -> Result<CompositionTarget, Error> {
    let mut lookup = SourceCompositionLookup::new(source, at, base, maximum)?;
    for _ in 0..128 {
        if lookup.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))? == BuildStatus::Ready {
            return lookup.finish();
        }
    }
    panic!("composition lookup stalled within fixed poll bound");
}
fn target(
    context: MovieSourceSeekContext<'_>,
    at: Time,
    quantum: u64,
) -> Result<MovieSeekResult<'_>, Error> {
    let mut planner = MovieSourceSeekPlanner::new(context, at, MovieSeekLimits::default())?;
    for _ in 0..2048 {
        if planner.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))?
            == BuildStatus::Ready
        {
            return planner.finish();
        }
    }
    panic!("movie/recovery seek composition stalled within fixed poll bound");
}

#[test]
fn movie_point_mapping_is_exact_and_repeated_edit_identity_stays_separate() {
    let pool = Budget::new(65536).unwrap();
    let edits = fixture::edits(&pool, &[(1, -1, 1, 0), (2, 1000, 1, 0), (2, 1000, 1, 0)]);
    for quantum in [1, 4096] {
        let a = point(Some(&edits), clocks(3, 1000, None), time(1, 2), quantum).unwrap();
        let b = point(Some(&edits), clocks(3, 1000, None), time(7, 6), quantum).unwrap();
        let (MoviePoint::Media(a), MoviePoint::Media(b)) = (a, b) else {
            panic!()
        };
        assert_eq!((a.edit_ordinal, b.edit_ordinal), (Some(2), Some(3)));
        assert_eq!((a.source_time, b.source_time), (time(7, 6), time(7, 6)));
        assert_eq!((a.movie_start, b.movie_start), (time(1, 3), time(1, 1)));
        assert_eq!(
            (a.movie_end, b.movie_end),
            (Some(time(1, 1)), Some(time(5, 3)))
        );
        assert_eq!(a.edit_entry, Some(edits.entry(1).unwrap()));
        assert_eq!(b.edit_entry, Some(edits.entry(2).unwrap()));
        assert_eq!(
            (a.requested_movie_time, b.requested_movie_time),
            (time(1, 2), time(7, 6))
        );
    }
}

#[test]
fn empty_edits_boundaries_outside_and_identity_timeline_are_explicit() {
    let pool = Budget::new(65536).unwrap();
    let edits = fixture::edits(&pool, &[(1, -1, 1, 0), (2, 1000, 1, 0), (2, 1000, 1, 0)]);
    let MoviePoint::Empty(gap) = point(Some(&edits), clocks(3, 1000, None), time(0, 1), 1).unwrap()
    else {
        panic!()
    };
    assert_eq!(gap.edit_ordinal, 1);
    assert_eq!(gap.movie_range, time(0, 1)..time(1, 3));
    assert_eq!(gap.entry.media_time, -1);
    for (at, ordinal) in [(time(1, 3), 2), (time(1, 1), 3)] {
        let MoviePoint::Media(p) = point(Some(&edits), clocks(3, 1000, None), at, 1).unwrap()
        else {
            panic!()
        };
        assert_eq!(p.edit_ordinal, Some(ordinal));
        assert_eq!(p.source_time, time(1, 1));
    }
    for at in [time(-1, 1000), time(5, 3), time(2, 1)] {
        assert!(
            matches!(point(Some(&edits),clocks(3,1000,None),at,1).unwrap(),MoviePoint::Outside {requested_movie_time} if requested_movie_time==at)
        );
    }
    let MoviePoint::Media(identity) =
        point(None, clocks(1000, 1000, Some(time(6, 1))), time(11, 2), 1).unwrap()
    else {
        panic!()
    };
    assert_eq!(identity.edit_ordinal, None);
    assert_eq!(identity.edit_entry, None);
    assert_eq!(identity.source_time, time(11, 2));
    assert!(matches!(
        point(None, clocks(1000, 1000, Some(time(6, 1))), time(6, 1), 1).unwrap(),
        MoviePoint::Outside { .. }
    ));
}

#[test]
fn composition_predecessor_uses_source_cts_without_sorting_or_clamping_raw_offsets() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let (table, _) = fixture::regular(&pool, "r-ctts-v1");
    let base = TimeBase::new(1, 1000).unwrap();
    assert_eq!(
        composition(
            SourceTrack::new(1, Some(&table), &[]).unwrap(),
            time(-1, 1000),
            base,
            6
        )
        .unwrap(),
        CompositionTarget::BeforeFirst
    );
    let CompositionTarget::Target(sample) = composition(
        SourceTrack::new(1, Some(&table), &[]).unwrap(),
        time(3, 2),
        base,
        6,
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(sample.ordinal, 3);
    assert_eq!(sample.dts_ticks, Some(2000));
    assert_eq!(sample.cts_ticks, Some(1000));
    assert_eq!(
        sample.composition_offset.unwrap().raw_bits(),
        u32::from_be_bytes((-1000i32).to_be_bytes())
    );
    let (table, _) = fixture::regular(&pool, "r-negative-cts");
    let CompositionTarget::Target(sample) = composition(
        SourceTrack::new(1, Some(&table), &[]).unwrap(),
        time(0, 1),
        base,
        6,
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(sample.ordinal, 1);
    assert_eq!(sample.cts_ticks, Some(-1000));
}

#[test]
fn duplicate_and_unknown_source_clocks_never_produce_a_fabricated_target() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let base = TimeBase::new(1, 1000).unwrap();
    let duplicate = [
        fixture::fragment(&pool, 1, 4096, 1, Some(0), &[0]),
        fixture::fragment(&pool, 2, 8192, 2, Some(0), &[0]),
    ];
    for (index, fragment) in duplicate.iter().enumerate() {
        let SampleStep::Sample(sample) = fragment.cursor().poll(&mut WorkBudget::new(1)).unwrap()
        else {
            panic!()
        };
        assert_eq!(sample.ordinal, u64::try_from(index + 1).unwrap());
        assert_eq!((sample.dts_ticks, sample.cts_ticks), (Some(0), Some(0)));
    }
    let result = composition(
        SourceTrack::new(7, None, &duplicate).unwrap(),
        time(0, 1),
        base,
        2,
    )
    .unwrap();
    assert_eq!(
        result,
        CompositionTarget::Ambiguous {
            first_ordinal: 1,
            second_ordinal: 2,
            source_time: time(0, 1)
        }
    );
    let unknown = [fixture::fragment(&pool, 1, 4096, 1, None, &[0])];
    let mut source = unknown[0].cursor();
    let SampleStep::Sample(sample) = source.poll(&mut WorkBudget::new(1)).unwrap() else {
        panic!()
    };
    assert_eq!((sample.dts_ticks, sample.cts_ticks), (None, None));
    assert_eq!(
        composition(
            SourceTrack::new(7, None, &unknown).unwrap(),
            time(0, 1),
            base,
            1
        )
        .unwrap(),
        CompositionTarget::UnknownSourceTime { ordinal: 1 }
    );
    let extent = 4096..4098;
    let result = target(
        MovieSourceSeekContext {
            source: SourceTrack::new(7, None, &unknown).unwrap(),
            edits: None,
            clocks: clocks(1000, 1000, Some(time(1, 100))),
            media_extents: std::slice::from_ref(&extent),
            recovery_kind: RecoveryKind::Roll,
            recovery_scopes: &[],
        },
        time(0, 1),
        1,
    )
    .unwrap();
    assert_eq!(
        result.status,
        MovieSeekStatus::UnknownSourceTime { ordinal: 1 }
    );
    assert!(result.plan.is_none());
}

#[test]
fn repeated_movie_edit_and_roll_mapping_feed_real_source_seek_with_config_return() {
    for quantum in [1, 4096] {
        let pool = Budget::new(1024 * 1024).unwrap();
        let (table, media) = fixture::regular(&pool, "r-description-change");
        let edits = fixture::edits(&pool, &[(3000, 2000, 1, 0), (3000, 2000, 1, 0)]);
        let descriptions = RecoveryDescriptions::parse(
            wire::descriptions(&pool, RecoveryKind::Roll, 1, 2, 0, &[-1], 0),
            RecoverySyntax::Versions0And1,
            RecoveryLimits::default(),
        )
        .unwrap();
        let mappings = RecoveryMappings::parse(
            wire::mappings(&pool, RecoveryKind::Roll, 0, 0, &[(6, 1)]),
            RecoveryLimits::default(),
        )
        .unwrap();
        let scopes = [RecoveryScope {
            kind: RecoveryKind::Roll,
            first_ordinal: 1,
            sample_count: 6,
            fragment_ordinal: None,
            track_descriptions: Some(&descriptions),
            local_descriptions: None,
            mappings: Some(&mappings),
        }];
        let context = MovieSourceSeekContext {
            source: SourceTrack::new(1, Some(&table), &[]).unwrap(),
            edits: Some(&edits),
            clocks: clocks(1000, 1000, Some(time(6, 1))),
            media_extents: &media,
            recovery_kind: RecoveryKind::Roll,
            recovery_scopes: &scopes,
        };
        let before = pool.used();
        let result = target(context, time(11, 2), quantum).unwrap();
        assert_eq!(result.status, MovieSeekStatus::Planned);
        let point = result.point.as_ref().unwrap();
        assert_eq!(point.edit_ordinal, Some(2));
        assert_eq!(point.source_time, time(9, 2));
        let recovery = result.recovery.as_ref().unwrap();
        assert_eq!(recovery.raw_mapping_index, Some(1));
        assert_eq!(recovery.preceding_samples(), Ok(1));
        let plan = result.plan.as_ref().unwrap();
        let landing = plan.landing();
        assert_eq!(
            (
                landing.target.ordinal,
                landing.sync_sample.ordinal,
                landing.decode_start.ordinal
            ),
            (5, 4, 3)
        );
        let mut cursor = plan.cursor();
        let mut out = Vec::new();
        for _ in 0..2048 {
            match cursor
                .poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
                .unwrap()
            {
                SampleStep::Sample(s) => out.push(s),
                SampleStep::Yield => {}
                SampleStep::End => break,
            }
        }
        assert_eq!(out.iter().map(|s| s.ordinal).collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!(
            out.iter().map(|s| s.description_index).collect::<Vec<_>>(),
            [2, 2, 1]
        );
        assert_eq!(
            out.iter().map(|s| s.range.clone()).collect::<Vec<_>>(),
            [797..815, 815..833, 833..845]
        );
        assert_eq!(
            pool.used(),
            before,
            "composed targeting may not allocate another index"
        );
    }
}

#[test]
fn inherited_fragment_prol_drives_cross_fragment_preroll_in_original_decode_order() {
    for quantum in [1, 4096] {
        let pool = Budget::new(1024 * 1024).unwrap();
        let high = (1u64 << 32) + 256;
        let indexes = [
            fixture::fragment(&pool, 1, high, 1, Some(0), &[0, 0x10000, 0x10000]),
            fixture::fragment(&pool, 4, high - 64, 2, Some(30), &[0x10000, 0, 0x10000]),
        ];
        let description = RecoveryDescriptions::parse(
            wire::descriptions(&pool, RecoveryKind::Prol, 2, 2, 1, &[-2], 0),
            RecoverySyntax::Mpeg2026Draft,
            RecoveryLimits::default(),
        )
        .unwrap();
        let scopes = [
            RecoveryScope {
                kind: RecoveryKind::Prol,
                first_ordinal: 1,
                sample_count: 3,
                fragment_ordinal: Some(1),
                track_descriptions: Some(&description),
                local_descriptions: None,
                mappings: None,
            },
            RecoveryScope {
                kind: RecoveryKind::Prol,
                first_ordinal: 4,
                sample_count: 3,
                fragment_ordinal: Some(2),
                track_descriptions: Some(&description),
                local_descriptions: None,
                mappings: None,
            },
        ];
        let extent = high - 64..high + 6;
        let result = target(
            MovieSourceSeekContext {
                source: SourceTrack::new(7, None, &indexes).unwrap(),
                edits: None,
                clocks: clocks(1000, 1000, Some(time(3, 50))),
                media_extents: std::slice::from_ref(&extent),
                recovery_kind: RecoveryKind::Prol,
                recovery_scopes: &scopes,
            },
            time(11, 200),
            quantum,
        )
        .unwrap();
        assert_eq!(result.status, MovieSeekStatus::Planned);
        assert_eq!(
            result.recovery.as_ref().unwrap().mapping_origin,
            RecoveryMappingOrigin::InheritedTrackDefault
        );
        let plan = result.plan.as_ref().unwrap();
        assert_eq!(
            (
                plan.landing().target.ordinal,
                plan.landing().sync_sample.ordinal,
                plan.landing().decode_start.ordinal
            ),
            (6, 5, 3)
        );
        let mut cursor = plan.cursor();
        let mut out = Vec::new();
        for _ in 0..2048 {
            match cursor
                .poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
                .unwrap()
            {
                SampleStep::Sample(s) => out.push(s),
                SampleStep::Yield => {}
                SampleStep::End => break,
            }
        }
        assert_eq!(
            out.iter().map(|s| s.ordinal).collect::<Vec<_>>(),
            [3, 4, 5, 6]
        );
        assert_eq!(
            out.iter().map(|s| s.dts_ticks).collect::<Vec<_>>(),
            [Some(20), Some(30), Some(40), Some(50)]
        );
        assert_eq!(
            out.iter().map(|s| s.description_index).collect::<Vec<_>>(),
            [1, 2, 2, 2]
        );
        assert!(out[0].range.start > out[1].range.start);
    }
}

#[test]
fn unresolved_rates_zero_duration_and_work_limits_are_not_approximated() {
    let pool = Budget::new(1024 * 1024).unwrap();
    let clock = clocks(1000, 1000, None);
    let valid = fixture::edits(&pool, &[(1000, 0, 1, 0), (1000, 1000, 1, 0)]);
    let mut lookup = MovieEditLocator::new(Some(&valid), clock, time(1, 2), 4096).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        lookup.poll(&mut WorkBudget::new(0), &cancel),
        Ok(BuildStatus::Yield)
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        lookup.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        lookup.poll(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Cancelled)
    );
    assert!(matches!(
        MovieEditLocator::new(Some(&valid), clock, time(1, 2), 1),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
    for rate in [(0, 0), (-1, 0), (1, 16384)] {
        let edits = fixture::edits(&pool, &[(1000, 0, rate.0, rate.1)]);
        assert!(matches!(
            point(Some(&edits), clock, time(1, 2), 1),
            Err(Error::Unsupported("non-unit movie seek rate"))
        ));
    }
    let zero = fixture::edits(&pool, &[(0, 0, 1, 0)]);
    assert!(matches!(
        point(Some(&zero), clock, time(0, 1), 1),
        Err(Error::Unsupported(
            "zero-duration edit timeline remains unresolved"
        ))
    ));
    let (table, _) = fixture::regular(&pool, "r-front-stco");
    assert!(matches!(
        composition(
            SourceTrack::new(1, Some(&table), &[]).unwrap(),
            time(0, 1),
            clock.media_base,
            1
        ),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
}
