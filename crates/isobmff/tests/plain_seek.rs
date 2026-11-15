//! Seeks over tracks whose index entries are their samples read those entries
//! from the tables. They must land where a scan of every index entry lands, and
//! continue as a replay of every track from its beginning continues.
mod common;
#[path = "support/demux.rs"]
mod support;
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BuildStatus,
    compat::mov::{
        packets::{CursorStatus, DemuxProfile, DemuxTrack, PacketCursor, PacketLimits},
        seek::{GlobalSeekJob, IndexCursor, IndexEntry, IndexLimits, SeekJob, SeekLimits},
    },
};

/// Regular tables of every layout, edited tracks and fragments.
const FIXTURES: &[&str] = &[
    "r-front-stco.mov",
    "r-end-co64.mov",
    "r-constant-stsz.mov",
    "r-ctts-v0.mov",
    "r-ctts-v1.mov",
    "r-negative-cts.mov",
    "r-multitrack.mov",
    "r-multiple-mdats.mov",
    "r-stss-absent.mov",
    "r-stss-empty.mov",
    "r-stz2-4.mov",
    "r-stz2-8.mov",
    "r-stz2-16.mov",
    "r-vfr.mov",
    "r-edits-empty-trim.mov",
    "r-edits-repeat.mov",
    "r-elst-v1.mov",
    "edit-leading-empty-one.mov",
    "edit-single-late.mov",
    "edit-single-one.mov",
    "edit-single-trim.mov",
    "edit-single-two.mov",
    "edit-two-repeat-one.mov",
    "f-explicit.mp4",
    "f-tfhd-defaults.mp4",
    "f-trex-defaults.mp4",
    "f-first-flags.mp4",
    "f-multitrack.mp4",
    "f-ctts-v0.mp4",
    "f-ctts-v1.mp4",
    "f-tfdt64.mp4",
];

type Row = (u32, u64, Range<u64>, Option<i64>, Option<i64>, u32);

fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn index(
    pool: &Budget,
    track: DemuxTrack<'_>,
    media: &[Range<u64>],
) -> Result<Vec<IndexEntry>, Error> {
    let mut cursor = IndexCursor::new_complete_source(
        pool,
        track,
        media,
        IndexLimits::default(),
        DemuxProfile::NoParseNoFillin,
    )?;
    let mut entries = Vec::new();
    loop {
        match cursor.poll(&mut WorkBudget::new(4096), &cancel())? {
            CursorStatus::Yield => {}
            CursorStatus::End => return Ok(entries),
            CursorStatus::Ready => entries.push(cursor.take_entry()?),
        }
    }
}

/// The entry a backward seek to `target` selects, restated from the seek's
/// rule over every entry: the last key at or before the target, or else the
/// first key, or a fragment track's first entry where `prefix` admits it.
fn select(entries: &[IndexEntry], target: i64, fragments: bool, prefix: bool) -> Option<usize> {
    let mut first = None;
    let mut best = None;
    let mut previous = None;
    let mut unordered: Option<i64> = None;
    for (at, entry) in entries.iter().enumerate() {
        if entry.timestamp == i64::MIN {
            return None;
        }
        let key = entry.flags & 1 != 0;
        if at == 0 && !key && fragments {
            first = Some(at);
        }
        if previous.is_some_and(|prior| entry.timestamp <= prior) || unordered.is_some() {
            unordered = Some(unordered.map_or(entry.timestamp, |least| least.min(entry.timestamp)));
        }
        previous = Some(entry.timestamp);
        if key {
            first = first.or(Some(at));
            if entry.timestamp <= target {
                best = Some(at);
            }
        }
    }
    if unordered.is_some_and(|least| target >= least) {
        return None;
    }
    best.or(first.filter(|&at| entries[at].flags & 1 != 0 || prefix))
}

fn rows(cursor: &mut PacketCursor<'_>, count: usize) -> Result<Vec<Row>, Error> {
    let mut rows = Vec::new();
    while rows.len() < count {
        match cursor.poll(&mut WorkBudget::new(4096), &cancel())? {
            CursorStatus::Yield => {}
            CursorStatus::End => break,
            CursorStatus::Ready => {
                let row = cursor.take_occurrence()?;
                rows.push((
                    row.track_id,
                    row.source.ordinal,
                    row.source.range,
                    row.demux.dts,
                    row.demux.pts,
                    row.demux.flags,
                ));
            }
        }
    }
    Ok(rows)
}

#[test]
fn table_seeks_land_and_continue_as_full_scans_do() {
    let mut checked = 0;
    for name in FIXTURES {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let bytes = std::fs::read(&path).unwrap();
        let pool = Budget::new(64 << 20).unwrap();
        let movie = support::parse(&pool, &bytes);
        let tracks = movie.demux();
        for (at, &track) in tracks.iter().enumerate() {
            let Ok(entries) = index(&pool, track, &movie.media) else {
                continue;
            };
            let fragments = movie
                .fragments
                .iter()
                .any(|fragment| fragment.track_id() == track.track_id);
            let mut targets = vec![i64::MIN + 1, -1, 0, 1 << 40];
            // About sixteen entries from each track, with its last.
            let stride = entries.len().div_ceil(16).max(1);
            for entry in entries.iter().step_by(stride).chain(entries.last()) {
                targets.extend([
                    entry.timestamp.saturating_sub(1),
                    entry.timestamp,
                    entry.timestamp.saturating_add(1),
                ]);
            }
            for target in targets {
                let mut outcomes = Vec::new();
                for quantum in [3, 4096] {
                    let mut job = SeekJob::new_complete_source(
                        &pool,
                        track,
                        &movie.media,
                        target,
                        SeekLimits::default(),
                        DemuxProfile::NoParseNoFillin,
                    )
                    .unwrap();
                    let outcome = loop {
                        match job.poll(&mut WorkBudget::new(quantum), &cancel()) {
                            Ok(BuildStatus::Yield) => {}
                            Ok(BuildStatus::Ready) => break job.finish(),
                            Err(error) => break Err(error),
                        }
                    };
                    outcomes.push(outcome.map(|plan| plan.landing().clone()));
                }
                let landing = outcomes[1].as_ref().ok();
                for other in &outcomes[..1] {
                    match (other, landing) {
                        (Ok(other), Some(landing)) => {
                            assert_eq!(other.selected.ordinal, landing.selected.ordinal, "{name}");
                            assert_eq!(other.scanned_entries, landing.scanned_entries, "{name}");
                        }
                        (Err(other), None) => {
                            assert_eq!(Some(other), outcomes[1].as_ref().err(), "{name}");
                        }
                        _ => panic!("{name} track {at} target {target}: budgets disagree"),
                    }
                }
                // Fragment edits move the query; the scan below does not.
                if !(fragments && track.edits.is_some()) {
                    let expected = select(&entries, target, fragments, false);
                    assert_eq!(
                        landing.map(|landing| landing.selected.ordinal),
                        expected.map(|at| at as u64),
                        "{name} track {at} target {target}"
                    );
                }
                let Some(landing) = landing else {
                    continue;
                };
                assert_eq!(landing.scanned_entries, entries.len() as u64, "{name}");
                let entry = &entries[usize::try_from(landing.selected.ordinal).unwrap()];
                let selected = &landing.selected.occurrence;
                assert_eq!(selected.source, entry.occurrence.source, "{name}");
                assert_eq!(selected.demux, entry.occurrence.demux, "{name}");
                assert_eq!(
                    selected.clock_origin, entry.occurrence.clock_origin,
                    "{name}"
                );

                // Every track continues from its anchor as a replay reaches it.
                let mut global = GlobalSeekJob::new_complete_source(
                    &pool,
                    &tracks,
                    &movie.media,
                    track.track_id,
                    target,
                    SeekLimits::default(),
                    DemuxProfile::NoParseNoFillin,
                )
                .unwrap();
                let plan = loop {
                    match global.poll(&mut WorkBudget::new(5), &cancel()) {
                        Ok(BuildStatus::Yield) => {}
                        Ok(BuildStatus::Ready) => break global.finish(),
                        Err(error) => break Err(error),
                    }
                };
                let Ok(plan) = plan else {
                    continue;
                };
                let resumed = rows(&mut plan.cursor().unwrap(), 12);
                let mut replay = PacketCursor::new_complete_source_at(
                    &pool,
                    &tracks,
                    &movie.media,
                    PacketLimits::default(),
                    DemuxProfile::NoParseNoFillin,
                    plan.anchors(),
                )
                .unwrap();
                assert_eq!(
                    resumed,
                    rows(&mut replay, 12),
                    "{name} track {at} target {target}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 300, "{checked} seeks compared");
}
