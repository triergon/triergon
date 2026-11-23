use super::*;
use crate::ebml_kit::{element, mkv, track, track_entry, uint};
use crate::storage::SharedBytes;
use crate::track::parse::TracksJob;
use triergon_core::Budget;

mod cue_ties;

#[test]
fn block_start_seek_accepts_positive_sub_hz_audio_rate() {
    use crate::BlockingReader;
    use crate::ebml_kit::{cluster, simple};
    use std::io::Cursor;

    let audio = track(
        2,
        2,
        &[
            element(0xe1, &element(0xb5, &0.5_f64.to_be_bytes())),
            uint(0x56aa, 1_000_000),
        ]
        .concat(),
    );
    let tracks = element(0x1654ae6b, &[track(1, 1, &[]), audio].concat());
    let raw = mkv(&[tracks, cluster(1, simple(1, 0, 0x80, b"keyframe"))].concat());
    let mut reader =
        BlockingReader::open_seekable(Cursor::new(raw), crate::Limits::default()).unwrap();
    let mut audio = reader.tracks()[1].clone();
    let recovery = RecoveryAudio::BlockStart;
    assert_eq!(recovery.delay_ns(&audio, 1_000_000).unwrap(), 0);
    assert_eq!(
        RecoveryAudio::Overlap.delay_ns(&audio, 1_000_000).unwrap(),
        1_000_000
    );

    let cancel = AtomicBool::new(false);
    let landing = crate::compat::seek::at_block_start(&mut reader, 0, 1_000_000, &cancel).unwrap();
    assert_eq!(landing.actual_ns, 1_000_000);
    let packet = reader.next_typed_packet(&cancel).unwrap().unwrap();
    assert_eq!(packet.packet.data.as_ref(), b"keyframe");

    for raw_rate in [0.0, -0.5, f64::NAN, f64::INFINITY] {
        audio.sample_rate = raw_rate;
        assert!(matches!(
            recovery.delay_ns(&audio, 1_000_000),
            Err(Error::Invalid("seek codec-delay sample rate"))
        ));
    }
}

#[test]
fn seek_plan_uses_all_track_preroll_negative_floor_and_keeps_extreme_unselected_ticks() {
    use crate::seek::Cue;
    let pool = Budget::new(64 * 1024).unwrap();
    let video = track_entry(1, 1, &uint(0x56aa, 2));
    let audio = track_entry(2, 2, &uint(0x56bb, 10));
    let raw = [element(0xae, &video), element(0xae, &audio)].concat();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut tracks =
        TracksJob::new(source, None, crate::Limits::default(), 4096, &pool, 1, 1).unwrap();
    let mut config = None;
    for _ in 0..10_000 {
        match tracks
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                config = Some(value);
                break;
            }
            other => panic!("config {other:?}"),
        }
    }
    let cue = |ticks, track, cluster| Cue {
        ticks,
        track,
        cluster,
        relative: None,
        block: None,
        codec_state: 0,
    };
    let cues = [cue(0, 1, 100), cue(u64::MAX, 1, 200), cue(u64::MAX, 2, 300)];
    let mut plan =
        SeekPlanJob::new(config.unwrap(), 0, 0, true, 3, 100, RecoveryAudio::Overlap).unwrap();
    let mut result = None;
    for _ in 0..1000 {
        match plan
            .step(&cues, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("plan {other:?}"),
        }
    }
    let result = result.unwrap();
    assert_eq!(result.preroll_ns, 10);
    assert_eq!(result.discard_until_ns, 0);
    assert!(matches!(
        result.landing,
        LandingPlan::Cue {
            cue: Cue { ticks: 0, .. },
            whole_cluster: true,
            scan_after_validation: None
        }
    ));
}
