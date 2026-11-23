//! Additional demux global-seek boundaries.
mod common;
#[path = "support/demux.rs"]
mod support;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, TimeBase, WorkBudget};
use triergon_isobmff::{
    BuildStatus, CursorStatus,
    compat::mov::{
        packets::DemuxProfile,
        seek::{
            GlobalSeekJob, GlobalSeekPlan, IndexCursor, IndexEntry, IndexLimits, SeekJob,
            SeekLimits,
        },
    },
};
fn entries(pool: &Budget, movie: &support::Movie) -> Vec<IndexEntry> {
    let tracks = movie.demux();
    let mut c = IndexCursor::new_complete_source(
        pool,
        tracks[0],
        &movie.media,
        IndexLimits::default(),
        DemuxProfile::NoParseNoFillin,
    )
    .unwrap();
    let mut out = Vec::new();
    for _ in 0..4096 {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::End => return out,
            CursorStatus::Ready => out.push(c.take_entry().unwrap()),
        }
    }
    panic!("index progress bound");
}
fn global<'a>(pool: &Budget, movie: &'a support::Movie, id: u32, ticks: i64) -> GlobalSeekPlan<'a> {
    let tracks = movie.demux();
    let mut job = GlobalSeekJob::new_complete_source(
        pool,
        &tracks,
        &movie.media,
        id,
        ticks,
        SeekLimits::default(),
        DemuxProfile::NoParseNoFillin,
    )
    .unwrap();
    assert_eq!(
        job.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        BuildStatus::Yield
    );
    for _ in 0..8192 {
        if job
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == BuildStatus::Ready
        {
            return job.finish().unwrap();
        }
    }
    panic!("global plan progress bound");
}
fn next4(plan: &GlobalSeekPlan<'_>) -> Vec<(u32, u64, u64, i64, u32)> {
    let mut c = plan.cursor().unwrap();
    let mut rows = Vec::new();
    for _ in 0..8192 {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::End => panic!("native replay ended before expected4"),
            CursorStatus::Ready => {
                let row = c.take_occurrence().unwrap();
                rows.push((
                    row.track_id,
                    row.source.ordinal,
                    row.source.range.start,
                    row.demux.dts_field.signed_ticks(),
                    row.demux.flags,
                ));
                if rows.len() == 4 {
                    return rows;
                }
            }
        }
    }
    panic!("global replay progress bound");
}
#[test]
fn demux_index_signed_discard_bits_and_trun_flag_presence_are_preserved() {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let movie = support::parse(&pool, include_bytes!("fixtures/f-first-flags.mp4"));
    let raw: Vec<_> = movie
        .fragments
        .iter()
        .map(|f| f.raw().bytes().as_ref().to_vec())
        .collect();
    let rows = entries(&pool, &movie);
    assert_eq!(
        rows.iter().map(|r| r.min_distance).collect::<Vec<_>>(),
        [0, 1, 2, 0, 1, 2]
    );
    for (f, before) in movie.fragments.iter().zip(raw) {
        assert_eq!(f.raw().bytes().as_ref(), before);
    }
    let movie = support::parse(&pool, include_bytes!("fixtures/r-edits-empty-trim.mov"));
    let rows = entries(&pool, &movie);
    assert_eq!(
        rows.iter().map(|r| r.flags).collect::<Vec<_>>(),
        [-1, 0, 0, 1, 0, -2]
    );
    assert_eq!(
        rows.iter().map(|r| r.raw_flag_bits).collect::<Vec<_>>(),
        [3, 0, 0, 1, 0, 2]
    );
    assert_eq!(
        rows.iter().map(|r| r.timestamp).collect::<Vec<_>>(),
        [22976, 24000, 25024, 26048, 27072, 28096]
    );
    assert_eq!(rows[0].occurrence.demux.flags, 5);
    assert_eq!(rows[5].occurrence.demux.flags, 4);
}
#[test]
fn repeated_edit_index_keeps_demux_rollback_and_unique_source_identity() {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let movie = support::parse(&pool, include_bytes!("fixtures/r-edits-repeat.mov"));
    let original = movie.tracks[0].table.raw().bytes().as_ref().to_vec();
    let rows = entries(&pool, &movie);
    assert_eq!(rows.len(), 8);
    assert_eq!(
        rows.iter().map(|r| r.timestamp).collect::<Vec<_>>(),
        [0, 1000, 2000, 3000, 2000, 3000, 4000, 4000]
    );
    assert_eq!(
        rows.iter().map(|r| r.flags).collect::<Vec<_>>(),
        [1, 0, -2, -1, 1, 0, -2, 1]
    );
    assert_eq!(
        rows.iter()
            .map(|r| r.occurrence.source.ordinal)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 4, 5, 6, 1]
    );
    let plan = global(&pool, &movie, 1, 1500);
    assert_eq!(
        next4(&plan).iter().map(|r| r.3).collect::<Vec<_>>(),
        [0, 1000, 2000, 3000]
    );
    assert_eq!(movie.tracks[0].table.raw().bytes().as_ref(), original);
}
#[test]
fn global_regular_seek_aligns_to_selected_demux_clock_not_request_or_file_position() {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let movie = support::parse(&pool, include_bytes!("fixtures/r-multitrack.mov"));
    let baseline = pool.used();
    {
        let plan = global(&pool, &movie, 1, 1500);
        assert_eq!(
            plan.anchors()
                .iter()
                .map(|p| (p.track_id, p.occurrence))
                .collect::<Vec<_>>(),
            [(1, 0), (2, 0)]
        );
        assert_eq!(
            next4(&plan).iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(),
            [(1, 1), (1, 2), (2, 1), (2, 2)]
        );
    }
    assert_eq!(pool.used(), baseline);
    {
        let plan = global(&pool, &movie, 2, 1500);
        assert_eq!(plan.landing().selected.timestamp, 1000);
        assert_eq!(plan.landing().selected.position, 1182);
        assert_eq!(
            plan.anchors()
                .iter()
                .map(|p| (p.track_id, p.occurrence))
                .collect::<Vec<_>>(),
            [(1, 0), (2, 1)]
        );
        let rows = next4(&plan);
        assert_eq!(
            rows.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(),
            [(1, 1), (1, 2), (2, 2), (1, 3)]
        );
        assert!(rows[0].2 < 1182);
    }
    assert_eq!(pool.used(), baseline);
}
#[test]
fn global_fragment_seek_reconstructs_prefix_state_and_keeps_actual_global_order() {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let movie = support::parse(&pool, include_bytes!("fixtures/f-multitrack.mp4"));
    let plan = global(&pool, &movie, 2, 3500);
    assert_eq!(
        plan.anchors()
            .iter()
            .map(|p| (p.track_id, p.occurrence))
            .collect::<Vec<_>>(),
        [(1, 3), (2, 3)]
    );
    assert_eq!(
        next4(&plan),
        [
            (1, 4, 1647, 3000, 1),
            (1, 5, 1659, 4000, 0),
            (2, 4, 1683, 3000, 1),
            (1, 6, 1671, 5000, 0)
        ]
    );
}

#[test]
fn fragmented_multitrack_seek_retains_exact_source_ranges_and_payloads() {
    // The authored fragment runs place three video samples followed by three
    // text samples in each mdat. The second fragment starts at 3000 ticks.
    let case = common::fixtures::CASES
        .iter()
        .find(|case| case.name == "f-multitrack")
        .unwrap();
    let expected = [
        (1, 4, 1647..1659, 3000),
        (1, 5, 1659..1671, 4000),
        (2, 4, 1683..1688, 3000),
        (1, 6, 1671..1683, 5000),
        (2, 5, 1688..1694, 4000),
        (2, 6, 1694..1700, 5000),
    ];
    for quantum in [1, 4096] {
        let pool = Budget::new(4 * 1024 * 1024).unwrap();
        let movie = support::parse(&pool, case.bytes);
        let plan = global(&pool, &movie, 2, 3500);
        assert_eq!(plan.landing().selected.timestamp, 3000);
        let mut cursor = plan.cursor().unwrap();
        let mut rows = Vec::new();
        let mut ended = false;
        for _ in 0..8192 {
            match cursor
                .poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
                .unwrap()
            {
                CursorStatus::Yield => {}
                CursorStatus::End => {
                    ended = true;
                    break;
                }
                CursorStatus::Ready => {
                    let occurrence = cursor.take_occurrence().unwrap();
                    let range = occurrence.source.range.clone();
                    let source = case
                        .samples
                        .iter()
                        .find(|sample| {
                            sample.track == occurrence.track_id
                                && sample.ordinal == occurrence.source.ordinal
                        })
                        .unwrap();
                    assert_eq!(
                        &case.bytes[usize::try_from(range.start).unwrap()
                            ..usize::try_from(range.end).unwrap()],
                        source.payload
                    );
                    rows.push((
                        occurrence.track_id,
                        occurrence.source.ordinal,
                        range,
                        occurrence.demux.dts_field.signed_ticks(),
                    ));
                }
            }
        }
        assert!(
            ended,
            "seek replay did not reach the end within the work bound"
        );
        assert_eq!(rows, expected);
    }
}
#[test]
fn unsupported_rescale_never_guesses_a_demux_time() {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let movie = support::parse(&pool, include_bytes!("fixtures/r-multitrack.mov"));
    let mut tracks = movie.demux();
    tracks[1].media_base = TimeBase::new(1, 48000).unwrap();
    assert!(matches!(
        GlobalSeekJob::new_complete_source(
            &pool,
            &tracks,
            &movie.media,
            1,
            1500,
            SeekLimits::default(),
            DemuxProfile::NoParseNoFillin
        ),
        Err(Error::Unsupported(_))
    ));
}
#[test]
fn repeated_edit_seek_lands_on_the_last_key_at_or_before_the_target_in_index_order() {
    // Index: 0@735 key, 1000@747, 2000@759 discard, 3000@771 key and discard,
    // 2000@771 key, 3000@783, 4000@795 discard, 4000@735 key. The second edit replays
    // media 3000 from 2000, the third media 0 from 4000 (ISO/IEC 14496-12 §8.6.6).
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let movie = support::parse(&pool, include_bytes!("fixtures/r-edits-repeat.mov"));
    let tracks = movie.demux();
    for (target, entry, timestamp, position) in [
        (1500, 0, 0, 735),
        (3500, 4, 2000, 771),
        (4500, 7, 4000, 735),
    ] {
        let mut job = SeekJob::new_complete_source(
            &pool,
            tracks[0],
            &movie.media,
            target,
            SeekLimits::default(),
            DemuxProfile::NoParseNoFillin,
        )
        .unwrap();
        let mut ready = false;
        for _ in 0..4096 {
            if job
                .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
                == BuildStatus::Ready
            {
                ready = true;
                break;
            }
        }
        assert!(
            ready,
            "target {target} did not finish within the work bound"
        );
        let landing = job.finish().unwrap().landing().selected.clone();
        assert_eq!(
            (landing.ordinal, landing.timestamp, landing.position),
            (entry, timestamp, position),
            "target {target}"
        );
    }
}

#[test]
fn seek_lowers_the_target_by_the_composition_shift() {
    // The demuxer lowers a query by the largest amount a composition time it took into
    // account precedes its decode time (ISO/IEC 14496-12 section 8.6.1.3). A regular
    // track's index times are lowered by the same amount, so the key at decode time
    // 3000 is the landing from 3000 on: with a smallest offset of -1000, and of -100
    // where the -700 of a `ctts` table's last two rows is not taken into account. A
    // fragmented track's index keeps its decode times, so its key at 3000 is the
    // landing only from 4000 on.
    type Landing = (i64, u64, i64, i64);
    let cases: [(&[u8], [Landing; 2]); 3] = [
        (
            include_bytes!("fixtures/r-ctts-v1.mov"),
            [(2999, 0, -1000, 731), (3000, 3, 2000, 767)],
        ),
        (
            include_bytes!("fixtures/native-followup/follow-clock-later-minimum-four.mov"),
            [(2999, 0, -100, 28), (3000, 3, 2900, 64)],
        ),
        (
            include_bytes!("fixtures/f-ctts-v1.mp4"),
            [(3999, 0, 0, 771), (4000, 3, 3000, 975)],
        ),
    ];
    for (bytes, landings) in cases {
        let pool = Budget::new(4 * 1024 * 1024).unwrap();
        let movie = support::parse(&pool, bytes);
        let tracks = movie.demux();
        for (target, entry, timestamp, position) in landings {
            // One work unit a poll takes the entry-by-entry seek, a large budget the
            // seek that reads the tables.
            for work in [1, 4096] {
                let mut job = SeekJob::new_complete_source(
                    &pool,
                    tracks[0],
                    &movie.media,
                    target,
                    SeekLimits::default(),
                    DemuxProfile::NoParseNoFillin,
                )
                .unwrap();
                let ready = (0..8192).any(|_| {
                    job.poll(&mut WorkBudget::new(work), &AtomicBool::new(false))
                        .unwrap()
                        == BuildStatus::Ready
                });
                assert!(
                    ready,
                    "target {target} did not finish within the work bound"
                );
                let landing = job.finish().unwrap().landing().selected.clone();
                assert_eq!(
                    (landing.ordinal, landing.timestamp, landing.position),
                    (entry, timestamp, position),
                    "target {target}, work {work}"
                );
            }
        }
    }
}
