//! Real authored AVC/AAC containers and independent wire/native observations.
//! RFC9559 specifies source Cue fields and recommends time order, but does not
//! choose an equal-time winner. These guards cover the observed cross-Cluster
//! replay policy, without modeling an unstable sort or an internal native index.
use super::*;
use crate::matroska::typed::seek::{LandingPlan, SeekPlanJob};
use std::io::Cursor;

struct Case {
    name: &'static str,
    bytes: &'static [u8],
    ties: usize,
    roll: u64,
}
const CASES: &[Case] = &[
    Case {
        name: "ties-03-physical-preroll-0",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-03-physical-preroll-0.mkv"
        )),
        ties: 3,
        roll: 0,
    },
    Case {
        name: "ties-03-physical-preroll-500000000",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-03-physical-preroll-500000000.mkv"
        )),
        ties: 3,
        roll: 500000000,
    },
    Case {
        name: "ties-03-reverse-ties-preroll-0",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-03-reverse-ties-preroll-0.mkv"
        )),
        ties: 3,
        roll: 0,
    },
    Case {
        name: "ties-03-reverse-ties-preroll-500000000",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-03-reverse-ties-preroll-500000000.mkv"
        )),
        ties: 3,
        roll: 500000000,
    },
    Case {
        name: "ties-03-rotate-ties-preroll-0",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-03-rotate-ties-preroll-0.mkv"
        )),
        ties: 3,
        roll: 0,
    },
    Case {
        name: "ties-03-rotate-ties-preroll-500000000",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-03-rotate-ties-preroll-500000000.mkv"
        )),
        ties: 3,
        roll: 500000000,
    },
    Case {
        name: "ties-03-audio-position-first-preroll-0",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-03-audio-position-first-preroll-0.mkv"
        )),
        ties: 3,
        roll: 0,
    },
    Case {
        name: "ties-03-audio-position-first-preroll-500000000",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-03-audio-position-first-preroll-500000000.mkv"
        )),
        ties: 3,
        roll: 500000000,
    },
    Case {
        name: "ties-24-physical-preroll-0",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-24-physical-preroll-0.mkv"
        )),
        ties: 24,
        roll: 0,
    },
    Case {
        name: "ties-24-physical-preroll-500000000",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-24-physical-preroll-500000000.mkv"
        )),
        ties: 24,
        roll: 500000000,
    },
    Case {
        name: "ties-24-reverse-ties-preroll-0",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-24-reverse-ties-preroll-0.mkv"
        )),
        ties: 24,
        roll: 0,
    },
    Case {
        name: "ties-24-reverse-ties-preroll-500000000",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-24-reverse-ties-preroll-500000000.mkv"
        )),
        ties: 24,
        roll: 500000000,
    },
    Case {
        name: "ties-24-rotate-ties-preroll-0",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-24-rotate-ties-preroll-0.mkv"
        )),
        ties: 24,
        roll: 0,
    },
    Case {
        name: "ties-24-rotate-ties-preroll-500000000",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-24-rotate-ties-preroll-500000000.mkv"
        )),
        ties: 24,
        roll: 500000000,
    },
    Case {
        name: "ties-24-audio-position-first-preroll-0",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-24-audio-position-first-preroll-0.mkv"
        )),
        ties: 24,
        roll: 0,
    },
    Case {
        name: "ties-24-audio-position-first-preroll-500000000",
        bytes: include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cue-ties/ties-24-audio-position-first-preroll-500000000.mkv"
        )),
        ties: 24,
        roll: 500000000,
    },
];
const POINTS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cue-ties/wire-cues.tsv"
));
const LANDINGS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cue-ties/native-landings.tsv"
));
const VIDEO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cue-ties/avc-packet.bin"
));
const AUDIO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cue-ties/aac-packet.bin"
));
const TARGETS: [i64; 7] = [
    0,
    250_000_000,
    750_000_000,
    1_000_000_000,
    1_250_000_000,
    1_750_000_000,
    2_500_000_000,
];

struct WirePoint {
    cue: crate::cues::Cue,
    cluster_index: usize,
}
struct Landing {
    cluster_index: usize,
    ticks: u64,
    packets: usize,
    from_start: bool,
}
fn wire_points(case: &Case) -> Vec<WirePoint> {
    POINTS
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            if fields[0] != case.name {
                return None;
            }
            Some(WirePoint {
                cue: crate::cues::Cue {
                    ticks: fields[2].parse().unwrap(),
                    track: fields[3].parse().unwrap(),
                    cluster: fields[4].parse().unwrap(),
                    relative: Some(fields[5].parse().unwrap()),
                    block: None,
                    codec_state: fields[6].parse().unwrap(),
                },
                cluster_index: fields[7].parse().unwrap(),
            })
        })
        .collect()
}
fn observed_landing(case: &Case, target: i64, playback: bool) -> Landing {
    // The no-preroll sister file has identical media/positions. Direct plain
    // seek_previous is a format policy check here, not a claimed native API run.
    let name = if playback {
        case.name.to_owned()
    } else {
        case.name.replace("preroll-500000000", "preroll-0")
    };
    let fields = LANDINGS
        .lines()
        .skip(1)
        .map(|line| line.split('\t').collect::<Vec<_>>())
        .find(|row| row[0] == name && row[1].parse::<i64>().unwrap() == target)
        .unwrap();
    Landing {
        cluster_index: fields[3].parse().unwrap(),
        ticks: fields[4].parse().unwrap(),
        packets: fields[5].parse().unwrap(),
        from_start: fields[6] == "1",
    }
}
fn configuration_from_fixture(case: &Case, pool: &Budget) -> configuration::Configuration {
    // Only source tracks are used from the existing blocking parser. Its Cues
    // vector and std unstable tie permutation never enter this expectation.
    let reader = crate::matroska::Demuxer::open_seekable_in(
        Cursor::new(case.bytes),
        crate::Limits::default(),
        pool,
    )
    .unwrap();
    let mut tracks = BudgetVec::new(pool);
    tracks.try_reserve_exact(reader.tracks().len()).unwrap();
    for track in reader.tracks() {
        tracks.try_push(track.clone()).unwrap();
    }
    let mut job = configuration::ConfigurationJob::from_validated_vec(tracks, pool, 7, 3);
    match job
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        ParseStep::Event(value) => value,
        other => panic!("configuration: {other:?}"),
    }
}
fn fields(cue: crate::cues::Cue) -> (u64, u64, u64, Option<u64>, Option<u64>, u64) {
    (
        cue.ticks,
        cue.track,
        cue.cluster,
        cue.relative,
        cue.block,
        cue.codec_state,
    )
}
#[test]
fn cue_ties_select_the_observed_earliest_cluster_from_real_wire_positions() {
    let cancel = AtomicBool::new(false);
    for case in CASES {
        let pool = Budget::new(4 * 1024 * 1024).unwrap();
        let configuration = configuration_from_fixture(case, &pool);
        let points = wire_points(case);
        let cues: Vec<_> = points.iter().map(|point| point.cue).collect();
        assert_eq!(points.len(), 4 * case.ties + 2);
        for target in TARGETS {
            for playback in [false, true] {
                for units in [1, 7, 8192] {
                    let expected = observed_landing(case, target, playback);
                    let expected_cue = points
                        .iter()
                        .find(|point| {
                            point.cue.track == 1 && point.cluster_index == expected.cluster_index
                        })
                        .unwrap()
                        .cue;
                    let mut job = SeekPlanJob::new(
                        configuration.clone(),
                        0,
                        target,
                        playback,
                        1_000_000,
                        points.iter().map(|point| point.cue.cluster).min().unwrap(),
                    )
                    .unwrap();
                    let mut result = None;
                    for _ in 0..100_000 {
                        let mut work = WorkBudget::new(units);
                        let step = job.step(&cues, &mut work, &cancel).unwrap();
                        assert!(work.spent() <= units);
                        match step {
                            ParseStep::Yield => (),
                            ParseStep::Event(value) => {
                                result = Some(value);
                                break;
                            }
                            other => panic!("plan: {other:?}"),
                        }
                    }
                    let result = result.unwrap();
                    assert_eq!(
                        (
                            result.requested_ns,
                            result.discard_until_ns,
                            result.preroll_ns
                        ),
                        (target, target, if playback { case.roll } else { 0 })
                    );
                    let LandingPlan::Cue {
                        cue, whole_cluster, ..
                    } = result.landing
                    else {
                        panic!("a validated real Cue exists");
                    };
                    assert_eq!(
                        fields(cue),
                        fields(expected_cue),
                        "{} target={target} playback={playback} work={units}",
                        case.name
                    );
                    assert_eq!(whole_cluster, playback);
                }
            }
        }
    }
}
#[test]
fn cue_ties_replay_the_complete_native_observed_suffix_fresh_and_after_eof() {
    let cancel = AtomicBool::new(false);
    for case in CASES {
        for target in TARGETS {
            for after_eof in [false, true] {
                let mut reader = crate::typed::BlockingReader::open_seekable(
                    Cursor::new(case.bytes),
                    crate::Limits::default(),
                )
                .unwrap();
                assert_eq!(reader.tracks().len(), 2);
                assert_eq!(reader.tracks()[0].codec_delay_ns, 0);
                assert_eq!(reader.tracks()[1].codec_delay_ns, 21_333_333);
                if after_eof {
                    while reader.next_packet(&cancel).unwrap().is_some() {}
                }
                let expected = observed_landing(case, target, true);
                let receipt = reader.seek_for_playback(0, target, &cancel).unwrap();
                assert_eq!(
                    (
                        receipt.actual_ns,
                        receipt.discard_until_ns,
                        receipt.preroll_ns,
                        receipt.replays_from_start,
                        receipt.used_index
                    ),
                    (
                        expected.ticks as i64 * 1_000_000,
                        target,
                        case.roll,
                        expected.from_start,
                        true
                    ),
                    "{} target={target} after_eof={after_eof}",
                    case.name
                );
                let mut count = 0;
                while let Some(typed) = reader.next_typed_packet(&cancel).unwrap() {
                    let packet = typed.packet;
                    let cluster = expected.cluster_index + count / 2;
                    let ticks = if cluster < case.ties {
                        0
                    } else if cluster < 2 * case.ties {
                        1000
                    } else {
                        2000
                    };
                    let audio = count % 2 == 0;
                    let delay = if audio { 21_333_333 } else { 0 };
                    assert_eq!(packet.track, if audio { 1 } else { 0 });
                    assert!(packet.keyframe);
                    assert!(packet.dts_ns.is_none());
                    assert_eq!(packet.data.as_ref(), if audio { AUDIO } else { VIDEO });
                    let source = packet.timing.source().unwrap();
                    let pts = source.pts.unwrap();
                    assert_eq!(
                        (pts.numerator(), pts.denominator()),
                        (i128::from(ticks) * 1_000_000 - delay, 1)
                    );
                    count += 1;
                }
                assert_eq!(
                    count, expected.packets,
                    "{} target={target} after_eof={after_eof}",
                    case.name
                );
            }
        }
    }
}
