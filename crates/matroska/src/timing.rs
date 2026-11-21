//! Matroska source time.
//!
//! Container time never loses precision to satisfy another API's clock grid. The
//! packet clock FFmpeg reports is a separate policy,
//! [`compat::clock`](crate::compat::clock).
use crate::{Error, Result};

/// Exact rational nanoseconds. Denominators are bounded by a block's frame count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactTime {
    numerator: i128,
    denominator: u16,
}

impl ExactTime {
    /// `numerator / denominator` nanoseconds in lowest terms. Fails with
    /// [`Error::Invalid`] for a zero denominator.
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
    /// The signed numerator of the reduced fraction, in nanoseconds.
    pub fn numerator(self) -> i128 {
        self.numerator
    }
    /// The denominator of the reduced fraction, at least 1.
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
    /// Round once, to the nearest nanosecond with ties upward, as a duration. Fails
    /// with [`Error::Invalid`] for a negative value and [`Error::Overflow`] past `u64`.
    pub fn rounded_duration_ns(self) -> Result<u64> {
        let n = u128::try_from(self.numerator).map_err(|_| Error::Invalid("negative duration"))?;
        if self.denominator == 1 {
            return u64::try_from(n).map_err(|_| Error::Overflow);
        }
        let d = u128::from(self.denominator);
        u64::try_from((n + d / 2) / d).map_err(|_| Error::Overflow)
    }
}

/// The source timing facts of one frame of a Block, from which
/// [`source`](Self::source) derives its exact time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockTiming {
    /// The Block's timestamp in Segment ticks: Cluster Timestamp plus the Block's
    /// signed relative timestamp.
    pub timestamp_ticks: i128,
    /// TimestampScale: nanoseconds per Segment tick, nonzero.
    pub scale_ns: u64,
    /// The track's CodecDelay in nanoseconds; 0 when absent.
    pub codec_delay_ns: u64,
    /// The track's DefaultDuration in nanoseconds; `None` when absent.
    pub default_duration_ns: Option<u64>,
    /// BlockDuration in Segment ticks, covering every frame of the Block; `None`
    /// when absent.
    pub duration_ticks: Option<u64>,
    /// Index of this frame in the Block's lace, below `frames`.
    pub frame: u16,
    /// Number of frames in the Block's lace; 1 when it is not laced.
    pub frames: u16,
}

/// Exact presentation time and duration of one frame, in rational nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketTiming {
    /// Presentation time; `None` when it cannot be derived, as for a later laced
    /// frame without a known duration.
    pub pts: Option<ExactTime>,
    /// Frame duration; `None` when unknown.
    pub duration: Option<ExactTime>,
}

impl BlockTiming {
    pub(crate) fn validate(self) -> Result<()> {
        if self.scale_ns == 0 || self.frames == 0 || self.frame >= self.frames {
            return Err(Error::Invalid("block timing parameters"));
        }
        Ok(())
    }

    /// RFC 9559 §§10.3.5, 11.2: delay is subtracted in nanoseconds.
    #[inline]
    pub(crate) fn source_base_ns(self) -> Result<i128> {
        self.validate()?;
        self.timestamp_ticks
            .checked_mul(i128::from(self.scale_ns))
            .and_then(|n| n.checked_sub(i128::from(self.codec_delay_ns)))
            .ok_or(Error::Overflow)
    }

    /// The exact source time (RFC 9559 §11.2): the Block timestamp in nanoseconds
    /// less CodecDelay, plus the earlier frames' share of BlockDuration, or of
    /// DefaultDuration when BlockDuration is absent. Fails with [`Error::Invalid`]
    /// for zero `scale_ns` or `frame` not below `frames`, and with
    /// [`Error::Overflow`] when a value does not fit.
    pub fn source(self) -> Result<PacketTiming> {
        let base = self.source_base_ns()?;
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
}
