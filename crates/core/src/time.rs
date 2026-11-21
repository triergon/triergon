//! Exact reduced rational seconds, with separately requested rounded views.
use crate::wide::Wide;
use crate::{Error, Result};
use std::{
    cmp::Ordering,
    num::{NonZeroU64, NonZeroU128},
};

/// How a conversion to whole ticks treats a value between two of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// The tick nearer to zero.
    TowardZero,
    /// The tick farther from zero.
    AwayFromZero,
    /// The lower tick.
    Floor,
    /// The upper tick.
    Ceil,
    /// The nearest tick; halfway, the one farther from zero.
    NearestAway,
}

/// The length of one clock tick in seconds, as a reduced fraction of two
/// non-zero `u64`: 1/90000 for an MPEG clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeBase {
    numerator: NonZeroU64,
    denominator: NonZeroU64,
}

impl TimeBase {
    /// One microsecond per tick.
    pub const MICROSECONDS: Self = Self {
        numerator: NonZeroU64::MIN,
        denominator: NonZeroU64::new(1_000_000).unwrap(),
    };

    /// One nanosecond per tick.
    pub const NANOSECONDS: Self = Self {
        numerator: NonZeroU64::MIN,
        denominator: NonZeroU64::new(1_000_000_000).unwrap(),
    };

    /// `numerator / denominator` seconds per tick, reduced. A zero in either
    /// is [`Error::Invalid`].
    pub fn new(numerator: u64, denominator: u64) -> Result<Self> {
        if numerator == 0 || denominator == 0 {
            return Err(Error::Invalid("zero timebase"));
        }
        let [numerator, denominator] =
            exact_quotients([numerator, denominator], gcd_u64(numerator, denominator));
        Ok(Self {
            numerator: NonZeroU64::new(numerator).ok_or(Error::Invalid("timebase"))?,
            denominator: NonZeroU64::new(denominator).ok_or(Error::Invalid("timebase"))?,
        })
    }
    /// The reduced numerator.
    pub fn numerator(self) -> u64 {
        self.numerator.get()
    }
    /// The reduced denominator.
    pub fn denominator(self) -> u64 {
        self.denominator.get()
    }
    /// `ticks` of this base in ticks of `to`, rounded as asked.
    /// [`Error::Overflow`] when the result does not fit `i64`.
    pub fn rescale(self, ticks: i64, to: Self, rounding: Rounding) -> Result<i64> {
        if self == to || ticks == 0 {
            return Ok(ticks);
        }
        // A rounded conversion needs the original ratio, without first
        // reducing it to canonical seconds.
        let scale = u128::from(self.numerator()) * u128::from(to.denominator());
        let divisor = u128::from(self.denominator()) * u128::from(to.numerator());
        let Some(magnitude) = scale.checked_mul(u128::from(ticks.unsigned_abs())) else {
            return Time::from_ticks(i128::from(ticks), self)?.to_i64_ticks(to, rounding);
        };
        let (quotient, remainder) = match (u64::try_from(magnitude), u64::try_from(divisor)) {
            (Ok(magnitude), Ok(divisor)) => (
                u128::from(magnitude / divisor),
                u128::from(magnitude % divisor),
            ),
            _ => (magnitude / divisor, magnitude % divisor),
        };
        let result = rounded(
            quotient,
            remainder != 0,
            remainder >= divisor - remainder,
            ticks < 0,
            rounding,
        )?;
        i64::try_from(result).map_err(|_| Error::Overflow)
    }
}

/// The conversion of one time base's ticks into another's, prepared once for a
/// stream: [`TickScale::rescale`] returns what [`TimeBase::rescale`] returns, but
/// divides by a reciprocal computed here, where `rescale` divides for every call.
/// Without a divide instruction, as on 32-bit ARMv7, each such division is a
/// software loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickScale {
    from: TimeBase,
    to: TimeBase,
    /// The reduced ratio `factor / divisor` of `from` in `to`, and the divisor's
    /// reciprocal, when both terms fit 64 bits; `rescale` handles the others.
    reduced: Option<(u64, Reciprocal)>,
}

impl TickScale {
    /// The conversion of ticks of `from` into ticks of `to`.
    pub fn new(from: TimeBase, to: TimeBase) -> Self {
        // Both bases are reduced, so the ratio `(a · d) / (b · c)` of `a/b` in
        // `c/d` reduces by `gcd(a, c) · gcd(b, d)`.
        let numerators = binary_gcd(from.numerator(), to.numerator());
        let denominators = binary_gcd(from.denominator(), to.denominator());
        let factor =
            u128::from(from.numerator() / numerators) * u128::from(to.denominator() / denominators);
        let divisor =
            u128::from(from.denominator() / denominators) * u128::from(to.numerator() / numerators);
        let reduced = u64::try_from(factor)
            .ok()
            .zip(u64::try_from(divisor).ok())
            .and_then(|(factor, divisor)| Some((factor, Reciprocal::new(divisor)?)));
        Self { from, to, reduced }
    }

    /// The base converted from.
    pub fn from(&self) -> TimeBase {
        self.from
    }

    /// The base converted to.
    pub fn to(&self) -> TimeBase {
        self.to
    }

    /// `ticks` of `from` in ticks of `to`, rounded as asked: the result of
    /// [`TimeBase::rescale`], error included.
    pub fn rescale(&self, ticks: i64, rounding: Rounding) -> Result<i64> {
        let Some((quotient, remainder, at_least_half)) = self.divide(ticks.unsigned_abs()) else {
            return self.from.rescale(ticks, self.to, rounding);
        };
        let result = rounded(
            u128::from(quotient),
            remainder,
            at_least_half,
            ticks < 0,
            rounding,
        )?;
        i64::try_from(result).map_err(|_| Error::Overflow)
    }

    /// The magnitude of `ticks` in ticks of `to`, rounded to nearest with ties
    /// away from zero; `None` when the conversion has no reduced 64-bit factor
    /// or `ticks` times that factor does not fit 64 bits.
    pub fn nearest_away_magnitude(&self, ticks: u64) -> Option<u64> {
        let (quotient, _, at_least_half) = self.divide(ticks)?;
        // A zero remainder is never at least half of a nonzero divisor.
        Some(quotient + u64::from(at_least_half))
    }

    /// The quotient of `ticks · factor / divisor`, whether a remainder is left
    /// and whether it is at least half the divisor; `None` when the product
    /// does not fit 64 bits.
    fn divide(&self, ticks: u64) -> Option<(u64, bool, bool)> {
        let (factor, divisor) = self.reduced?;
        let (quotient, remainder) = divisor.divide(ticks.checked_mul(factor)?);
        Some((
            quotient,
            remainder != 0,
            remainder != 0 && remainder >= divisor.get() - remainder,
        ))
    }
}

/// Whether 64-bit `/` and `%` are library loops on this target, as on 32-bit
/// ARMv7, each costing many times a multiplication. There the reductions below
/// use shifts, subtractions and multiplications instead; where division is an
/// instruction they divide.
const SOFTWARE_DIVISION: bool = cfg!(target_pointer_width = "32");

/// The greatest common divisor of two nonzero values by shifts and subtractions
/// alone, in 32-bit registers when both values fit them.
fn binary_gcd(a: u64, b: u64) -> u64 {
    debug_assert!(a != 0 && b != 0);
    if let (Ok(a), Ok(b)) = (u32::try_from(a), u32::try_from(b)) {
        return u64::from(binary_gcd_u32(a, b));
    }
    let shift = (a | b).trailing_zeros();
    let (mut a, mut b) = (a >> a.trailing_zeros(), b);
    loop {
        b >>= b.trailing_zeros();
        (a, b) = (a.min(b), a.max(b) - a.min(b));
        if b == 0 {
            return a << shift;
        }
    }
}

/// [`binary_gcd`] of two nonzero 32-bit values.
fn binary_gcd_u32(a: u32, b: u32) -> u32 {
    let shift = (a | b).trailing_zeros();
    let (mut a, mut b) = (a >> a.trailing_zeros(), b);
    loop {
        b >>= b.trailing_zeros();
        (a, b) = (a.min(b), a.max(b) - a.min(b));
        if b == 0 {
            return a << shift;
        }
    }
}

/// A nonzero divisor prepared to divide its multiples without division: it is
/// `odd · 2^shift`, and a multiple `n` is `(n >> shift) · odd⁻¹` modulo 2⁶⁴,
/// since an odd value is invertible modulo a power of two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExactDivisor {
    shift: u32,
    odd: u64,
    inverse: u64,
}

impl ExactDivisor {
    fn new(divisor: NonZeroU64) -> Self {
        let shift = divisor.trailing_zeros();
        let odd = divisor.get() >> shift;
        // Each Newton step x ← x · (2 − odd · x) doubles the number of correct
        // low bits, and an odd value is its own inverse modulo 8: four steps in
        // 32 bits give 48 ≥ 32 bits, one more in 64 bits completes the inverse.
        let low = odd as u32;
        let mut inverse = low;
        for _ in 0..4 {
            inverse = inverse.wrapping_mul(2_u32.wrapping_sub(low.wrapping_mul(inverse)));
        }
        let inverse = u64::from(inverse);
        Self {
            shift,
            odd,
            inverse: inverse.wrapping_mul(2_u64.wrapping_sub(odd.wrapping_mul(inverse))),
        }
    }

    /// `dividend / divisor` for a multiple of the divisor.
    fn quotient(self, dividend: u64) -> u64 {
        (dividend >> self.shift).wrapping_mul(self.inverse)
    }

    /// `dividend / divisor` when the divisor divides `dividend`. The candidate
    /// times `odd` equals `dividend >> shift` modulo 2⁶⁴, and is the true
    /// quotient exactly when that product does not overflow.
    fn checked_quotient(self, dividend: u64) -> Option<u64> {
        if dividend & ((1 << self.shift) - 1) != 0 {
            return None;
        }
        let quotient = self.quotient(dividend);
        (high_product(quotient, self.odd) == 0).then_some(quotient)
    }
}

/// Both `dividends` divided by `divisor`, a nonzero divisor of each.
#[inline]
fn exact_quotients(dividends: [u64; 2], divisor: u64) -> [u64; 2] {
    debug_assert!(divisor != 0 && dividends.iter().all(|value| value % divisor == 0));
    match NonZeroU64::new(divisor) {
        Some(divisor) if SOFTWARE_DIVISION && divisor.get() != 1 => {
            let divisor = ExactDivisor::new(divisor);
            dividends.map(|value| divisor.quotient(value))
        }
        _ => dividends.map(|value| value / divisor),
    }
}

/// `dividend / divisor` when the nonzero `divisor` divides `dividend`.
#[inline]
fn checked_exact_quotient(dividend: u64, divisor: NonZeroU64) -> Option<u64> {
    if SOFTWARE_DIVISION {
        ExactDivisor::new(divisor).checked_quotient(dividend)
    } else {
        dividend
            .is_multiple_of(divisor.get())
            .then(|| dividend / divisor)
    }
}

/// A nonzero 64-bit divisor with its reciprocal `⌊(2⁶⁴ − 1) / d⌋`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reciprocal {
    divisor: NonZeroU64,
    reciprocal: u64,
}

impl Reciprocal {
    fn new(divisor: u64) -> Option<Self> {
        let divisor = NonZeroU64::new(divisor)?;
        Some(Self {
            divisor,
            reciprocal: u64::MAX / divisor,
        })
    }

    fn get(self) -> u64 {
        self.divisor.get()
    }

    /// `value / d` and `value % d`. Since `2⁶⁴ − d · reciprocal ≤ d`, the exact
    /// `value · reciprocal / 2⁶⁴` lies less than `value / 2⁶⁴ · (1 + 1/d) < 2` below
    /// `value / d`, so its integer part is at most two below the quotient and never
    /// above it: at most two corrections follow, and `quotient · d` never exceeds
    /// `value`.
    fn divide(self, value: u64) -> (u64, u64) {
        let divisor = self.get();
        if divisor == 1 {
            return (value, 0);
        }
        let mut quotient = high_product(value, self.reciprocal);
        let mut remainder = value - quotient * divisor;
        while remainder >= divisor {
            quotient += 1;
            remainder -= divisor;
        }
        (quotient, remainder)
    }
}

/// The high 64 bits of `a · b`, from 32-bit limbs: four multiplications that
/// 32-bit targets execute as instructions.
fn high_product(a: u64, b: u64) -> u64 {
    let (a_low, a_high) = (a & 0xffff_ffff, a >> 32);
    let (b_low, b_high) = (b & 0xffff_ffff, b >> 32);
    let low = a_low * b_low;
    let middle_a = a_high * b_low;
    let middle_b = a_low * b_high;
    let carry = ((low >> 32) + (middle_a & 0xffff_ffff) + (middle_b & 0xffff_ffff)) >> 32;
    a_high * b_high + (middle_a >> 32) + (middle_b >> 32) + carry
}

/// An exact time in seconds, possibly negative: an `i128` over a positive
/// `u128`, always reduced, so equal times compare equal.
///
/// No value means "unknown"; that is `Option<Time>`. Arithmetic fails with
/// [`Error::Overflow`] only when the reduced result does not fit, never because
/// an intermediate product did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Time {
    numerator: i128,
    denominator: NonZeroU128,
}

#[inline]
fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    if SOFTWARE_DIVISION {
        return match (a, b) {
            (0, value) | (value, 0) => value,
            _ => binary_gcd(a, b),
        };
    }
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[inline]
fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        if let (Ok(a), Ok(b)) = (u64::try_from(a), u64::try_from(b)) {
            return u128::from(gcd_u64(a, b));
        }
        (a, b) = (b, a % b);
    }
    a
}

impl Time {
    /// `numerator / denominator` seconds, reduced. A zero denominator is
    /// [`Error::Invalid`].
    pub fn new(numerator: i128, denominator: u128) -> Result<Self> {
        if denominator == 0 {
            return Err(Error::Invalid("zero time denominator"));
        }
        let magnitude = numerator.unsigned_abs();
        if let (Ok(magnitude), Ok(denominator)) =
            (u64::try_from(magnitude), u64::try_from(denominator))
        {
            let [reduced, denominator] =
                exact_quotients([magnitude, denominator], gcd_u64(magnitude, denominator));
            let reduced = i128::from(reduced);
            return Ok(Self {
                numerator: if numerator < 0 { -reduced } else { reduced },
                denominator: NonZeroU128::new(u128::from(denominator))
                    .ok_or(Error::Invalid("time denominator"))?,
            });
        }
        Self::reduced_wide(magnitude, denominator, numerator < 0)
    }
    /// [`new`](Self::new) for a magnitude or denominator beyond 64 bits.
    #[cold]
    #[inline(never)]
    fn reduced_wide(magnitude: u128, denominator: u128, negative: bool) -> Result<Self> {
        let divisor = gcd(magnitude, denominator);
        Ok(Self {
            numerator: signed(magnitude / divisor, negative)?,
            denominator: NonZeroU128::new(denominator / divisor)
                .ok_or(Error::Invalid("time denominator"))?,
        })
    }
    /// The time of `ticks` ticks of `base`.
    pub fn from_ticks(ticks: i128, base: TimeBase) -> Result<Self> {
        let absolute = ticks.unsigned_abs();
        let base_denominator = base.denominator();
        let (quotient, denominator) = if let Ok(narrow) = u64::try_from(absolute) {
            let [quotient, denominator] = exact_quotients(
                [narrow, base_denominator],
                gcd_u64(narrow, base_denominator),
            );
            (u128::from(quotient), u128::from(denominator))
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
    /// The reduced numerator, which carries the sign.
    pub fn numerator(self) -> i128 {
        self.numerator
    }
    /// The reduced denominator, at least 1.
    pub fn denominator(self) -> u128 {
        self.denominator.get()
    }
    fn combine(self, other: Self, subtract: bool) -> Result<Self> {
        let divisor = gcd(self.denominator(), other.denominator());
        let (left_factor, right_factor) = if let (Ok(left), Ok(right)) = (
            u64::try_from(self.denominator()),
            u64::try_from(other.denominator()),
        ) {
            // Both denominators are positive; their gcd is nonzero and fits u64.
            let [right, left] = exact_quotients([right, left], divisor as u64);
            (u128::from(right), u128::from(left))
        } else {
            (other.denominator() / divisor, self.denominator() / divisor)
        };
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
        self.combine_wide(other, left_factor, right_factor, subtract)
    }
    /// [`combine`](Self::combine) through 256-bit intermediates, for operands
    /// whose cross products or common denominator overflow 128 bits.
    #[cold]
    #[inline(never)]
    fn combine_wide(
        self,
        other: Self,
        left_factor: u128,
        right_factor: u128,
        subtract: bool,
    ) -> Result<Self> {
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
    /// The exact sum.
    pub fn checked_add(self, other: Self) -> Result<Self> {
        self.combine(other, false)
    }
    /// The exact difference.
    pub fn checked_sub(self, other: Self) -> Result<Self> {
        self.combine(other, true)
    }
    /// This time in ticks of `base`, rounded as asked.
    pub fn to_ticks(self, base: TimeBase, rounding: Rounding) -> Result<i128> {
        let exact_denominator = self.denominator();
        if base.numerator() == 1
            && let Ok(denominator) = NonZeroU64::try_from(self.denominator)
            && let Some(scale) = checked_exact_quotient(base.denominator(), denominator)
        {
            let magnitude = self
                .numerator
                .unsigned_abs()
                .checked_mul(u128::from(scale))
                .ok_or(Error::Overflow)?;
            return signed(magnitude, self.numerator < 0);
        }
        if base.numerator() == 1
            && let (Ok(magnitude), Ok(denominator)) = (
                u64::try_from(self.numerator.unsigned_abs()),
                u64::try_from(exact_denominator),
            )
            && let Some(scaled) = magnitude.checked_mul(base.denominator())
        {
            let remainder = scaled % denominator;
            return rounded(
                u128::from(scaled / denominator),
                remainder != 0,
                remainder >= denominator - remainder,
                self.numerator < 0,
                rounding,
            );
        }
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
        wide_quotient(
            [numerator, scale],
            [denominator, divisor],
            negative,
            rounding,
        )
    }
    /// [`to_ticks`](Self::to_ticks) as an `i64`; [`Error::Overflow`] when it
    /// does not fit.
    pub fn to_i64_ticks(self, base: TimeBase, rounding: Rounding) -> Result<i64> {
        i64::try_from(self.to_ticks(base, rounding)?).map_err(|_| Error::Overflow)
    }
    /// This time in nanoseconds, rounded as asked.
    pub fn to_nanoseconds(self, rounding: Rounding) -> Result<i64> {
        self.to_i64_ticks(TimeBase::NANOSECONDS, rounding)
    }
}

/// The product of `numerator` over the product of `denominator`, rounded,
/// through 256-bit intermediates, for products that overflow 128 bits.
#[cold]
#[inline(never)]
fn wide_quotient(
    numerator: [u128; 2],
    denominator: [u128; 2],
    negative: bool,
    rounding: Rounding,
) -> Result<i128> {
    let numerator = Wide::product(numerator[0], numerator[1]);
    let denominator = Wide::product(denominator[0], denominator[1]);
    let (quotient, remainder) = numerator.div_rem(denominator);
    rounded(
        quotient.to_u128().ok_or(Error::Overflow)?,
        remainder != Wide::ZERO,
        remainder >= denominator.sub(remainder),
        negative,
        rounding,
    )
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

fn positive_cmp(a: u128, b: u128, c: u128, d: u128) -> Ordering {
    if let Some(left) = a.checked_mul(d)
        && let Some(right) = c.checked_mul(b)
    {
        return left.cmp(&right);
    }
    continued_fraction_cmp(a, b, c, d)
}

/// [`positive_cmp`] for cross products beyond 128 bits: compares the continued
/// fraction expansions of `a / b` and `c / d` term by term.
#[cold]
#[inline(never)]
fn continued_fraction_cmp(mut a: u128, mut b: u128, mut c: u128, mut d: u128) -> Ordering {
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

#[cfg(test)]
mod tick_scale_tests {
    use super::*;

    fn values() -> Vec<u64> {
        let mut values = vec![
            0,
            1,
            2,
            3,
            7,
            1 << 31,
            (1 << 32) - 1,
            1 << 32,
            (1 << 32) + 1,
        ];
        values.extend([
            u64::MAX,
            u64::MAX - 1,
            u64::MAX / 3,
            i64::MAX as u64,
            1 << 63,
        ]);
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..2000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            values.push(x >> (x % 64));
        }
        values
    }

    #[test]
    fn reciprocal_division_is_exact() {
        let values = values();
        for &divisor in values.iter().filter(|&&d| d != 0).take(600) {
            let reciprocal = Reciprocal::new(divisor).unwrap();
            for &value in &values {
                assert_eq!(
                    reciprocal.divide(value),
                    (value / divisor, value % divisor),
                    "{value} / {divisor}"
                );
            }
        }
        assert!(Reciprocal::new(0).is_none());
    }

    fn euclid(mut a: u64, mut b: u64) -> u64 {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    }

    #[test]
    fn binary_gcd_matches_euclid() {
        let values = values();
        for &a in values.iter().take(300) {
            for &b in values.iter().take(300) {
                if a != 0 && b != 0 {
                    assert_eq!(binary_gcd(a, b), euclid(a, b), "gcd({a}, {b})");
                }
                assert_eq!(gcd_u64(a, b), euclid(a, b), "gcd({a}, {b})");
            }
        }
    }

    #[test]
    fn exact_division_matches_division() {
        let values = values();
        for &divisor in values.iter().filter(|&&d| d != 0).take(600) {
            let exact = ExactDivisor::new(NonZeroU64::new(divisor).unwrap());
            for &value in &values {
                let (quotient, remainder) = (value / divisor, value % divisor);
                assert_eq!(
                    exact.quotient(value - remainder),
                    quotient,
                    "{value} / {divisor}"
                );
                let expected = (remainder == 0).then_some(quotient);
                assert_eq!(
                    exact.checked_quotient(value),
                    expected,
                    "{value} / {divisor}"
                );
                let divisor = NonZeroU64::new(divisor).unwrap();
                assert_eq!(checked_exact_quotient(value, divisor), expected);
                let multiple = value - remainder;
                assert_eq!(
                    exact_quotients([multiple, 0], divisor.get()),
                    [quotient, 0],
                    "{multiple} / {divisor}"
                );
            }
        }
    }

    #[test]
    fn high_product_matches_wide_multiplication() {
        let values = values();
        for &a in values.iter().take(300) {
            for &b in values.iter().take(300) {
                let wide = (u128::from(a) * u128::from(b)) >> 64;
                assert_eq!(u128::from(high_product(a, b)), wide);
            }
        }
    }

    #[test]
    fn tick_scale_matches_rescale() {
        let bases = [
            (1, 1),
            (1, 1_000),
            (1, 90_000),
            (1, 48_000),
            (1, 44_100),
            (1, 600),
            (1001, 30_000),
            (1, 1_000_000),
            (1, 1_000_000_000),
            (3, 7),
            (u64::MAX, 1),
            (1, u64::MAX),
            (u64::MAX - 1, u64::MAX),
            (1 << 40, 3),
        ]
        .map(|(n, d)| TimeBase::new(n, d).unwrap());
        let roundings = [
            Rounding::TowardZero,
            Rounding::AwayFromZero,
            Rounding::Floor,
            Rounding::Ceil,
            Rounding::NearestAway,
        ];
        let magnitudes = values();
        for from in bases {
            for to in bases {
                let scale = TickScale::new(from, to);
                let factor = u128::from(from.numerator()) * u128::from(to.denominator());
                let divisor = u128::from(from.denominator()) * u128::from(to.numerator());
                for &magnitude in magnitudes.iter().take(400) {
                    for ticks in [
                        magnitude as i64,
                        (magnitude as i64).wrapping_neg(),
                        i64::MIN,
                    ] {
                        for rounding in roundings {
                            assert_eq!(
                                scale.rescale(ticks, rounding),
                                from.rescale(ticks, to, rounding),
                                "{ticks} from {from:?} to {to:?} {rounding:?}"
                            );
                        }
                    }
                    // The unreduced product can exceed 128 bits where the
                    // reduced one fits 64; those are covered by `rescale` above.
                    if let (Some(nearest), Some(scaled)) = (
                        scale.nearest_away_magnitude(magnitude),
                        u128::from(magnitude).checked_mul(factor),
                    ) {
                        let (quotient, remainder) = (scaled / divisor, scaled % divisor);
                        let exact = quotient
                            + u128::from(remainder != 0 && remainder >= divisor - remainder);
                        assert_eq!(
                            u128::from(nearest),
                            exact,
                            "{magnitude} from {from:?} to {to:?}"
                        );
                    }
                }
            }
        }
    }
}
