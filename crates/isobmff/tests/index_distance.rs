//! Literal demux index observations, with independent source assertions.
mod common;
#[path = "support/demux.rs"]
mod support;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_isobmff::compat::mov::{
    clock::ClockOrigin,
    packets::{CursorStatus, DemuxProfile, PacketCursor, PacketLimits},
    seek::{IndexCursor, IndexEntry, IndexLimits},
};

fn entries(pool: &Budget, movie: &support::Movie, track: usize, quantum: u64) -> Vec<IndexEntry> {
    let tracks = movie.demux();
    let baseline = pool.used();
    let cancel = AtomicBool::new(false);
    let mut cursor = IndexCursor::new_complete_source(
        pool,
        tracks[track],
        &movie.media,
        IndexLimits::default(),
        DemuxProfile::NoParseNoFillin,
    )
    .unwrap();
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        CursorStatus::Yield
    );
    let mut rows = Vec::new();
    let mut ended = false;
    for _ in 0..4096 {
        match cursor.poll(&mut WorkBudget::new(quantum), &cancel).unwrap() {
            CursorStatus::Yield => {}
            CursorStatus::End => {
                ended = true;
                break;
            }
            CursorStatus::Ready => {
                let held = pool.used();
                assert_eq!(
                    cursor.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
                    CursorStatus::Ready
                );
                assert_eq!(pool.used(), held);
                rows.push(cursor.take_entry().unwrap());
            }
        }
    }
    assert!(ended, "native distance pass exceeded unchanged poll bound");
    drop(cursor);
    assert_eq!(pool.used(), baseline);
    rows
}

#[test]
fn one_trun_counts_each_nonkey_after_the_key_without_a_flag_presence_lag() {
    let pool = Budget::new(2 * 1024 * 1024).unwrap();
    let movie = support::parse(
        &pool,
        include_bytes!("fixtures/native-behavior/flag-contiguous-key.mp4"),
    );
    assert_eq!(movie.tracks[0].table.sample_count(), 0);
    for quantum in [1, 4096] {
        let rows = entries(&pool, &movie, 0, quantum);
        assert_eq!(rows.len(), 6);
        for (i, row) in rows.iter().enumerate() {
            let ticks = i128::try_from(i).unwrap() * 1000;
            assert_eq!(row.occurrence.source.ordinal, u64::try_from(i).unwrap() + 1);
            assert_eq!(row.occurrence.source.dts_ticks, Some(ticks));
            assert_eq!(row.occurrence.source.cts_ticks, Some(ticks));
            assert!(row.occurrence.source.fragment_flags.is_some());
            assert_eq!(row.occurrence.source.is_container_sync(), i % 3 == 0);
            assert_eq!(
                row.occurrence.source.range.end - row.occurrence.source.range.start,
                12
            );
        }
        // Demux reference output demux-index/105-{ordinary,tiny}.
        let actual: Vec<_> = rows.iter().map(|row| row.min_distance).collect();
        assert_eq!(actual, [0, 1, 2, 0, 1, 2]);
    }
}

#[test]
fn one_trun_distance_is_independent_for_each_track_and_demux_null_clock() {
    let pool = Budget::new(2 * 1024 * 1024).unwrap();
    let movie = support::parse(
        &pool,
        include_bytes!("fixtures/native-clock29/order27-MIN-scale1000-cat12.mp4"),
    );
    assert_eq!(movie.tracks.len(), 2);
    let offsets = [0_i128, 1, 3, 4, 6, 7];
    for quantum in [1, 4096] {
        for (track, source_track) in movie.tracks.iter().enumerate() {
            let rows = entries(&pool, &movie, track, quantum);
            assert_eq!(rows.len(), 6);
            for (row, offset) in rows.iter().zip(offsets) {
                let source_ticks = if track == 0 {
                    (1_i128 << 63) + offset
                } else {
                    offset
                };
                assert_eq!(row.occurrence.source.dts_ticks, Some(source_ticks));
                assert_eq!(row.track_id, source_track.id);
            }
            if track == 0 {
                assert_eq!(rows[0].timestamp, i64::MIN);
                assert_eq!(rows[0].occurrence.demux.dts, None);
                assert_eq!(rows[0].occurrence.demux.dts_field.signed_ticks(), i64::MIN);
            }
            // Demux reference output demux-index/223-{ordinary,tiny}.
            let actual: Vec<_> = rows.iter().map(|row| row.min_distance).collect();
            assert_eq!(actual, [0, 1, 2, 0, 1, 2]);
        }
    }
}

#[test]
fn index_replay_uses_checked_facts_for_the_selected_track() {
    let pool = Budget::new(2 * 1024 * 1024).unwrap();
    let movie = support::parse(
        &pool,
        include_bytes!("fixtures/native-clock29/order27-MIN-scale1000-cat12.mp4"),
    );
    let tracks = movie.demux();
    let mut packets = PacketCursor::new_complete_source(
        &pool,
        &tracks,
        &movie.media,
        PacketLimits::default(),
        DemuxProfile::NoParseNoFillin,
    )
    .unwrap();
    assert!(IndexCursor::replay_track(&packets, tracks[0].track_id, 6).is_err());
    let cancel = AtomicBool::new(false);
    loop {
        match packets.poll(&mut WorkBudget::new(3), &cancel).unwrap() {
            CursorStatus::Ready => {
                packets.take_occurrence().unwrap();
            }
            CursorStatus::Yield => {}
            CursorStatus::End => break,
        }
    }
    for (track, selected) in tracks.iter().enumerate() {
        let mut replay = IndexCursor::replay_track(&packets, selected.track_id, 6).unwrap();
        for expected in entries(&pool, &movie, track, 3) {
            while replay.poll(&mut WorkBudget::new(3), &cancel).unwrap() == CursorStatus::Yield {}
            let actual = replay.take_entry().unwrap();
            assert_eq!(actual.track_id, selected.track_id);
            assert_eq!(actual.timestamp, expected.timestamp);
            assert_eq!(actual.position, expected.position);
            assert_eq!(actual.min_distance, expected.min_distance);
            assert_eq!(
                actual.occurrence.source.dts_ticks,
                expected.occurrence.source.dts_ticks
            );
        }
    }
}

#[test]
fn one_trun_distance_keeps_unsigned_source_dts_and_signed_cts_at_demux_sentinel() {
    let pool = Budget::new(2 * 1024 * 1024).unwrap();
    let movie = support::parse(
        &pool,
        include_bytes!("fixtures/native-clock28/clock27-DTS-MIN-PTS-next.mp4"),
    );
    for quantum in [1, 4096] {
        let rows = entries(&pool, &movie, 0, quantum);
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].occurrence.source.dts_ticks, Some(1_i128 << 63));
        assert_eq!(
            rows[0].occurrence.source.cts_ticks,
            Some((1_i128 << 63) + 1)
        );
        assert_eq!(rows[0].occurrence.demux.dts, None);
        assert_eq!(rows[0].occurrence.demux.pts, Some(i64::MIN + 1));
        assert_eq!(rows[0].timestamp, i64::MIN);
        // Demux reference output demux-index/220-{ordinary,tiny}.
        let actual: Vec<_> = rows.iter().map(|row| row.min_distance).collect();
        assert_eq!(actual, [0, 1, 2, 0, 1, 2]);
    }
}

#[test]
fn split_trun_append_keeps_original_distance_and_unknown_source_clock_gold() {
    let pool = Budget::new(2 * 1024 * 1024).unwrap();
    for (bytes, second_origin_unknown) in [
        (include_bytes!("fixtures/f-explicit.mp4").as_slice(), false),
        (
            include_bytes!("fixtures/f-no-second-tfdt.mp4").as_slice(),
            true,
        ),
    ] {
        let movie = support::parse(&pool, bytes);
        for quantum in [1, 4096] {
            let rows = entries(&pool, &movie, 0, quantum);
            assert_eq!(rows.len(), 6);
            for (i, row) in rows.iter().enumerate() {
                let source_ticks = if second_origin_unknown && i >= 3 {
                    None
                } else {
                    Some(i128::try_from(i).unwrap() * 1000)
                };
                assert_eq!(row.occurrence.source.dts_ticks, source_ticks);
                assert_eq!(row.occurrence.source.cts_ticks, source_ticks);
                if second_origin_unknown && i >= 3 {
                    assert!(matches!(
                        row.occurrence.clock_origin,
                        ClockOrigin::PredecessorEnd(_)
                    ));
                }
            }
            // Demux reference output demux-index/026.
            let actual: Vec<_> = rows.iter().map(|row| row.min_distance).collect();
            assert_eq!(actual, [0, 0, 1, 0, 0, 1]);
        }
    }
}
