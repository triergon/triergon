//! Literal v36 C34 index tuples and independent original source facts.
//! All12 existing inputs/oracles and all143 older assertions remain unchanged.
mod common;
#[path = "support/native34.rs"]
mod support;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_isobmff::{
    Mov9ClockOrigin, Mov9CursorStatus, Mov9NativeIndexCursor, Mov9NativeIndexLimits, Mov9Profile,
    SampleStep, SourceTrack, TrackSampleCursor,
};

// Native tuple: position, raw signed timestamp, signed index flags, min_distance.
type NativeRow = (i64, i64, i32, i32);
fn check(
    bytes: &[u8],
    source_ticks: &[Option<i128>],
    durations: &[u32],
    source_keys: &[u64],
    gold: &[NativeRow],
) {
    assert_eq!(source_ticks.len(), gold.len());
    assert_eq!(durations.len(), gold.len());
    let pool = Budget::new(2 * 1024 * 1024).unwrap();
    let movie = support::parse(&pool, bytes);
    assert_eq!(movie.tracks.len(), 1);
    assert_eq!(movie.tracks[0].table.sample_count(), 0);
    let source = SourceTrack::new(1, Some(&movie.tracks[0].table), &movie.fragments).unwrap();
    let samples = {
        let mut cursor = TrackSampleCursor::new(source);
        let mut samples = Vec::new();
        let mut ended = false;
        for _ in 0..4096 {
            match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
                SampleStep::Yield => {}
                SampleStep::Sample(sample) => samples.push(sample),
                SampleStep::End => {
                    ended = true;
                    break;
                }
            }
        }
        assert!(ended, "canonical source exceeded unchanged poll bound");
        assert_eq!(samples.len(), source_ticks.len());
        for (i, sample) in samples.iter().enumerate() {
            let ordinal = u64::try_from(i).unwrap() + 1;
            let key = source_keys.contains(&ordinal);
            assert_eq!(sample.ordinal, ordinal);
            assert_eq!(sample.dts_ticks, source_ticks[i]);
            assert_eq!(sample.cts_ticks, source_ticks[i]);
            assert_eq!(sample.duration_ticks, durations[i]);
            assert_eq!(sample.description_index, 1);
            assert_eq!(i64::try_from(sample.range.start).unwrap(), gold[i].0);
            assert_eq!(sample.range.end - sample.range.start, 12);
            assert_eq!(sample.is_container_sync(), key);
            assert_eq!(
                sample.fragment_flags,
                Some(if key { 0x0200_0000 } else { 0x0101_0000 })
            );
        }
        samples
    };
    let tracks = movie.native();
    for quantum in [1, 4096] {
        let baseline = pool.used();
        let cancel = AtomicBool::new(false);
        let mut cursor = Mov9NativeIndexCursor::new_complete_source(
            &pool,
            tracks[0],
            &movie.media,
            Mov9NativeIndexLimits::default(),
            Mov9Profile::PinnedNoParseNoFillin,
        )
        .unwrap();
        assert_eq!(
            cursor.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            Mov9CursorStatus::Yield
        );
        let mut rows = Vec::new();
        let mut ended = false;
        for _ in 0..4096 {
            match cursor.poll(&mut WorkBudget::new(quantum), &cancel).unwrap() {
                Mov9CursorStatus::Yield => {}
                Mov9CursorStatus::End => {
                    ended = true;
                    break;
                }
                Mov9CursorStatus::Ready => {
                    let held = pool.used();
                    assert_eq!(
                        cursor.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
                        Mov9CursorStatus::Ready
                    );
                    assert_eq!(pool.used(), held);
                    rows.push(cursor.take_entry().unwrap());
                }
            }
        }
        assert!(ended, "native append exceeded unchanged poll bound");
        assert_eq!(rows.len(), gold.len());
        for (i, (row, expected)) in rows.iter().zip(gold).enumerate() {
            assert_eq!(&row.occurrence.source, &samples[i]);
            assert_eq!(row.ordinal, u64::try_from(i).unwrap());
            assert_eq!(row.track_id, 1);
            assert_eq!(
                (row.position, row.timestamp, row.flags, row.min_distance),
                *expected
            );
            assert_eq!(row.size, 12);
            let (bits, packet_flags) = match expected.2 {
                0 => (0, 0),
                1 => (1, 1),
                -2 => (2, 4),
                -1 => (3, 5),
                _ => panic!("unmapped literal native flags"),
            };
            assert_eq!(row.raw_flag_bits, bits);
            assert_eq!(row.occurrence.native.flags, packet_flags);
            assert_eq!(row.occurrence.native.dts_field.signed_ticks(), expected.1);
            assert_eq!(row.occurrence.native.pts_field.signed_ticks(), expected.1);
            let api = if expected.1 == i64::MIN {
                None
            } else {
                Some(expected.1)
            };
            assert_eq!(row.occurrence.native.dts, api);
            assert_eq!(row.occurrence.native.pts, api);
            assert_eq!(row.occurrence.native.duration, i64::from(durations[i]));
            match source_ticks[i] {
                Some(source_tick) => {
                    assert_eq!(row.occurrence.clock_origin, Mov9ClockOrigin::SourceClock);
                    assert_eq!(
                        row.occurrence.native.dts_field.exact_native_ticks,
                        source_tick
                    );
                }
                None => {
                    let witness = match row.occurrence.clock_origin {
                        Mov9ClockOrigin::PredecessorEnd(witness) => witness,
                        other => panic!("unknown source lost explicit native witness: {other:?}"),
                    };
                    let previous = i.checked_sub(1).unwrap();
                    assert_eq!(witness.predecessor_sample, samples[previous].ordinal);
                    assert_eq!(witness.predecessor_duration_ticks, durations[previous]);
                    assert_eq!(
                        witness.predecessor_source_dts_known,
                        source_ticks[previous].is_some()
                    );
                    assert_eq!(
                        witness.predecessor_native_dts_ticks,
                        rows[previous]
                            .occurrence
                            .native
                            .dts_field
                            .exact_native_ticks
                    );
                    assert_eq!(witness.derived_dts_ticks, i128::from(expected.1));
                    assert_eq!(
                        witness
                            .predecessor_native_dts_ticks
                            .checked_add(i128::from(durations[previous])),
                        Some(witness.derived_dts_ticks)
                    );
                    assert_eq!(
                        row.occurrence.native.dts_field.exact_native_ticks,
                        witness.derived_dts_ticks
                    );
                }
            }
        }
        drop(cursor);
        assert_eq!(pool.used(), baseline);
    }
}

fn check_wide_origin(bytes: &[u8]) {
    let origin = 1_i128 << 63;
    check(
        bytes,
        &[
            Some(origin),
            Some(origin + 1000),
            Some(origin + 2000),
            Some(origin + 3000),
            Some(origin + 4000),
            Some(origin + 5000),
        ],
        &[1000; 6],
        &[1, 4],
        &[
            (799, i64::MIN, -1, 0),
            (811, i64::MIN + 1000, 0, 0),
            (823, i64::MIN + 2000, 0, 1),
            (991, i64::MIN + 3000, 1, 0),
            (1003, i64::MIN + 4000, 0, 0),
            (1015, i64::MIN + 5000, 0, 1),
        ],
    );
}
#[test]
fn legacy_tfdt_native_null_does_not_erase_known_source_or_block_run_append() {
    // Actual v36 native-index/055-reference: the first native API clock is null.
    check_wide_origin(include_bytes!(
        "fixtures/native-append37/f-tfdt-above-i64.mp4"
    ));
}
#[test]
fn iso6_tfdt_native_null_preserves_unsigned_source_and_raw_index_clock() {
    // Actual v36 native-index/090-reference, distinct unchanged source input.
    check_wide_origin(include_bytes!(
        "fixtures/native-append37/modern-iso6-tfdt-above-i64.mp4"
    ));
}
#[test]
fn new_fragment_nonkey_starts_zero_without_a_synthetic_container_key() {
    check(
        include_bytes!("fixtures/native-append37/flag-contiguous-nonkey.mp4"),
        &[
            Some(0),
            Some(1000),
            Some(2000),
            Some(3000),
            Some(4000),
            Some(5000),
        ],
        &[1000; 6],
        &[1],
        &[
            (743, 0, 1, 0),
            (755, 1000, 0, 1),
            (767, 2000, 0, 2),
            (915, 3000, 0, 0),
            (927, 4000, 0, 1),
            (939, 5000, 0, 2),
        ],
    );
}
#[test]
fn overlapping_fragment_keeps_nonkey_discard_bits_and_source_position_order() {
    check(
        include_bytes!("fixtures/native-append37/flag-one-sample-overlap-nonkey.mp4"),
        &[
            Some(0),
            Some(1000),
            Some(2000),
            Some(2000),
            Some(3000),
            Some(4000),
        ],
        &[1000; 6],
        &[1],
        &[
            (743, 0, 1, 0),
            (755, 1000, 0, 1),
            (767, 2000, 0, 2),
            (915, 2000, -2, 0),
            (927, 3000, 0, 1),
            (939, 4000, 0, 2),
        ],
    );
}
#[test]
fn backward_third_fragment_resets_distance_without_sorting_or_removing_discard() {
    check(
        include_bytes!("fixtures/native-append37/follow-flag-three-fragment-backward.mp4"),
        &[
            Some(0),
            Some(1000),
            Some(10000),
            Some(11000),
            Some(2000),
            Some(3000),
        ],
        &[1000; 6],
        &[1, 4],
        &[
            (731, 0, 1, 0),
            (743, 1000, 0, 1),
            (879, 10000, 0, 0),
            (891, 11000, 1, 0),
            (1027, 2000, -2, 0),
            (1039, 3000, -2, 1),
        ],
    );
}
#[test]
fn absent_origin_after_gap_keeps_source_none_and_explicit_native_prefix() {
    check(
        include_bytes!("fixtures/native-append37/origin18-three-gap-then-absent.mp4"),
        &[Some(0), Some(1000), Some(7000), Some(8000), None, None],
        &[1000; 6],
        &[1, 4],
        &[
            (731, 0, 1, 0),
            (743, 1000, 0, 1),
            (879, 7000, 0, 0),
            (891, 8000, 1, 0),
            (1007, 9000, 0, 0),
            (1019, 10000, 0, 1),
        ],
    );
}
#[test]
fn absent_origin_after_rollback_keeps_source_none_and_actual_discard_history() {
    check(
        include_bytes!("fixtures/native-append37/origin18-three-backward-then-absent.mp4"),
        &[Some(7000), Some(8000), Some(1000), Some(2000), None, None],
        &[1000; 6],
        &[1, 4],
        &[
            (731, 7000, 1, 0),
            (743, 8000, 0, 1),
            (879, 1000, -2, 0),
            (891, 2000, -1, 0),
            (1007, 3000, 0, 0),
            (1019, 4000, 0, 1),
        ],
    );
}
#[test]
fn explicit_origin_control_preserves_source_knownness_with_same_native_counter() {
    check(
        include_bytes!("fixtures/native-append37/discard22-explicit-equivalent.mp4"),
        &[
            Some(7000),
            Some(8000),
            Some(1000),
            Some(2000),
            Some(3000),
            Some(4000),
        ],
        &[1000; 6],
        &[1, 4],
        &[
            (731, 7000, 1, 0),
            (743, 8000, 0, 1),
            (879, 1000, -2, 0),
            (891, 2000, -1, 0),
            (1027, 3000, 0, 0),
            (1039, 4000, 0, 1),
        ],
    );
}
#[test]
fn later_anchor_control_keeps_all_native_entries_and_original_source_identity() {
    check(
        include_bytes!("fixtures/native-append37/discard22-later-anchor-5000.mp4"),
        &[
            Some(7000),
            Some(8000),
            Some(1000),
            Some(2000),
            None,
            None,
            Some(5000),
            Some(6000),
        ],
        &[1000; 8],
        &[1, 4, 7],
        &[
            (731, 7000, 1, 0),
            (743, 8000, 0, 1),
            (879, 1000, -2, 0),
            (891, 2000, -1, 0),
            (1007, 3000, 0, 0),
            (1019, 4000, 0, 1),
            (1155, 5000, 1, 0),
            (1167, 6000, 0, 1),
        ],
    );
}
#[test]
fn long_predecessor_duration_and_intermediate_anchor_do_not_raise_index_distance() {
    check(
        include_bytes!(
            "fixtures/native-append37/discard22-long-derived-then-intermediate-anchor.mp4"
        ),
        &[
            Some(7000),
            Some(8000),
            Some(1000),
            Some(2000),
            None,
            None,
            Some(10000),
            Some(11000),
        ],
        &[1000, 1000, 1000, 23000, 1000, 1000, 1000, 1000],
        &[1, 4, 7],
        &[
            (731, 7000, 1, 0),
            (743, 8000, 0, 1),
            (879, 1000, -2, 0),
            (891, 2000, -1, 0),
            (1007, 25000, 0, 0),
            (1019, 26000, 0, 1),
            (1155, 10000, -1, 0),
            (1167, 11000, -2, 1),
        ],
    );
}
#[test]
fn zero_delta_derived_clock_equality_keeps_source_none_and_both_entries() {
    check(
        include_bytes!("fixtures/native-append37/discard22-zero-delta-derived-equality.mp4"),
        &[Some(7000), Some(8000), Some(1000), Some(2000), None, None],
        &[1000, 1000, 1000, 1000, 0, 1000],
        &[1, 4],
        &[
            (731, 7000, 1, 0),
            (743, 8000, 0, 1),
            (879, 1000, -2, 0),
            (891, 2000, -1, 0),
            (1007, 3000, 0, 0),
            (1019, 3000, 0, 1),
        ],
    );
}
#[test]
fn split_backward_run_preserves_the_observed_flag_discriminator() {
    check(
        include_bytes!("fixtures/native-append37/discard24-split-backward-run.mp4"),
        &[
            Some(7000),
            Some(8000),
            Some(1000),
            Some(2000),
            Some(3000),
            Some(4000),
        ],
        &[1000; 6],
        &[1, 4],
        &[
            (731, 7000, 1, 0),
            (743, 8000, 0, 1),
            (899, 1000, -2, 0),
            (911, 2000, 1, 0),
            (1047, 3000, 0, 0),
            (1059, 4000, 0, 1),
        ],
    );
}
