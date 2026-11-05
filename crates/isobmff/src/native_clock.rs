//! Pinned native signed/null field representation, never a source clock type.
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

/// Observation provenance for the separately exposed native stream clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9MediaClockCause {
    SourceScale,
    ObservedZeroScaleWord,
    /// Only this observed all-ones word is supported. Other out-of-range
    /// denominators remain refused; no general fallback timebase is guessed.
    ObservedAllOnesScaleWord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9MediaClock {
    pub source_base: SourceTimeBase,
    pub native_base: TimeBase,
    pub cause: Mov9MediaClockCause,
}
impl Mov9MediaClock {
    pub fn for_pinned_zero_scale(_profile: crate::PinnedMovD32RegularRecoveryV1) -> Result<Self> {
        Ok(Self {
            source_base: SourceTimeBase::UNAVAILABLE,
            native_base: TimeBase::new(1, 1)?,
            cause: Mov9MediaClockCause::ObservedZeroScaleWord,
        })
    }
    pub fn from_source(source_base: TimeBase) -> Result<Self> {
        if source_base.numerator() == 1 && source_base.denominator() == u64::from(u32::MAX) {
            return Ok(Self {
                source_base: source_base.into(),
                native_base: TimeBase::new(1, 1)?,
                cause: Mov9MediaClockCause::ObservedAllOnesScaleWord,
            });
        }
        if source_base.denominator() > u64::from(i32::MAX.unsigned_abs()) {
            return Err(Error::Unsupported(
                "native unobserved large media timescale",
            ));
        }
        Ok(Self {
            source_base: source_base.into(),
            native_base: source_base,
            cause: Mov9MediaClockCause::SourceScale,
        })
    }
}

/// Native scheduling state is independent of source-knownness and API fields.
/// Unorderable heads retain the incumbent in the pinned catalogue-order fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9SchedulingClock {
    SingleTrack,
    /// Explicit from-start nonseekable operation selects by actual byte address.
    NonseekableSourcePosition(u64),
    ExactMicroseconds(i64),
    ApiSentinel,
    RescaleOverflow {
        exact_microseconds: Option<i128>,
    },
}
impl Mov9SchedulingClock {
    pub fn from_native(field: Mov9ClockField, base: TimeBase) -> Result<Self> {
        let Some(ticks) = field.api_ticks() else {
            return Ok(Self::ApiSentinel);
        };
        let time = Time::from_ticks(i128::from(ticks), base)?;
        let rounded = match time.to_ticks(TimeBase::new(1, 1_000_000)?, Rounding::NearestAway) {
            Ok(value) => value,
            Err(Error::Overflow) => {
                return Ok(Self::RescaleOverflow {
                    exact_microseconds: None,
                });
            }
            Err(error) => return Err(error),
        };
        match i64::try_from(rounded) {
            Ok(value) if value != i64::MIN => Ok(Self::ExactMicroseconds(value)),
            Ok(_) => Err(Error::Unsupported(
                "native exact scheduling sentinel collision unreviewed",
            )),
            Err(_) => Ok(Self::RescaleOverflow {
                exact_microseconds: Some(rounded),
            }),
        }
    }
}

/// A native API field has a raw representation even when its API value is null.
/// `exact_native_ticks` is the format-native projection input (which can be
/// derived); source Sample/Time knownness and origin provenance remain separate.
/// Supported by cp27 native boundary observations on the separately identified
/// Linux formats worker and the original Mac wide-tfdt observations. Not ISO
/// source semantics, a generic wrap rule, or qualification of either worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9ClockField {
    pub exact_native_ticks: i128,
    pub native_bits: u64,
}

impl Mov9ClockField {
    pub fn from_exact_native_ticks(ticks: i128) -> Result<Self> {
        let bits = ticks.rem_euclid(1i128 << 64);
        Ok(Self {
            exact_native_ticks: ticks,
            native_bits: u64::try_from(bits).map_err(|_| Error::Overflow)?,
        })
    }
    pub fn signed_ticks(self) -> i64 {
        i64::from_be_bytes(self.native_bits.to_be_bytes())
    }
    /// This null is an API sentinel, not an absent source timestamp.
    pub fn api_ticks(self) -> Option<i64> {
        let value = self.signed_ticks();
        (value != i64::MIN).then_some(value)
    }
}
