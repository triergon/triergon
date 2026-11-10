//! The packet clock FFmpeg's Matroska demuxer reports. Documented on
//! [`compat::clock`](crate::compat::clock).
use crate::{
    Error, Result,
    timing::{BlockTiming, ExactTime, PacketTiming},
};

/// The packet timestamp and duration FFmpeg reports for one block or lace frame,
/// in whole Segment ticks.
///
/// CodecDelay is rounded to ticks and subtracted from the block timestamp; a lace's
/// duration is split at integer tick boundaries. A block with a negative timestamp,
/// a lace frame after a zero-duration one and a later lace frame of a block shorter
/// than one tick per frame have no timestamp; a zero duration is `None`. The exact
/// source values stay available from [`BlockTiming::source`].
pub fn packet_timing(timing: BlockTiming) -> Result<PacketTiming> {
    timing.validate()?;
    let scale = i128::from(timing.scale_ns);
    let delay = if timing.codec_delay_ns == 0 {
        0
    } else {
        match timing.codec_delay_ns.checked_add(timing.scale_ns / 2) {
            Some(value) => i128::from(value / timing.scale_ns),
            None => (i128::from(timing.codec_delay_ns) + scale / 2) / scale,
        }
    };
    let total_ticks = match timing.duration_ticks.filter(|d| *d != 0) {
        Some(d) => i128::from(d),
        None => match timing.default_duration_ns {
            Some(duration) if timing.frames == 1 => i128::from(duration / timing.scale_ns),
            Some(duration) => divide_ticks(
                i128::from(duration) * i128::from(timing.frames),
                timing.scale_ns,
            ),
            None => 0,
        },
    };
    let (start, duration) = if timing.frames == 1 || total_ticks == 0 {
        (0, total_ticks)
    } else {
        let frames = u64::from(timing.frames);
        let boundary = |n: u16| divide_ticks(total_ticks * i128::from(n), frames);
        let start = boundary(timing.frame);
        (start, boundary(timing.frame + 1) - start)
    };
    // A negative block time or less than one tick per frame suppresses inferred
    // lace times. Otherwise consecutive tick boundaries differ by at least one.
    let pts = if timing.timestamp_ticks < 0
        || (timing.frame > 0 && total_ticks < i128::from(timing.frames))
    {
        None
    } else {
        let ticks = timing
            .timestamp_ticks
            .checked_sub(delay)
            .and_then(|n| n.checked_add(start))
            .ok_or(Error::Overflow)?;
        Some(ExactTime::new(
            ticks.checked_mul(scale).ok_or(Error::Overflow)?,
            1,
        )?)
    };
    Ok(PacketTiming {
        pts,
        duration: if duration == 0 {
            None
        } else {
            Some(ExactTime::new(
                duration.checked_mul(scale).ok_or(Error::Overflow)?,
                1,
            )?)
        },
    })
}

/// The common timestamp numerator fits a machine divide. Keep the wide path
/// for large defaults and laces whose tick product does not fit in 64 bits.
fn divide_ticks(numerator: i128, denominator: u64) -> i128 {
    match u64::try_from(numerator) {
        Ok(numerator) => i128::from(numerator / denominator),
        Err(_) => numerator / i128::from(denominator),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
