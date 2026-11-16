//! Integer division by a value known only at run time.
//!
//! The 32-bit ARM targets have no integer divide instruction in their baseline, and the
//! library routine that stands in for it costs several times a VFP double division
//! there. A double holds both 32-bit operands exactly, and its rounded quotient never
//! reaches the next integer: with `a = k b + r`, `0 < r < b`, the exact quotient lies
//! `r / b >= 1 / b` below `k + 1`, while the rounding error is at most `k 2^-53`, smaller
//! because `k b <= |a| < 2^32`. So the truncated double quotient is the integer one.
//! Other targets divide as integers.

/// `a / b` rounded towards zero, for `b > 0`.
#[inline(always)]
#[must_use]
pub fn divide(a: i32, b: i32) -> i32 {
    debug_assert!(b > 0, "divisor {b}");
    #[cfg(all(target_arch = "arm", target_abi = "eabihf"))]
    {
        (f64::from(a) / f64::from(b)) as i32
    }
    #[cfg(not(all(target_arch = "arm", target_abi = "eabihf")))]
    {
        a / b
    }
}

/// `a / b` rounded towards zero, for `b > 0`.
#[inline(always)]
#[must_use]
pub fn divide_u32(a: u32, b: u32) -> u32 {
    debug_assert!(b > 0, "divisor {b}");
    #[cfg(all(target_arch = "arm", target_abi = "eabihf"))]
    {
        (f64::from(a) / f64::from(b)) as u32
    }
    #[cfg(not(all(target_arch = "arm", target_abi = "eabihf")))]
    {
        a / b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotients_match_integer_division() {
        let edges = [1, 2, 3, 7, 255, 256, 32_767, 65_535, i32::MAX - 1, i32::MAX];
        let mut seed = 0x9e37_79b9_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..1_000_000 {
            let (a, b) = (next(), next().max(1));
            assert_eq!(divide_u32(a, b), a / b, "{a} / {b}");
            let (a, b) = (a as i32, (b >> 1).max(1) as i32);
            assert_eq!(divide(a, b), a / b, "{a} / {b}");
        }
        for &b in &edges {
            for a in [i32::MIN, i32::MIN + 1, -b, -1, 0, 1, b - 1, b, i32::MAX] {
                assert_eq!(divide(a, b), a / b, "{a} / {b}");
            }
            let b = b as u32;
            for a in [0, 1, b - 1, b, b + 1, u32::MAX - 1, u32::MAX] {
                assert_eq!(divide_u32(a, b), a / b, "{a} / {b}");
            }
        }
    }
}
