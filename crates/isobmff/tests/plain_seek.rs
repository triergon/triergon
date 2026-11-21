//! Seeks over tracks whose index entries are their samples read those entries
//! from the tables. They must land where a scan of every index entry lands,
//! refuse where that scan refuses, with the same error, and continue as a
//! replay of every track from its beginning continues.
mod common;
#[path = "support/demux.rs"]
mod support;
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BuildStatus, CursorStatus,
    compat::mov::{
        packets::{DemuxProfile, DemuxTrack, PacketCursor, PacketLimits},
        seek::{
            GlobalSeekJob, GlobalSeekPlan, IndexCursor, IndexEntry, IndexLimits, SeekJob,
            SeekLanding, SeekLimits,
        },
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

/// Targets every track is sought to, before those near its entries.
const TARGETS: [i64; 4] = [i64::MIN + 1, -1, 0, 1 << 40];

type Row = (u32, u64, Range<u64>, Option<i64>, Option<i64>, u32);

/// What a comparison of one movie saw: seeks that landed and continued as a
/// replay does, and seeks refused as the full index refuses them.
#[derive(Default)]
struct Compared {
    continued: usize,
    refused: Vec<Error>,
}

fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
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

fn seek(
    pool: &Budget,
    track: DemuxTrack<'_>,
    media: &[Range<u64>],
    target: i64,
    quantum: u64,
) -> Result<SeekLanding, Error> {
    let mut job = SeekJob::new_complete_source(
        pool,
        track,
        media,
        target,
        SeekLimits::default(),
        DemuxProfile::NoParseNoFillin,
    )
    .unwrap();
    loop {
        match job.poll(&mut WorkBudget::new(quantum), &cancel())? {
            BuildStatus::Yield => {}
            BuildStatus::Ready => return Ok(job.finish()?.landing().clone()),
        }
    }
}

fn global_seek<'a>(
    pool: &Budget,
    tracks: &[DemuxTrack<'a>],
    media: &'a [Range<u64>],
    track_id: u32,
    target: i64,
) -> Result<GlobalSeekPlan<'a>, Error> {
    let mut global = GlobalSeekJob::new_complete_source(
        pool,
        tracks,
        media,
        track_id,
        target,
        SeekLimits::default(),
        DemuxProfile::NoParseNoFillin,
    )
    .unwrap();
    loop {
        match global.poll(&mut WorkBudget::new(5), &cancel())? {
            BuildStatus::Yield => {}
            BuildStatus::Ready => return global.finish(),
        }
    }
}

/// The entry a backward seek to `target` selects, restated from the seek's
/// rule over every entry: the last key at or before the target, or else the
/// first key, or a fragment track's first entry where `prefix` admits it. The
/// seek refuses an entry with the null timestamp, a target at or after an
/// entry out of timestamp order, and an index without a key to fall back to.
fn select(
    entries: &[IndexEntry],
    target: i64,
    fragments: bool,
    prefix: bool,
) -> Result<usize, Error> {
    let mut first = None;
    let mut best = None;
    let mut previous = None;
    let mut unordered: Option<i64> = None;
    for (at, entry) in entries.iter().enumerate() {
        if entry.timestamp == i64::MIN {
            return Err(Error::Unsupported("seek over a null index clock"));
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
        return Err(Error::Unsupported(
            "seek query between overlapping index clocks",
        ));
    }
    best.or(first.filter(|&at| entries[at].flags & 1 != 0 || prefix))
        .ok_or(Error::Unsupported("seek fallback is not a key packet"))
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

/// Seeks every track of `movie` to targets around its entries, in small and
/// large work quanta and as the reference of a seek of every track, and holds
/// each to a scan of the track's full index.
fn compare(name: &str, pool: &Budget, movie: &support::Movie) -> Compared {
    let mut compared = Compared::default();
    let tracks = movie.demux();
    let indexes: Vec<_> = tracks
        .iter()
        .map(|&track| index(pool, track, &movie.media))
        .collect();
    for (at, &track) in tracks.iter().enumerate() {
        let entries = match &indexes[at] {
            Ok(entries) => entries,
            Err(error) => {
                // A track whose full index fails has no seek: each one fails
                // as the index does, alone or as the reference of every track.
                for target in TARGETS {
                    for quantum in [3, 4096] {
                        let outcome = seek(pool, track, &movie.media, target, quantum);
                        assert_eq!(
                            outcome.err(),
                            Some(*error),
                            "{name} track {at} target {target}"
                        );
                    }
                    let global = global_seek(pool, &tracks, &movie.media, track.track_id, target);
                    assert_eq!(
                        global.err(),
                        Some(*error),
                        "{name} track {at} target {target}"
                    );
                    compared.refused.push(*error);
                }
                continue;
            }
        };
        // The first index failure of another track, which a seek of every
        // track meets once its reference has landed.
        let other_failure = indexes
            .iter()
            .enumerate()
            .filter(|&(other, _)| other != at)
            .find_map(|(_, result)| result.as_ref().err().copied());
        let fragments = movie
            .fragments
            .iter()
            .any(|fragment| fragment.track_id() == track.track_id);
        let mut targets = TARGETS.to_vec();
        // About sixteen entries from each track, with its last.
        let stride = entries.len().div_ceil(16).max(1);
        for entry in entries.iter().step_by(stride).chain(entries.last()) {
            targets.extend([
                entry.timestamp.saturating_sub(1),
                entry.timestamp,
                entry.timestamp.saturating_add(1),
            ]);
        }
        // A seek refuses the null timestamp as an argument, before any entry.
        targets.retain(|&target| target != i64::MIN);
        for target in targets {
            let outcomes =
                [3, 4096].map(|quantum| seek(pool, track, &movie.media, target, quantum));
            let facts = |outcome: &Result<SeekLanding, Error>| {
                outcome
                    .as_ref()
                    .map(|landing| (landing.selected.ordinal, landing.scanned_entries))
                    .map_err(|error| *error)
            };
            assert_eq!(
                facts(&outcomes[0]),
                facts(&outcomes[1]),
                "{name} track {at} target {target}: budgets disagree"
            );
            let landing = outcomes[1].as_ref();
            // Fragment edits move the query; the scan below does not.
            if !(fragments && track.edits.is_some()) {
                let expected = select(entries, target, fragments, false);
                assert_eq!(
                    landing
                        .map(|landing| landing.selected.ordinal)
                        .map_err(|error| *error),
                    expected.map(|at| at as u64),
                    "{name} track {at} target {target}"
                );
            }
            let global = global_seek(pool, &tracks, &movie.media, track.track_id, target);
            let landing = match landing {
                Ok(landing) => landing,
                Err(error) => {
                    // The reference track refuses before any other is sought.
                    assert_eq!(
                        global.err(),
                        Some(*error),
                        "{name} track {at} target {target}"
                    );
                    compared.refused.push(*error);
                    continue;
                }
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

            // Only another track's index refuses once the reference has landed.
            let plan = match global {
                Ok(plan) => {
                    assert_eq!(other_failure, None, "{name} track {at} target {target}");
                    plan
                }
                Err(error) => {
                    assert_eq!(
                        Some(error),
                        other_failure,
                        "{name} track {at} target {target}"
                    );
                    compared.refused.push(error);
                    continue;
                }
            };
            // Every track continues from its anchor as a replay reaches it.
            let resumed = rows(&mut plan.cursor().unwrap(), 12);
            let mut replay = PacketCursor::new_complete_source_at(
                pool,
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
            compared.continued += 1;
        }
    }
    compared
}

#[test]
fn table_seeks_land_and_continue_as_full_scans_do() {
    let mut continued = 0;
    for name in FIXTURES {
        let bytes = fixture(name);
        let pool = Budget::new(64 << 20).unwrap();
        continued += compare(name, &pool, &support::parse(&pool, &bytes)).continued;
    }
    assert!(continued > 300, "{continued} seeks compared");
}

/// The offset of the payload of each box of type `kind` in `bytes`.
fn payloads(bytes: &[u8], kind: &[u8; 4]) -> Vec<usize> {
    bytes
        .windows(4)
        .enumerate()
        .filter(|(_, window)| window == kind)
        .map(|(at, _)| at + 4)
        .collect()
}

/// `bytes` with `value` written over them at `at`.
fn write(bytes: &mut [u8], at: usize, value: &[u8]) {
    bytes[at..at + value.len()].copy_from_slice(value);
}

/// `name`'s bytes with the size of its sample `n`, counted from zero, set to
/// `size` in its `stsz` box.
fn with_sample_size(name: &str, n: usize, size: u32) -> Vec<u8> {
    let mut bytes = fixture(name);
    let [stsz] = payloads(&bytes, b"stsz")[..] else {
        panic!("{name} needs one stsz box");
    };
    // Version and flags, constant size, count, then one size per sample.
    write(&mut bytes, stsz + 12 + 4 * n, &size.to_be_bytes());
    bytes
}

/// Sets the offset of chunk `n`, counted from zero, to `offset` in the `co64`
/// box of `bytes`.
fn with_chunk_offset(bytes: &mut [u8], n: usize, offset: u64) {
    let [co64] = payloads(bytes, b"co64")[..] else {
        panic!("needs one co64 box");
    };
    // Version and flags, count, then one offset per chunk.
    write(bytes, co64 + 8 + 8 * n, &offset.to_be_bytes());
}

/// Each malformed movie, refused through the scan of its full index, is
/// refused the same way through the walk over its tables.
#[test]
fn malformed_tables_refuse_as_full_scans_do() {
    let pool = Budget::new(64 << 20).unwrap();
    let check = |name: &str, movie: &support::Movie, expected: Error| {
        let compared = compare(name, &pool, movie);
        assert!(!compared.refused.is_empty(), "{name} was not refused");
        assert!(
            compared.refused.iter().all(|&error| error == expected),
            "{name}: {:?}",
            compared.refused
        );
    };

    // r-end-co64 holds three chunks of 2, 1 and 3 twelve-byte samples. Its last
    // chunk moves to just below 2^64 into a second extent, and its last sample
    // declares 2^32 - 1 bytes, which end past 2^64.
    let mut bytes = with_sample_size("r-end-co64.mov", 5, u32::MAX);
    let last_chunk = u64::MAX - 0x1000;
    with_chunk_offset(&mut bytes, 2, last_chunk);
    let mut movie = support::parse(&pool, &bytes);
    movie.media.push(last_chunk..u64::MAX);
    check("sample end past 2^64", &movie, Error::Overflow);

    // Its last chunk at 2^63, one past the greatest signed 64-bit position.
    let mut bytes = fixture("r-end-co64.mov");
    with_chunk_offset(&mut bytes, 2, 1 << 63);
    let mut movie = support::parse(&pool, &bytes);
    movie.media.push(1 << 63..(1 << 63) + 36);
    check(
        "chunk offset past i64::MAX",
        &movie,
        Error::Unsupported("index signed position"),
    );

    // r-front-stco's last sample declares 0x2000_0000 bytes, one more than an
    // index size holds, inside an extent that covers them.
    let bytes = with_sample_size("r-front-stco.mov", 5, 0x2000_0000);
    let mut movie = support::parse(&pool, &bytes);
    let [media] = &mut movie.media[..] else {
        panic!("r-front-stco has one mdat");
    };
    media.end = media.start + (1 << 31);
    check(
        "sample size past 0x1fff_ffff",
        &movie,
        Error::Unsupported("index signed size"),
    );

    // f-tfdt64's fragments start at 2^63 ticks: the first entry's timestamp
    // reads as i64::MIN, the index's null clock.
    let mut bytes = fixture("f-tfdt64.mp4");
    for (n, tfdt) in payloads(&bytes, b"tfdt").into_iter().enumerate() {
        // Version 1 and flags, then a 64-bit decode time; three samples of
        // 1,000 ticks each per fragment.
        let time = (1u64 << 63) + 3000 * n as u64;
        write(&mut bytes, tfdt + 4, &time.to_be_bytes());
    }
    let movie = support::parse(&pool, &bytes);
    let entries = index(&pool, movie.demux()[0], &movie.media).unwrap();
    assert_eq!(entries[0].timestamp, i64::MIN);
    check(
        "timestamp at i64::MIN",
        &movie,
        Error::Unsupported("seek over a null index clock"),
    );

    // f-explicit's two fragments in reverse order: the second's ordinals,
    // 4 to 6, come before the first's.
    let bytes = fixture("f-explicit.mp4");
    let mut movie = support::parse(&pool, &bytes);
    movie.fragments.swap(0, 1);
    check(
        "fragment ordinals out of order",
        &movie,
        Error::Invalid("source ordinal order"),
    );
}
