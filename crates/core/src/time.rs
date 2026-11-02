//! Exact reduced rational seconds, with separately requested rounded views.
use crate::wide::Wide;
use crate::{Error, Result};
use std::{
    cmp::Ordering,
    num::{NonZeroU64, NonZeroU128},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    TowardZero,
    AwayFromZero,
    Floor,
    Ceil,
    NearestAway,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeBase {
    numerator: NonZeroU64,
    denominator: NonZeroU64,
}

impl TimeBase {
    pub fn new(numerator: u64, denominator: u64) -> Result<Self> {
        if numerator == 0 || denominator == 0 {
            return Err(Error::Invalid("zero timebase"));
        }
        let divisor = gcd(u128::from(numerator), u128::from(denominator)) as u64;
        Ok(Self {
            numerator: NonZeroU64::new(numerator / divisor).ok_or(Error::Invalid("timebase"))?,
            denominator: NonZeroU64::new(denominator / divisor)
                .ok_or(Error::Invalid("timebase"))?,
        })
    }
    pub fn numerator(self) -> u64 {
        self.numerator.get()
    }
    pub fn denominator(self) -> u64 {
        self.denominator.get()
    }
    pub fn rescale(self, ticks: i64, to: Self, rounding: Rounding) -> Result<i64> {
        Time::from_ticks(i128::from(ticks), self)?.to_i64_ticks(to, rounding)
    }
}

/// Canonical exact source time. Unknown time is `Option<Time>`; no value is a
/// sentinel. Arithmetic reduces before checking the public representation's
/// bounds; a wider intermediate alone is not a reason to reject valid time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Time {
    numerator: i128,
    denominator: NonZeroU128,
}

#[inline]
fn gcd(mut a: u128, mut b: u128) -> u128 {
    if let (Ok(mut a), Ok(mut b)) = (u64::try_from(a), u64::try_from(b)) {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        return u128::from(a);
    }
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl Time {
    pub fn new(numerator: i128, denominator: u128) -> Result<Self> {
        if denominator == 0 {
            return Err(Error::Invalid("zero time denominator"));
        }
        let divisor = gcd(numerator.unsigned_abs(), denominator);
        Ok(Self {
            numerator: signed(numerator.unsigned_abs() / divisor, numerator < 0)?,
            denominator: NonZeroU128::new(denominator / divisor)
                .ok_or(Error::Invalid("time denominator"))?,
        })
    }
    pub fn from_ticks(ticks: i128, base: TimeBase) -> Result<Self> {
        let absolute = ticks.unsigned_abs();
        let base_denominator = base.denominator();
        let (quotient, denominator) = if let Ok(narrow) = u64::try_from(absolute) {
            // gcd is in 1..=base_denominator, so this conversion is exact.
            let divisor = gcd(u128::from(narrow), u128::from(base_denominator)) as u64;
            (
                u128::from(narrow / divisor),
                u128::from(base_denominator / divisor),
            )
        } else {
            let divisor = gcd(absolute, u128::from(base_denominator));
            (absolute / divisor, u128::from(base_denominator) / divisor)
        };
        let magnitude = quotient
            .checked_mul(u128::from(base.numerator()))
            .ok_or(Error::Overflow)?;
        // The base is reduced. Cancelling ticks against its denominator also
        // makes the product coprime to the remaining denominator, including
        // canonical zero; reducing the same result again is unnecessary.
        Ok(Self {
            numerator: signed(magnitude, ticks < 0)?,
            denominator: NonZeroU128::new(denominator).ok_or(Error::Invalid("time denominator"))?,
        })
    }
    pub fn numerator(self) -> i128 {
        self.numerator
    }
    pub fn denominator(self) -> u128 {
        self.denominator.get()
    }
    fn combine(self, other: Self, subtract: bool) -> Result<Self> {
        let divisor = gcd(self.denominator(), other.denominator());
        let left_factor = other.denominator() / divisor;
        let right_factor = self.denominator() / divisor;
        let fast = || {
            let denominator = self.denominator().checked_mul(left_factor)?;
            let left = self
                .numerator
                .checked_mul(i128::try_from(left_factor).ok()?)?;
            let right = other
                .numerator
                .checked_mul(i128::try_from(right_factor).ok()?)?;
            let numerator = if subtract {
                left.checked_sub(right)?
            } else {
                left.checked_add(right)?
            };
            Some((numerator, denominator))
        };
        if let Some((numerator, denominator)) = fast() {
            return Self::new(numerator, denominator);
        }
        let left = Wide::product(self.numerator.unsigned_abs(), left_factor);
        let right = Wide::product(other.numerator.unsigned_abs(), right_factor);
        let left_negative = self.numerator < 0;
        let right_negative = (other.numerator < 0) ^ subtract;
        let (magnitude, negative) = if left_negative == right_negative {
            (
                left.checked_add(right).ok_or(Error::Overflow)?,
                left_negative,
            )
        } else if left >= right {
            (left.sub(right), left_negative)
        } else {
            (right.sub(left), right_negative)
        };
        if magnitude == Wide::ZERO {
            return Self::new(0, 1);
        }
        let denominator = Wide::product(self.denominator(), left_factor);
        let divisor = magnitude.gcd(denominator);
        let numerator = magnitude
            .div_rem(divisor)
            .0
            .to_u128()
            .ok_or(Error::Overflow)?;
        let denominator = denominator
            .div_rem(divisor)
            .0
            .to_u128()
            .ok_or(Error::Overflow)?;
        Self::new(signed(numerator, negative)?, denominator)
    }
    pub fn checked_add(self, other: Self) -> Result<Self> {
        self.combine(other, false)
    }
    pub fn checked_sub(self, other: Self) -> Result<Self> {
        self.combine(other, true)
    }
    pub fn to_ticks(self, base: TimeBase, rounding: Rounding) -> Result<i128> {
        // Cancel both cross factors before multiplication. This preserves large
        // source clocks, including unsigned tfdt values, without float conversion.
        let a = gcd(self.numerator.unsigned_abs(), u128::from(base.numerator()));
        let b = gcd(u128::from(base.denominator()), self.denominator());
        let numerator = self.numerator.unsigned_abs() / a;
        let scale = u128::from(base.denominator()) / b;
        let denominator = self.denominator() / b;
        let divisor = u128::from(base.numerator()) / a;
        let negative = self.numerator < 0;
        if let (Some(n), Some(d)) = (
            numerator.checked_mul(scale),
            denominator.checked_mul(divisor),
        ) {
            return rounded(n / d, n % d != 0, n % d >= d - n % d, negative, rounding);
        }
        let numerator = Wide::product(numerator, scale);
        let denominator = Wide::product(denominator, divisor);
        let (quotient, remainder) = numerator.div_rem(denominator);
        rounded(
            quotient.to_u128().ok_or(Error::Overflow)?,
            remainder != Wide::ZERO,
            remainder >= denominator.sub(remainder),
            negative,
            rounding,
        )
    }
    pub fn to_i64_ticks(self, base: TimeBase, rounding: Rounding) -> Result<i64> {
        i64::try_from(self.to_ticks(base, rounding)?).map_err(|_| Error::Overflow)
    }
    pub fn to_nanoseconds(self, rounding: Rounding) -> Result<i64> {
        self.to_i64_ticks(TimeBase::new(1, 1_000_000_000)?, rounding)
    }
}

fn rounded(
    mut quotient: u128,
    remainder: bool,
    at_least_half: bool,
    negative: bool,
    rounding: Rounding,
) -> Result<i128> {
    let increment = remainder
        && match rounding {
            Rounding::TowardZero => false,
            Rounding::AwayFromZero => true,
            Rounding::Floor => negative,
            Rounding::Ceil => !negative,
            Rounding::NearestAway => at_least_half,
        };
    if increment {
        quotient = quotient.checked_add(1).ok_or(Error::Overflow)?;
    }
    signed(quotient, negative)
}

fn signed(magnitude: u128, negative: bool) -> Result<i128> {
    if negative {
        if magnitude == 1_u128 << 127 {
            Ok(i128::MIN)
        } else {
            i128::try_from(magnitude)
                .map_err(|_| Error::Overflow)?
                .checked_neg()
                .ok_or(Error::Overflow)
        }
    } else {
        i128::try_from(magnitude).map_err(|_| Error::Overflow)
    }
}

fn positive_cmp(mut a: u128, mut b: u128, mut c: u128, mut d: u128) -> Ordering {
    let mut reverse = false;
    loop {
        let ordering = (a / b).cmp(&(c / d));
        if ordering != Ordering::Equal {
            return if reverse {
                ordering.reverse()
            } else {
                ordering
            };
        }
        let (left, right) = (a % b, c % d);
        if left == 0 || right == 0 {
            let ordering = left.cmp(&right);
            return if reverse {
                ordering.reverse()
            } else {
                ordering
            };
        }
        (a, b, c, d) = (b, left, d, right);
        reverse = !reverse;
    }
}

impl Ord for Time {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.numerator.signum().cmp(&other.numerator.signum()) {
            Ordering::Equal => {
                let ordering = positive_cmp(
                    self.numerator.unsigned_abs(),
                    self.denominator(),
                    other.numerator.unsigned_abs(),
                    other.denominator(),
                );
                if self.numerator < 0 {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
            value => value,
        }
    }
}
impl PartialOrd for Time {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
