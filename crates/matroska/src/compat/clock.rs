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
        let ticks = block_start_ticks(
            timing.timestamp_ticks,
            timing.codec_delay_ns,
            timing.scale_ns,
        )?
        .checked_add(start)
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

#[inline]
pub(crate) fn block_start_ticks(ticks: i128, delay_ns: u64, scale_ns: u64) -> Result<i128> {
    if delay_ns == 0 {
        return Ok(ticks);
    }
    let delay_ticks = match delay_ns.checked_add(scale_ns / 2) {
        Some(value) => i128::from(value / scale_ns),
        None => (i128::from(delay_ns) + i128::from(scale_ns / 2)) / i128::from(scale_ns),
    };
    ticks.checked_sub(delay_ticks).ok_or(Error::Overflow)
}

/// The common timestamp numerator fits a machine divide. Keep the wide path
/// for large defaults and laces whose tick product does not fit in 64 bits.
fn divide_ticks(numerator: i128, denominator: u64) -> i128 {
    match u64::try_from(numerator) {
        Ok(numerator) => i128::from(numerator / denominator),
        Err(_) => numerator / i128::from(denominator),
    }
}
