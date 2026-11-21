//! Binary fixed-point arithmetic: the products, quotients and roundings every
//! scaled coordinate and metric of this crate goes through.
//!
//! Three representations occur. Positions and distances at a size are 26.6
//! (1/64 pixel); scales, matrix coefficients and charstring numbers are 16.16;
//! the TrueType interpreter's unit vectors are 2.14. A value is a plain integer
//! holding the scaled number, and the name of each function says which
//! representation it works on.
//!
//! Overflow follows one of two policies, chosen by module:
//!
//! - The functions here take and return `i64` and compute in 64 bits. They are
//!   exact wherever the operands and the result fit 32 bits, and wrap in 64
//!   bits beyond that. Callers that keep 32-bit values narrow the result where
//!   they store it.
//! - [`wrap32`] has the same operations on values a 32-bit integer holds: each
//!   reads the low 32 bits of its operands and returns the low 32 bits of the
//!   64-bit result, sign-extended.
//!
//! Every product and quotient rounds half away from zero. The results are the
//! ones FreeType 2.15.0 gives on a target with a 32-bit `long`, which the
//! outline and metric comparisons depend on; the module is translated from it
//! (see `docs/provenance.md`).

pub mod wrap32;

/// One in 16.16.
pub const ONE: i64 = 0x1_0000;

/// 16.16 horizontal and vertical scales from font units to outline coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Scale {
    pub(crate) x: i64,
    pub(crate) y: i64,
}

impl Scale {
    /// Font units, unscaled.
    pub(crate) const UNITS: Self = Self { x: ONE, y: ONE };
}

/// `a × b / 65536`, rounded half away from zero: a value times a 16.16 factor.
pub const fn mul_fix(a: i64, b: i64) -> i64 {
    let ab = a.wrapping_mul(b);
    // The sign bit turns 0x8000 into 0x7fff for negative products.
    (ab.wrapping_add(0x8000).wrapping_add(ab >> 63)) >> 16
}

/// `a × 65536 / b`, rounded half away from zero: the 16.16 ratio of two values
/// of one representation. A zero divisor gives `0x7fffffff`, negated when `a`
/// is negative.
pub fn div_fix(a: i64, b: i64) -> i64 {
    let negative = (a < 0) != (b < 0);
    let (a, b) = (a.unsigned_abs(), b.unsigned_abs());
    let q = ((a << 16).wrapping_add(b >> 1))
        .checked_div(b)
        .unwrap_or(0x7fff_ffff);
    let q = q.cast_signed();
    if negative { q.wrapping_neg() } else { q }
}

/// `a × b / c`, rounded half away from zero. A zero divisor gives `0x7fffffff`,
/// negated when exactly one of `a` and `b` is negative.
pub fn mul_div(a: i64, b: i64, c: i64) -> i64 {
    let negative = ((a < 0) != (b < 0)) != (c < 0);
    let (a, b, c) = (a.unsigned_abs(), b.unsigned_abs(), c.unsigned_abs());
    let d = a
        .wrapping_mul(b)
        .wrapping_add(c >> 1)
        .checked_div(c)
        .unwrap_or(0x7fff_ffff);
    let d = d.cast_signed();
    if negative { d.wrapping_neg() } else { d }
}

/// A 26.6 value rounded to the nearest whole pixel, halves up.
pub const fn pix_round(v: i64) -> i64 {
    v.wrapping_add(32) & !63
}

/// A 26.6 value rounded down to a whole pixel.
pub const fn pix_floor(v: i64) -> i64 {
    v & !63
}

/// A 26.6 value rounded up to a whole pixel.
pub const fn pix_ceil(v: i64) -> i64 {
    v.wrapping_add(63) & !63
}

/// The index of the highest set bit (zero for zero).
const fn msb(z: u32) -> u32 {
    match z.checked_ilog2() {
        Some(bit) => bit,
        None => 0,
    }
}

/// The square root of a 16.16 value, by a 64-bit Babylonian iteration whose
/// quotients round up.
pub(crate) fn sqrt_fixed(v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    let r = (u64::from(v) << 16) - 1;
    let mut q = 1_u32 << ((17 + msb(v)) >> 1);
    loop {
        let t = q;
        // Every quotient fits: `t` starts at or above √r and stays near it.
        let quotient = u32::try_from(r / u64::from(t)).unwrap_or(u32::MAX);
        q = ((u64::from(t) + u64::from(quotient) + 1) >> 1) as u32;
        if q == t {
            return q;
        }
    }
}

/// Normalises `(x, y)` to a 16.16 unit vector in place and returns the
/// original length, by a Newton iteration on the low 32 bits of the
/// components, wrapping in 32 bits.
pub(crate) fn norm_len(v: &mut (i64, i64)) -> i64 {
    let (x0, y0) = (v.0 as i32, v.1 as i32);
    let (sx, sy) = (x0 < 0, y0 < 0);
    let (mut x, mut y) = (x0.unsigned_abs(), y0.unsigned_abs());
    if x == 0 {
        if y > 0 {
            v.1 = if sy { -ONE } else { ONE };
        }
        return i64::from(y);
    }
    if y == 0 {
        v.0 = if sx { -ONE } else { ONE };
        return i64::from(x);
    }
    let estimate = |x: u32, y: u32| {
        if x > y {
            x.wrapping_add(y >> 1)
        } else {
            y.wrapping_add(x >> 1)
        }
    };
    let mut l = estimate(x, y);
    let mut shift = 31 - msb(l).cast_signed();
    shift -= 15 + i32::from(l >= (0xAAAA_AAAA_u32 >> shift));
    if shift > 0 {
        x <<= shift;
        y <<= shift;
        l = estimate(x, y);
    } else {
        x >>= -shift;
        y >>= -shift;
        l >>= -shift;
    }
    let mut b = 0x1_0000_i32.wrapping_sub(l.cast_signed());
    let (xs, ys) = (x.cast_signed(), y.cast_signed());
    let (mut u, mut w);
    loop {
        u = xs.wrapping_add(xs.wrapping_mul(b) >> 16).cast_unsigned();
        w = ys.wrapping_add(ys.wrapping_mul(b) >> 16).cast_unsigned();
        // The squared length approaches 2^32; its wrapped difference is exact.
        let mut z = (u.wrapping_mul(u).wrapping_add(w.wrapping_mul(w)))
            .cast_signed()
            .wrapping_neg()
            / 0x200;
        z = z.wrapping_mul((0x1_0000 + b) >> 8) / 0x1_0000;
        b = b.wrapping_add(z);
        if z <= 0 {
            break;
        }
    }
    v.0 = if sx { -i64::from(u) } else { i64::from(u) };
    v.1 = if sy { -i64::from(w) } else { i64::from(w) };
    let dot = u
        .wrapping_mul(x)
        .wrapping_add(w.wrapping_mul(y))
        .cast_signed();
    let mut length = 0x1_0000_u32.wrapping_add((dot / 0x1_0000).cast_unsigned());
    if shift > 0 {
        length = length.wrapping_add(1 << (shift - 1)) >> shift;
    } else {
        length <<= -shift;
    }
    i64::from(length)
}

/// The length of `(x, y)`, by 22 CORDIC rotations of the vector onto the x
/// axis.
pub(crate) fn hypot(x: i64, y: i64) -> i64 {
    if x == 0 {
        return y.abs();
    }
    if y == 0 {
        return x.abs();
    }
    // Bring the larger component's top bit to bit 29.
    let top = msb((x.unsigned_abs() | y.unsigned_abs()) as u32).cast_signed();
    let (mut x, mut y, shift) = if top <= 29 {
        let s = 29 - top;
        (
            (x.cast_unsigned() << s).cast_signed(),
            (y.cast_unsigned() << s).cast_signed(),
            s,
        )
    } else {
        let s = top - 29;
        (x >> s, y >> s, -s)
    };
    // Rotate into the right half plane, then onto the axis, keeping only the
    // magnitude.
    if y > x {
        if y > -x {
            (x, y) = (y, -x);
        } else {
            (x, y) = (-x, -y);
        }
    } else if y < -x {
        (x, y) = (-y, x);
    }
    let mut b = 1_i64;
    for i in 1..23 {
        let (dx, dy) = ((y + b) >> i, (x + b) >> i);
        if y > 0 {
            (x, y) = (x + dx, y - dy);
        } else {
            (x, y) = (x - dx, y + dy);
        }
        b <<= 1;
    }
    // Undo the CORDIC gain: times 0.858785336480436 in 0.32.
    let magnitude = ((x.unsigned_abs() * 0xDBD9_5B16 + 0x4000_0000) >> 32).cast_signed();
    let x = if x < 0 { -magnitude } else { magnitude };
    if shift > 0 {
        (x + (1 << (shift - 1))) >> shift
    } else {
        i64::from((x as u32) << -shift)
    }
}

#[cfg(test)]
mod tests {
    use super::{ONE, sqrt_fixed};
    use super::{div_fix, hypot, mul_div, mul_fix, norm_len, pix_ceil, pix_floor, pix_round};

    #[test]
    fn products_and_quotients_round_half_away_from_zero() {
        assert_eq!(mul_fix(3, 0x8000), 2);
        assert_eq!(mul_fix(-3, 0x8000), -2);
        assert_eq!(mul_fix(1, 0x7fff), 0);
        assert_eq!(mul_fix(-1, 0x8000), -1);
        assert_eq!(mul_fix(1000, 0x10_624D), 16384);
        assert_eq!(div_fix(1, 3), 21845);
        assert_eq!(div_fix(-1, 3), -21845);
        assert_eq!(div_fix(16384, 0), 0x7fff_ffff);
        assert_eq!(mul_div(7, 1, 2), 4);
        assert_eq!(mul_div(-7, 1, 2), -4);
        assert_eq!([pix_round(95), pix_floor(-1), pix_ceil(1)], [64, -64, 64]);
    }

    #[test]
    fn square_roots_and_lengths() {
        assert_eq!(sqrt_fixed(4 << 16), 2 << 16);
        assert_eq!(sqrt_fixed(2 << 16), 92682);
        assert_eq!(hypot(3 << 16, 4 << 16), 5 << 16);
        assert_eq!(hypot(0, -7), 7);
        assert_eq!(hypot(ONE, ONE), 92682);
        let mut v = (3 << 16, -(4 << 16));
        assert_eq!(norm_len(&mut v), 5 << 16);
        assert_eq!(v, (39321, -52429));
        let mut axis = (0, -9);
        assert_eq!((norm_len(&mut axis), axis), (9, (0, -ONE)));
    }
}
