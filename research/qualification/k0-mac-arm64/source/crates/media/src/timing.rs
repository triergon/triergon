//! Matroska source time and explicit FFmpeg packet-clock compatibility.
//!
//! Container time never loses precision to satisfy another API's clock grid.
use crate::{Error, Result};

/// Exact rational nanoseconds. Denominators are bounded by a block's frame count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactTime {
    numerator: i128,
    denominator: u16,
}

impl ExactTime {
    pub fn new(numerator: i128, denominator: u16) -> Result<Self> {
        if denominator == 0 {
            return Err(Error::Invalid("zero time denominator"));
        }
        let mut a = numerator.unsigned_abs();
        let mut b = u128::from(denominator);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        let divisor = a as i128;
        Ok(Self {
            numerator: numerator / divisor,
            denominator: (i128::from(denominator) / divisor) as u16,
        })
    }
    pub fn numerator(self) -> i128 {
        self.numerator
    }
    pub fn denominator(self) -> u16 {
        self.denominator
    }
    /// Round once, to the nearest nanosecond, with ties away from zero.
    pub fn rounded_ns(self) -> Result<i64> {
        let d = i128::from(self.denominator);
        let q = self.numerator / d;
        let r = self.numerator % d;
        let v = if r.abs() * 2 >= d {
            q.checked_add(self.numerator.signum())
                .ok_or(Error::Overflow)?
        } else {
            q
        };
        i64::try_from(v).map_err(|_| Error::Overflow)
    }
    pub fn rounded_duration_ns(self) -> Result<u64> {
        let n = u128::try_from(self.numerator).map_err(|_| Error::Invalid("negative duration"))?;
        let d = u128::from(self.denominator);
        u64::try_from((n + d / 2) / d).map_err(|_| Error::Overflow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockTiming {
    pub timestamp_ticks: i128,
    pub scale_ns: u64,
    pub codec_delay_ns: u64,
    pub default_duration_ns: Option<u64>,
    pub duration_ticks: Option<u64>,
    pub frame: u16,
    pub frames: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketTiming {
    pub pts: Option<ExactTime>,
    pub duration: Option<ExactTime>,
}

impl BlockTiming {
    fn validate(self) -> Result<()> {
        if self.scale_ns == 0 || self.frames == 0 || self.frame >= self.frames {
            return Err(Error::Invalid("block timing parameters"));
        }
        Ok(())
    }

    /// RFC 9559 §§10.3.5, 11.4: delay is subtracted in nanoseconds.
    pub fn source(self) -> Result<PacketTiming> {
        self.validate()?;
        let base = self
            .timestamp_ticks
            .checked_mul(i128::from(self.scale_ns))
            .and_then(|n| n.checked_sub(i128::from(self.codec_delay_ns)))
            .ok_or(Error::Overflow)?;
        let duration = if let Some(ticks) = self.duration_ticks {
            Some(ExactTime::new(
                i128::from(ticks)
                    .checked_mul(i128::from(self.scale_ns))
                    .ok_or(Error::Overflow)?,
                self.frames,
            )?)
        } else {
            self.default_duration_ns
                .map(|d| ExactTime::new(i128::from(d), 1))
                .transpose()?
        };
        let pts = match duration {
            Some(d) => {
                let value = base
                    .checked_mul(i128::from(d.denominator))
                    .and_then(|n| {
                        d.numerator
                            .checked_mul(i128::from(self.frame))
                            .and_then(|step| n.checked_add(step))
                    })
                    .ok_or(Error::Overflow)?;
                Some(ExactTime::new(value, d.denominator)?)
            }
            None if self.frame == 0 => Some(ExactTime::new(base, 1)?),
            None => None,
        };
        Ok(PacketTiming { pts, duration })
    }

    /// Explicit compatibility with the pinned n9.0 packet API, in Segment ticks.
    /// Source precision, including pre-zero PTS, remains separately available.
    pub fn ffmpeg9(self) -> Result<PacketTiming> {
        self.validate()?;
        let scale = i128::from(self.scale_ns);
        let delay = (i128::from(self.codec_delay_ns) + scale / 2) / scale;
        let total_ticks = match self.duration_ticks.filter(|d| *d != 0) {
            Some(d) => i128::from(d),
            None => {
                i128::from(self.default_duration_ns.unwrap_or(0)) * i128::from(self.frames) / scale
            }
        };
        let boundary = |n: u16| total_ticks * i128::from(n) / i128::from(self.frames);
        let start = boundary(self.frame);
        let duration = boundary(self.frame + 1) - start;
        let previous_duration = if self.frame > 0 {
            start - boundary(self.frame - 1)
        } else {
            1
        };
        // Negative unadjusted block time has no packet timestamp in n9.0.
        // A zero-duration preceding lace also terminates inferred timestamps.
        let pts = if self.timestamp_ticks < 0
            || previous_duration == 0
            || (self.frame > 0 && total_ticks < i128::from(self.frames))
        {
            None
        } else {
            let ticks = self
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(b.ffmpeg9().unwrap().pts.unwrap().rounded_ns().unwrap(), 0);
        assert_eq!(
            b.source().unwrap().duration.unwrap().rounded_ns().unwrap(),
            21_333_333
        );
        assert_eq!(
            b.ffmpeg9().unwrap().duration.unwrap().rounded_ns().unwrap(),
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
        assert_eq!(b.ffmpeg9().unwrap().pts, None);
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
}
