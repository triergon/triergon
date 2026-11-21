//! Exact block and packet clocks: integral times and their rounding, the packet
//! clock with codec delay and its overflow, signed block times, fractional laces
//! and lace tick boundaries.
use triergon_matroska::Error;
use triergon_matroska::compat::clock::packet_timing;
use triergon_matroska::timing::{BlockTiming, ExactTime, PacketTiming};

#[test]
fn integral_exact_time_keeps_signed_extremes_unknowns_and_rounding_errors() {
    for value in [
        i128::MIN,
        i128::from(i64::MIN),
        -1,
        0,
        1,
        i128::from(i64::MAX),
        i128::MAX,
    ] {
        let time = ExactTime::new(value, 1).unwrap();
        assert_eq!(time.numerator(), value);
        assert_eq!(time.denominator(), 1);
        assert_eq!(time.rounded_ns().ok(), i64::try_from(value).ok());
        assert_eq!(time.rounded_duration_ns().ok(), u64::try_from(value).ok());
    }
    assert_eq!(
        ExactTime::new(0, u16::MAX).unwrap(),
        ExactTime::new(0, 1).unwrap()
    );
    assert!(ExactTime::new(1, 0).is_err());
    assert_eq!(ExactTime::new(-3, 2).unwrap().rounded_ns().unwrap(), -2);
    assert_eq!(ExactTime::new(3, 2).unwrap().rounded_ns().unwrap(), 2);
    assert!(
        ExactTime::new(-1, 1)
            .unwrap()
            .rounded_duration_ns()
            .is_err()
    );
}

#[test]
fn non_laced_packet_clock_keeps_wide_delay_fallback_zero_and_overflow() {
    let wide = BlockTiming {
        timestamp_ticks: 3,
        scale_ns: u64::MAX,
        codec_delay_ns: u64::MAX,
        default_duration_ns: None,
        duration_ticks: Some(1),
        frame: 0,
        frames: 1,
    };
    let time = packet_timing(wide).unwrap();
    assert_eq!(
        time.pts.unwrap(),
        ExactTime::new(2 * i128::from(u64::MAX), 1).unwrap()
    );
    assert_eq!(
        time.duration.unwrap(),
        ExactTime::new(i128::from(u64::MAX), 1).unwrap()
    );
    let zero = BlockTiming {
        timestamp_ticks: 0,
        scale_ns: 1,
        codec_delay_ns: 0,
        default_duration_ns: None,
        duration_ticks: Some(0),
        frame: 0,
        frames: 1,
    };
    let time = packet_timing(zero).unwrap();
    assert_eq!(time.pts.unwrap().rounded_ns().unwrap(), 0);
    assert_eq!(time.duration, None);
    assert_eq!(
        packet_timing(BlockTiming {
            timestamp_ticks: -1,
            ..zero
        })
        .unwrap()
        .pts,
        None
    );
    assert!(matches!(
        packet_timing(BlockTiming {
            timestamp_ticks: i128::MAX,
            ..wide
        }),
        Err(Error::Overflow)
    ));
}

fn block() -> BlockTiming {
    BlockTiming {
        timestamp_ticks: 21,
        scale_ns: 1_000_000,
        codec_delay_ns: 21_333_333,
        default_duration_ns: Some(21_333_333),
        duration_ticks: None,
        frame: 0,
        frames: 1,
    }
}

#[test]
fn nanosecond_source_and_packet_clock_are_separate() {
    let b = block();
    assert_eq!(
        b.source().unwrap().pts.unwrap().rounded_ns().unwrap(),
        -333_333
    );
    assert_eq!(
        packet_timing(b).unwrap().pts.unwrap().rounded_ns().unwrap(),
        0
    );
    assert_eq!(
        b.source().unwrap().duration.unwrap().rounded_ns().unwrap(),
        21_333_333
    );
    assert_eq!(
        packet_timing(b)
            .unwrap()
            .duration
            .unwrap()
            .rounded_ns()
            .unwrap(),
        21_000_000
    );
}

#[test]
fn negative_block_time_is_retained_in_source() {
    let b = BlockTiming {
        timestamp_ticks: -4,
        codec_delay_ns: 0,
        ..block()
    };
    assert_eq!(
        b.source().unwrap().pts.unwrap().rounded_ns().unwrap(),
        -4_000_000
    );
    assert_eq!(packet_timing(b).unwrap().pts, None);
}

#[test]
fn fractional_lace_duration_remains_exact() {
    let b = BlockTiming {
        timestamp_ticks: 0,
        codec_delay_ns: 0,
        duration_ticks: Some(1),
        frame: 2,
        frames: 3,
        ..block()
    };
    let t = b.source().unwrap();
    assert_eq!(t.pts.unwrap(), ExactTime::new(2_000_000, 3).unwrap());
    assert_eq!(t.duration.unwrap(), ExactTime::new(1_000_000, 3).unwrap());
    assert_eq!(ExactTime::new(-3, 2).unwrap().rounded_ns().unwrap(), -2);
}

#[test]
fn lace_tick_boundaries_match_wide_floor_division() {
    for frames in [2, 3, 256, u16::MAX] {
        for ticks in [0, 1, 2, 31, 256, u64::MAX] {
            for frame in [0, 1, frames - 1] {
                let timing = BlockTiming {
                    timestamp_ticks: 0,
                    scale_ns: 1,
                    codec_delay_ns: 0,
                    default_duration_ns: None,
                    duration_ticks: Some(ticks),
                    frame,
                    frames,
                };
                let boundary = |n| i128::from(ticks) * i128::from(n) / i128::from(frames);
                let start = boundary(frame);
                let duration = boundary(frame + 1) - start;
                let expected = PacketTiming {
                    pts: (frame == 0 || ticks >= u64::from(frames))
                        .then(|| ExactTime::new(start, 1).unwrap()),
                    duration: (duration != 0).then(|| ExactTime::new(duration, 1).unwrap()),
                };
                assert_eq!(packet_timing(timing).unwrap(), expected);
            }
        }
    }
}
