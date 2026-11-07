//! Additional demux global-seek boundaries.
mod common;
#[path = "support/demux.rs"]
mod support;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, TimeBase, WorkBudget};
use triergon_isobmff::{
    BuildStatus,
    compat::mov::{
        packets::{CursorStatus, DemuxProfile},
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
fn unsupported_rescale_and_overlapping_query_never_guess_a_demux_time() {
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
    let movie = support::parse(&pool, include_bytes!("fixtures/r-edits-repeat.mov"));
    let tracks = movie.demux();
    let mut job = SeekJob::new_complete_source(
        &pool,
        tracks[0],
        &movie.media,
        3500,
        SeekLimits::default(),
        DemuxProfile::NoParseNoFillin,
    )
    .unwrap();
    for _ in 0..4096 {
        match job.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Err(Error::Unsupported(_)) => return,
            Err(e) => panic!("wrong refusal {e}"),
            Ok(BuildStatus::Yield) => {}
            Ok(BuildStatus::Ready) => panic!("overlapping native index query guessed"),
        }
    }
    panic!("ambiguous query refusal stalled");
}
