//! Reference demux signed/null field representation, never a source clock type.
use triergon_core::{Error, Result, Rounding, Time, TimeBase};

/// A usable timebase from the source declaration, when available.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct SourceTimeBase(Option<TimeBase>);

impl SourceTimeBase {
    pub const UNAVAILABLE: Self = Self(None);

    pub const fn get(self) -> Option<TimeBase> {
        self.0
    }

    pub fn require(self) -> Result<TimeBase> {
        self.0.ok_or(Error::Invalid("zero media/movie timescale"))
    }
}

impl From<TimeBase> for SourceTimeBase {
    fn from(base: TimeBase) -> Self {
        Self(Some(base))
    }
}

impl std::fmt::Debug for SourceTimeBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(base) => base.fmt(f),
            None => f.write_str("Unavailable"),
        }
    }
}

/// Observation provenance for the separately exposed demux stream clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaClockCause {
    SourceScale,
    ObservedZeroScaleWord,
    /// Only this observed all-ones word is supported. Other out-of-range
    /// denominators remain refused; no general fallback timebase is guessed.
    ObservedAllOnesScaleWord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaClock {
    pub source_base: SourceTimeBase,
    pub demux_base: TimeBase,
    pub cause: MediaClockCause,
}
impl MediaClock {
    pub fn for_zero_scale(_profile: crate::RunRecoveryProfile) -> Result<Self> {
        Ok(Self {
            source_base: SourceTimeBase::UNAVAILABLE,
            demux_base: TimeBase::new(1, 1)?,
            cause: MediaClockCause::ObservedZeroScaleWord,
        })
    }
    pub fn from_source(source_base: TimeBase) -> Result<Self> {
        if source_base.numerator() == 1 && source_base.denominator() == u64::from(u32::MAX) {
            return Ok(Self {
                source_base: source_base.into(),
                demux_base: TimeBase::new(1, 1)?,
                cause: MediaClockCause::ObservedAllOnesScaleWord,
            });
        }
        if source_base.denominator() > u64::from(i32::MAX.unsigned_abs()) {
            return Err(Error::Unsupported(
                "native unobserved large media timescale",
            ));
        }
        Ok(Self {
            source_base: source_base.into(),
            demux_base: source_base,
            cause: MediaClockCause::SourceScale,
        })
    }
}

/// Demux scheduling state is independent of source-knownness and API fields.
/// Unorderable heads retain the incumbent in the reference catalogue-order fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulingClock {
    SingleTrack,
    /// Explicit from-start nonseekable operation selects by actual byte address.
    NonseekableSourcePosition(u64),
    ExactMicroseconds(i64),
    ApiSentinel,
    RescaleOverflow {
        exact_microseconds: Option<i128>,
    },
}
impl SchedulingClock {
    pub fn from_demux_clock(field: ClockField, base: TimeBase) -> Result<Self> {
        let Some(ticks) = field.api_ticks() else {
            return Ok(Self::ApiSentinel);
        };
        let microseconds = TimeBase::new(1, 1_000_000)?;
        match base.rescale(ticks, microseconds, Rounding::NearestAway) {
            Ok(i64::MIN) => {
                return Err(Error::Unsupported(
                    "native exact scheduling sentinel collision unreviewed",
                ));
            }
            Ok(value) => return Ok(Self::ExactMicroseconds(value)),
            Err(Error::Overflow) => {}
            Err(error) => return Err(error),
        }
        // Keep the wide diagnostic value when the scheduling clock itself
        // cannot fit in the demux signed field.
        let time = Time::from_ticks(i128::from(ticks), base)?;
        match time.to_ticks(microseconds, Rounding::NearestAway) {
            Ok(value) => Ok(Self::RescaleOverflow {
                exact_microseconds: Some(value),
            }),
            Err(Error::Overflow) => Ok(Self::RescaleOverflow {
                exact_microseconds: None,
            }),
            Err(error) => Err(error),
        }
    }
}

/// A demux API field has a raw representation even when its API value is null.
/// `exact_ticks` is the format-demux projection input (which can be
/// derived); source Sample/Time knownness and origin provenance remain separate.
/// Supported by demux boundary observations, including wide-tfdt cases. Not
/// ISO source semantics or a generic wrap rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockField {
    pub exact_ticks: i128,
    pub demux_bits: u64,
}

impl ClockField {
    pub fn from_exact_ticks(ticks: i128) -> Result<Self> {
        let bits = ticks.rem_euclid(1i128 << 64);
        Ok(Self {
            exact_ticks: ticks,
            demux_bits: u64::try_from(bits).map_err(|_| Error::Overflow)?,
        })
    }
    pub fn signed_ticks(self) -> i64 {
        i64::from_be_bytes(self.demux_bits.to_be_bytes())
    }
    /// This null is an API sentinel, not an absent source timestamp.
    pub fn api_ticks(self) -> Option<i64> {
        let value = self.signed_ticks();
        (value != i64::MIN).then_some(value)
    }
}
