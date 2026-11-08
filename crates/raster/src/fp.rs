//! Floating-point operations with the exact semantics of the C expressions libass
//! is built from. The reference is compiled without floating-point contraction,
//! and Rust never contracts, so expressions are written in the C source's order
//! and grouping; these helpers cover the macros and library calls whose
//! semantics differ from Rust's own.

/// `a > b ? a : b`, the C `FFMAX` macro, including its NaN behaviour.
#[inline(always)]
pub(crate) fn max(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

/// `a > b ? b : a`, the C `FFMIN` macro, including its NaN behaviour.
#[inline(always)]
pub(crate) fn min(a: f64, b: f64) -> f64 {
    if a > b { b } else { a }
}

/// C `lrint` in the default rounding mode: nearest, ties to even. Out-of-range
/// values saturate and NaN becomes 0, as AArch64's `frintx` + `fcvtzs` do.
#[inline(always)]
pub(crate) fn lrint(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `lrint` narrowed to 32 bits, for values the caller has bounded.
#[inline(always)]
pub(crate) fn lrint32(x: f64) -> i32 {
    lrint(x) as i32
}

/// The 2⁶⁴ scale that lifts subnormals into the normal range.
const TWO_64: f64 = 18_446_744_073_709_551_616.0;

/// C `frexp`: `x = mantissa · 2^exponent` with `|mantissa|` in [0.5, 1). Zero,
/// infinities and NaN return themselves with exponent 0.
pub(crate) fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    if biased == 0 {
        let (mantissa, exponent) = frexp(x * TWO_64);
        return (mantissa, exponent - 64);
    }
    let mantissa = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
    (mantissa, biased - 1022)
}

/// C `ldexp`: `x · 2^n`, rounded once.
pub(crate) fn ldexp(mut x: f64, mut n: i32) -> f64 {
    // Scale in steps of normal powers of two; the step into the subnormal range
    // first lifts by 2^53 so only the final multiplication rounds.
    let power = |e: i32| f64::from_bits(((e + 1023) as u64) << 52);
    if n > 1023 {
        x *= power(1023);
        n -= 1023;
        if n > 1023 {
            x *= power(1023);
            n = (n - 1023).min(1023);
        }
    } else if n < -1022 {
        x *= power(-1022) * power(53);
        n += 1022 - 53;
        if n < -1022 {
            x *= power(-1022) * power(53);
            n = (n + 1022 - 53).max(-1022);
        }
    }
    x * power(n)
}

#[cfg(test)]
mod tests {
    use super::{frexp, ldexp, lrint, max, min};

    #[test]
    fn frexp_splits_normals_subnormals_and_specials() {
        assert_eq!(frexp(1.0), (0.5, 1));
        assert_eq!(frexp(-12.0), (-0.75, 4));
        assert_eq!(frexp(0.3), (0.6, -1));
        assert_eq!(frexp(f64::MIN_POSITIVE / 4.0), (0.5, -1023));
        assert_eq!(frexp(0.0), (0.0, 0));
        assert_eq!(frexp(f64::INFINITY).1, 0);
    }

    #[test]
    fn ldexp_scales_exactly_and_rounds_once() {
        assert_eq!(ldexp(0.75, 4), 12.0);
        assert_eq!(ldexp(1.0, -1074), f64::from_bits(1));
        assert_eq!(ldexp(1.0, 1024), f64::INFINITY);
        assert_eq!(ldexp(3.0, -1075), f64::from_bits(2));
        for x in [0.1, 1.5, 1e300, 3e-310] {
            let (m, e) = frexp(x);
            assert_eq!(ldexp(m, e), x);
        }
    }

    #[test]
    fn lrint_rounds_ties_to_even_and_saturates() {
        assert_eq!(lrint(2.5), 2);
        assert_eq!(lrint(-3.5), -4);
        assert_eq!(lrint(f64::NAN), 0);
        assert_eq!(lrint(1e300), i64::MAX);
    }

    #[test]
    fn min_max_follow_the_c_macros_on_nan() {
        assert_eq!(max(f64::NAN, 1.0).to_bits(), 1.0f64.to_bits());
        assert!(max(1.0, f64::NAN).is_nan());
        assert!(min(f64::NAN, 1.0).is_nan());
    }
}
