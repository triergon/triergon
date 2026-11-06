//! Source assertions precede the native-index seam. Native tuples are
//! literal native observations.
mod common;
#[path = "support/native34.rs"]
mod support;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_isobmff::{
    Mov9ClockOrigin, Mov9CursorStatus, Mov9NativeIndexCursor, Mov9NativeIndexLimits, Mov9Profile,
    SampleStep, SourceTrack, TrackSampleCursor,
};

fn check(bytes: &[u8], positions: [i64; 6], distances: [i32; 6]) {
    let pool = Budget::new(2 * 1024 * 1024).unwrap();
    let movie = support::parse(&pool, bytes);
    let source = SourceTrack::new(1, Some(&movie.tracks[0].table), &movie.fragments).unwrap();
    let samples = {
        let mut canonical = TrackSampleCursor::new(source);
        let mut samples = Vec::new();
        let mut source_ended = false;
        for _ in 0..4096 {
            match canonical.poll(&mut WorkBudget::new(1)).unwrap() {
                SampleStep::Yield => {}
                SampleStep::Sample(sample) => samples.push(sample),
                SampleStep::End => {
                    source_ended = true;
                    break;
                }
            }
        }
        assert!(source_ended);
        assert_eq!(samples.len(), 6);
        for (i, (sample, cts)) in samples
            .iter()
            .zip([0_i128, 3000, 1000, 2000, 5000, 4000])
            .enumerate()
        {
            assert_eq!(sample.ordinal, u64::try_from(i).unwrap() + 1);
            assert_eq!(sample.description_index, 1);
            assert_eq!(sample.dts_ticks, Some(i128::try_from(i).unwrap() * 1000));
            assert_eq!(sample.cts_ticks, Some(cts));
            assert_eq!(sample.duration_ticks, 1000);
            assert_eq!(i64::try_from(sample.range.start).unwrap(), positions[i]);
            assert_eq!(sample.range.end - sample.range.start, 12);
            assert_eq!(
                sample.fragment_flags,
                Some(if i % 3 == 0 { 0x0200_0000 } else { 0x0101_0000 })
            );
        }
        assert_eq!(samples[0].composition_offset, None);
        for (sample, (bits, signed)) in samples[1..].iter().zip([
            (2000, 2000),
            (4294966296, -1000),
            (4294966296, -1000),
            (1000, 1000),
            (4294966296, -1000),
        ]) {
            let offset = sample.composition_offset.unwrap();
            assert_eq!(offset.version(), 1);
            assert_eq!(offset.raw_bits(), bits);
            assert_eq!(offset.signed_value(), Some(signed));
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
        assert!(ended, "native CTS append exceeded unchanged poll bound");
        assert_eq!(rows.len(), samples.len());
        for (i, (row, sample)) in rows.iter().zip(&samples).enumerate() {
            assert_eq!(&row.occurrence.source, sample);
            assert_eq!(row.occurrence.clock_origin, Mov9ClockOrigin::SourceClock);
            assert_eq!(row.ordinal, u64::try_from(i).unwrap());
            assert_eq!(row.position, positions[i]);
            assert_eq!(row.timestamp, i64::try_from(i).unwrap() * 1000);
            assert_eq!(row.size, 12);
            assert_eq!(row.flags, if i % 3 == 0 { 1 } else { 0 });
            assert_eq!(row.raw_flag_bits, if i % 3 == 0 { 1 } else { 0 });
            assert_eq!(row.min_distance, distances[i]);
        }
        drop(cursor);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn original_signed_cts_append_keeps_absent_first_field_and_negative_raw_bits() {
    // Actual v35 native-index/039-{ordinary,tiny}-reference.stdout.
    check(
        include_bytes!("fixtures/f-ctts-v1.mp4"),
        [771, 783, 795, 975, 987, 999],
        [0, 0, 1, 0, 0, 1],
    );
}

#[test]
fn selected_split_cts_append_preserves_each_source_sample_and_native_distance() {
    // Actual v35 native-index/184-{ordinary,tiny}-reference.stdout.
    check(
        include_bytes!("fixtures/native-append36/trun18-original-split-runs.mp4"),
        [771, 783, 795, 975, 987, 999],
        [0, 0, 1, 0, 0, 1],
    );
}

#[test]
fn single_sample_cts_runs_carry_the_last_distance_once_per_real_append() {
    // Actual v35 native-index/185-{ordinary,tiny}-reference.stdout.
    check(
        include_bytes!("fixtures/native-append36/trun18-original-single-sample-runs.mp4"),
        [791, 803, 815, 1015, 1027, 1039],
        [0, 0, 0, 0, 0, 0],
    );
}
