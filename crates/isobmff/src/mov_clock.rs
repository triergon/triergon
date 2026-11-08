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
        // The exact clock in microseconds, rounded to nearest with ties away from
        // zero; `None` when it does not fit `i128`. The scaled magnitude fits 128
        // bits for every source timescale; wider products go through exact time.
        let exact = match u128::from(ticks.unsigned_abs())
            .checked_mul(u128::from(base.numerator()) * 1_000_000)
        {
            Some(scaled) => {
                let magnitude = nearest_away(scaled, base.denominator());
                if ticks < 0 {
                    0i128.checked_sub_unsigned(magnitude)
                } else {
                    i128::try_from(magnitude).ok()
                }
            }
            None => match Time::from_ticks(i128::from(ticks), base)?
                .to_ticks(TimeBase::new(1, 1_000_000)?, Rounding::NearestAway)
            {
                Ok(value) => Some(value),
                Err(Error::Overflow) => None,
                Err(error) => return Err(error),
            },
        };
        // Keep the wide diagnostic value when the scheduling clock itself does
        // not fit the demux signed field.
        match exact.map(i64::try_from) {
            Some(Ok(i64::MIN)) => Err(Error::Unsupported(
                "native exact scheduling sentinel collision unreviewed",
            )),
            Some(Ok(value)) => Ok(Self::ExactMicroseconds(value)),
            Some(Err(_)) | None => Ok(Self::RescaleOverflow {
                exact_microseconds: exact,
            }),
        }
    }
}

/// `value / divisor` rounded to the nearest integer, ties away from zero.
///
/// A divisor below 2^32 divides in 32-bit limbs: each partial remainder is
/// below the divisor, so it and the next limb fit `u64`. This keeps 128-bit
/// division, a bit-serial loop on 32-bit targets, off the per-packet path.
fn nearest_away(value: u128, divisor: u64) -> u128 {
    let (quotient, remainder) = if let Ok(value) = u64::try_from(value) {
        (u128::from(value / divisor), value % divisor)
    } else if divisor <= u64::from(u32::MAX) {
        [96, 64, 32, 0]
            .into_iter()
            .fold((0, 0), |(quotient, remainder): (u128, u64), shift| {
                let partial = remainder << 32 | u64::from((value >> shift) as u32);
                (
                    quotient << 32 | u128::from(partial / divisor),
                    partial % divisor,
                )
            })
    } else {
        let divisor = u128::from(divisor);
        (value / divisor, (value % divisor) as u64)
    };
    quotient + u128::from(remainder >= divisor - remainder)
}

#[test]
fn nearest_away_matches_wide_division() {
    let values = [
        0,
        1,
        u128::from(u64::MAX),
        u128::from(u64::MAX) + 1,
        u128::from(i64::MAX.unsigned_abs()) * 1_000_000,
        u128::MAX / 3,
        u128::MAX,
    ];
    let divisors = [
        1,
        2,
        3,
        1_000,
        90_000,
        u64::from(u32::MAX),
        1 << 32,
        u64::MAX,
    ];
    let expected = |value: u128, divisor: u64| {
        let wide = u128::from(divisor);
        let remainder = value % wide;
        value / wide + u128::from(remainder >= wide - remainder)
    };
    for divisor in divisors {
        let wide = u128::from(divisor);
        // Ties and their neighbours: q·d + d/2 is an exact tie for even divisors.
        let ties = [1, 7, u128::from(u32::MAX), u128::MAX / wide - 1]
            .into_iter()
            .flat_map(|quotient| {
                let base = quotient * wide + wide / 2;
                [base - 1, base, base + 1]
            });
        for value in values.into_iter().chain(ties) {
            assert_eq!(
                nearest_away(value, divisor),
                expected(value, divisor),
                "{value} / {divisor}"
            );
        }
    }
}

#[test]
fn scheduling_keeps_signed_rounding_and_wide_clock_boundaries() {
    let half_microsecond = TimeBase::new(1, 2_000_000).unwrap();
    for (ticks, expected) in [(-3, -2), (-1, -1), (0, 0), (1, 1), (3, 2)] {
        let field = ClockField::from_exact_ticks(ticks).unwrap();
        assert_eq!(
            SchedulingClock::from_demux_clock(field, half_microsecond).unwrap(),
            SchedulingClock::ExactMicroseconds(expected)
        );
    }
    let largest = ClockField::from_exact_ticks(i128::from(i64::MAX)).unwrap();
    assert_eq!(
        SchedulingClock::from_demux_clock(largest, TimeBase::new(1, 1_000_000).unwrap()).unwrap(),
        SchedulingClock::ExactMicroseconds(i64::MAX)
    );
    let collision = ClockField::from_exact_ticks(i128::from(i64::MIN / 2)).unwrap();
    assert!(matches!(
        SchedulingClock::from_demux_clock(collision, TimeBase::new(1, 500_000).unwrap()),
        Err(Error::Unsupported(_))
    ));
    // A zero or all-ones timescale demuxes in seconds: past about 9.22e12 ticks the
    // clock no longer fits the signed field and keeps its exact value.
    let seconds = TimeBase::new(1, 1).unwrap();
    for ticks in [9_300_000_000_000_i64, -9_300_000_000_000, i64::MAX] {
        let field = ClockField::from_exact_ticks(i128::from(ticks)).unwrap();
        assert_eq!(
            SchedulingClock::from_demux_clock(field, seconds).unwrap(),
            SchedulingClock::RescaleOverflow {
                exact_microseconds: Some(i128::from(ticks) * 1_000_000),
            }
        );
    }
    // A product beyond 128 bits goes through exact time, which cannot hold it either.
    let wide = TimeBase::new(u64::MAX, 1).unwrap();
    let field = ClockField::from_exact_ticks(i128::from(i64::MAX)).unwrap();
    assert_eq!(
        SchedulingClock::from_demux_clock(field, wide).unwrap(),
        SchedulingClock::RescaleOverflow {
            exact_microseconds: None,
        }
    );
}

#[test]
fn scheduling_matches_exact_time() {
    let microseconds = TimeBase::new(1, 1_000_000).unwrap();
    for (numerator, denominator) in [(1, 90_000), (1, 1_001), (1001, 30_000), (1, 7), (3, 2)] {
        let base = TimeBase::new(numerator, denominator).unwrap();
        for ticks in [
            -(1_i64 << 62),
            -12_345_679,
            -1,
            1,
            45_000,
            1 << 40,
            i64::MAX,
        ] {
            let exact = Time::from_ticks(i128::from(ticks), base)
                .unwrap()
                .to_ticks(microseconds, Rounding::NearestAway)
                .unwrap();
            let field = ClockField::from_exact_ticks(i128::from(ticks)).unwrap();
            let expected = match i64::try_from(exact) {
                Ok(value) => SchedulingClock::ExactMicroseconds(value),
                Err(_) => SchedulingClock::RescaleOverflow {
                    exact_microseconds: Some(exact),
                },
            };
            assert_eq!(
                SchedulingClock::from_demux_clock(field, base).unwrap(),
                expected,
                "{ticks} at {numerator}/{denominator}"
            );
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
