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
        if denominator == 1 {
            return Ok(Self {
                numerator,
                denominator,
            });
        }
        if numerator == 0 {
            return Ok(Self {
                numerator: 0,
                denominator: 1,
            });
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
        if self.denominator == 1 {
            return i64::try_from(self.numerator).map_err(|_| Error::Overflow);
        }
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
        if self.denominator == 1 {
            return u64::try_from(n).map_err(|_| Error::Overflow);
        }
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

    /// RFC 9559 §§10.3.5, 11.2: delay is subtracted in nanoseconds.
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
            // A validated single frame always has index zero. Its integral
            // duration does not contribute to its presentation timestamp.
            _ if self.frames == 1 => Some(ExactTime::new(base, 1)?),
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
        let delay = if self.codec_delay_ns == 0 {
            0
        } else {
            match self.codec_delay_ns.checked_add(self.scale_ns / 2) {
                Some(value) => i128::from(value / self.scale_ns),
                None => (i128::from(self.codec_delay_ns) + scale / 2) / scale,
            }
        };
        let total_ticks = match self.duration_ticks.filter(|d| *d != 0) {
            Some(d) => i128::from(d),
            None => match self.default_duration_ns {
                Some(duration) if self.frames == 1 => i128::from(duration / self.scale_ns),
                Some(duration) => i128::from(duration) * i128::from(self.frames) / scale,
                None => 0,
            },
        };
        let (start, duration, previous_duration) = if self.frames == 1 {
            (0, total_ticks, 1)
        } else {
            let boundary = |n: u16| total_ticks * i128::from(n) / i128::from(self.frames);
            let start = boundary(self.frame);
            let duration = boundary(self.frame + 1) - start;
            let previous = if self.frame > 0 {
                start - boundary(self.frame - 1)
            } else {
                1
            };
            (start, duration, previous)
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
        let time = wide.ffmpeg9().unwrap();
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
        let time = zero.ffmpeg9().unwrap();
        assert_eq!(time.pts.unwrap().rounded_ns().unwrap(), 0);
        assert_eq!(time.duration, None);
        assert_eq!(
            BlockTiming {
                timestamp_ticks: -1,
                ..zero
            }
            .ffmpeg9()
            .unwrap()
            .pts,
            None
        );
        assert!(matches!(
            BlockTiming {
                timestamp_ticks: i128::MAX,
                ..wide
            }
            .ffmpeg9(),
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
