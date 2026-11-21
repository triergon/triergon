//! [`fixed`](super)'s operations on values held in 32 bits, wrapping as two's
//! complement arithmetic of that width does.
//!
//! The auto-hinter, the CFF FontMatrix and the linear advances keep their
//! positions, distances and scales within 32 bits, as the reference this
//! crate is compared with does on 32-bit ARM, and their results depend on
//! where a sum or product wraps. The values are carried in `i64`: sums,
//! differences and products of a few of them are exact there and agree with
//! wrapping 32-bit arithmetic in their low 32 bits, so [`wrap`] is applied
//! where a value is stored or read as a whole (compared, divided, shifted
//! right or made absolute). The products and quotients here compute in 64
//! bits and return the low 32 bits of the result. Font-unit values that fit
//! 16 bits, and thresholds derived from them, stay far inside 32 bits and
//! need none of this.

use crate::fixed;

/// The largest value 32 bits hold.
pub(crate) const MAX: i64 = i32::MAX as i64;
/// The smallest value 32 bits hold.
pub(crate) const MIN: i64 = i32::MIN as i64;

/// The low 32 bits of `v`, sign-extended.
pub(crate) const fn wrap(v: i64) -> i64 {
    v as i32 as i64
}

/// The low 32 bits of `v`, as an outline coordinate.
pub(crate) const fn to_i32(v: i64) -> i32 {
    v as i32
}

/// The magnitude of `v`'s low 32 bits; the smallest value stays negative.
pub(crate) const fn abs(v: i64) -> i64 {
    (v as i32).wrapping_abs() as i64
}

/// [`fixed::mul_fix`] in 32 bits.
pub(crate) const fn mul_fix(a: i64, b: i64) -> i64 {
    wrap(fixed::mul_fix(wrap(a), wrap(b)))
}

/// [`fixed::div_fix`] in 32 bits.
pub(crate) fn div_fix(a: i64, b: i64) -> i64 {
    wrap(fixed::div_fix(wrap(a), wrap(b)))
}

/// [`fixed::mul_div`] in 32 bits.
pub(crate) fn mul_div(a: i64, b: i64, c: i64) -> i64 {
    wrap(fixed::mul_div(wrap(a), wrap(b), wrap(c)))
}

/// [`fixed::pix_round`] in 32 bits.
pub(crate) const fn pix_round(v: i64) -> i64 {
    wrap(fixed::pix_round(wrap(v)))
}

/// [`fixed::pix_ceil`] in 32 bits.
pub(crate) const fn pix_ceil(v: i64) -> i64 {
    wrap(fixed::pix_ceil(wrap(v)))
}

#[cfg(test)]
mod tests {
    use super::{MAX, MIN, abs, div_fix, mul_div, mul_fix, pix_ceil, pix_round, wrap};

    #[test]
    fn results_keep_their_low_32_bits() {
        assert_eq!(wrap(MAX + 1), MIN);
        assert_eq!(wrap(MIN - 1), MAX);
        assert_eq!(abs(MIN), MIN);
        assert_eq!(abs(-5), 5);
        // 0x4000_0000 × 4.0 is 2^32, whose low 32 bits are zero.
        assert_eq!(mul_fix(0x4000_0000, 0x4_0000), 0);
        // 0x4000_0000 × 2.0 is 2^31, which wraps to the smallest value.
        assert_eq!(mul_fix(0x4000_0000, 0x2_0000), MIN);
        assert_eq!(mul_fix(-3, 0x8000), -2);
        // 0x10000 / (1/65536) is 2^32.
        assert_eq!(div_fix(0x1_0000, 1), 0);
        assert_eq!(div_fix(1, 0), 0x7fff_ffff);
        assert_eq!(div_fix(-1, 0), -0x7fff_ffff);
        assert_eq!(mul_div(0x4000_0000, 6, 3), MIN);
        assert_eq!(mul_div(-7, 1, 2), -4);
        // Rounding up to a pixel wraps at the top.
        assert_eq!(pix_round(MAX - 10), MIN);
        assert_eq!(pix_ceil(MAX - 10), MIN);
        assert_eq!(pix_round(95), 64);
    }
}
