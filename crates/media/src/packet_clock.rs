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
            Some(duration) => i128::from(duration) * i128::from(timing.frames) / scale,
            None => 0,
        },
    };
    let (start, duration, previous_duration) = if timing.frames == 1 {
        (0, total_ticks, 1)
    } else {
        let boundary = |n: u16| total_ticks * i128::from(n) / i128::from(timing.frames);
        let start = boundary(timing.frame);
        let duration = boundary(timing.frame + 1) - start;
        let previous = if timing.frame > 0 {
            start - boundary(timing.frame - 1)
        } else {
            1
        };
        (start, duration, previous)
    };
    // Negative unadjusted block time has no packet timestamp.
    // A zero-duration preceding lace also terminates inferred timestamps.
    let pts = if timing.timestamp_ticks < 0
        || previous_duration == 0
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
