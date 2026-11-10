//! FreeType's `FT_Long` arithmetic as GCC builds it for 32-bit ARMv7, where
//! `long`, and with it `FT_Pos` and `FT_Fixed`, is 32 bits wide. The
//! auto-hinter and the linear advances `FT_Load_Glyph` scales follow it there.
//!
//! Positions, distances and scales are kept in `i64`, each within `i32` as an
//! `FT_Long` holds it. Sums, differences and products of a few such values are
//! exact in `i64` and agree with GCC's wrapping 32-bit arithmetic in their low
//! 32 bits, so [`wrap`] is applied where a value is stored or read as a whole:
//! compared, divided, shifted right or made absolute. `FT_MulFix`, `FT_MulDiv`
//! and `FT_DivFix` compute in 64 bits and return the low 32 bits of their
//! result; so do the functions here, which also take their arguments as
//! `FT_Long`s. Font-unit values the hints keep in 16 bits (`FT_Short`), and the
//! thresholds derived from them, stay far inside 32 bits and need none of this.

use crate::fixed;

/// `FT_LONG_MAX`.
pub(crate) const MAX: i64 = i32::MAX as i64;
/// `FT_LONG_MIN`.
pub(crate) const MIN: i64 = i32::MIN as i64;

/// `v` as an `FT_Long` holds it: its low 32 bits, sign-extended.
pub(crate) const fn wrap(v: i64) -> i64 {
    v as i32 as i64
}

/// The 32 bits of `v`, as an `FT_Pos` outline coordinate.
pub(crate) const fn to_i32(v: i64) -> i32 {
    v as i32
}

/// `FT_ABS` of `v` as an `FT_Long`: `FT_LONG_MIN` stays negative.
pub(crate) const fn abs(v: i64) -> i64 {
    (v as i32).wrapping_abs() as i64
}

/// `FT_MulFix`.
pub(crate) const fn mul_fix(a: i64, b: i64) -> i64 {
    wrap(fixed::mul_fix(wrap(a), wrap(b)))
}

/// `FT_DivFix`.
pub(crate) fn div_fix(a: i64, b: i64) -> i64 {
    wrap(fixed::div_fix(wrap(a), wrap(b)))
}

/// `FT_MulDiv`.
pub(crate) fn mul_div(a: i64, b: i64, c: i64) -> i64 {
    wrap(fixed::mul_div(wrap(a), wrap(b), wrap(c)))
}

/// `FT_Load_Glyph`'s scaling of a linear advance in font units to 16.16
/// pixels, `FT_MulDiv(advance, scale, 64)`, after every load at a size.
pub(crate) fn linear_advance(units: i64, scale: i64) -> i64 {
    mul_div(units, scale, 64)
}

/// `FT_PIX_ROUND` (and `FT_PIX_ROUND_LONG`).
pub(crate) const fn pix_round(v: i64) -> i64 {
    wrap(fixed::pix_round(wrap(v)))
}

/// `FT_PIX_CEIL_LONG`.
pub(crate) const fn pix_ceil(v: i64) -> i64 {
    wrap(fixed::pix_ceil(wrap(v)))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX, MIN, abs, div_fix, linear_advance, mul_div, mul_fix, pix_ceil, pix_round, wrap,
    };

    #[test]
    fn results_keep_their_low_32_bits() {
        assert_eq!(wrap(MAX + 1), MIN);
        assert_eq!(wrap(MIN - 1), MAX);
        assert_eq!(abs(MIN), MIN);
        assert_eq!(abs(-5), 5);
        // 0x4000_0000 × 4.0 is 2^32, whose low 32 bits are zero.
        assert_eq!(mul_fix(0x4000_0000, 0x4_0000), 0);
        // 0x4000_0000 × 2.0 is 2^31, which wraps to FT_LONG_MIN.
        assert_eq!(mul_fix(0x4000_0000, 0x2_0000), MIN);
        assert_eq!(mul_fix(-3, 0x8000), -2);
        // 0x10000 / (1/65536) is 2^32.
        assert_eq!(div_fix(0x1_0000, 1), 0);
        assert_eq!(div_fix(1, 0), 0x7fff_ffff);
        assert_eq!(div_fix(-1, 0), -0x7fff_ffff);
        assert_eq!(mul_div(0x4000_0000, 6, 3), MIN);
        assert_eq!(mul_div(-7, 1, 2), -4);
        // FT_PIX_ROUND_LONG and FT_PIX_CEIL_LONG wrap at the top.
        assert_eq!(pix_round(MAX - 10), MIN);
        assert_eq!(pix_ceil(MAX - 10), MIN);
        assert_eq!(pix_round(95), 64);
    }

    #[test]
    fn linear_advances_wrap_as_on_32_bit_arm() {
        // 65535 units at a y scale of 2^20 (16 pixels per unit): 2^36 / 64
        // is 2^30, which fits; at 2^22 it is 2^32, whose low bits are zero.
        assert_eq!(linear_advance(65535, 1 << 20), 0x3fff_c000);
        assert_eq!(linear_advance(65535, 1 << 22), -0x1_0000);
        // A y scale of 2^31 is FT_LONG_MIN as a 32-bit FT_Fixed.
        assert_eq!(linear_advance(1, 1 << 31), -(1 << 25));
    }
}
