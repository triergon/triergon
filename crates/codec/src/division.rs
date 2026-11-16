//! Integer division by a value known only at run time.
//!
//! The 32-bit ARM targets have no integer divide instruction in their baseline, and the
//! library routine that stands in for it costs several times a VFP double division
//! there. A double holds both 32-bit operands exactly, and its rounded quotient never
//! reaches the next integer: with `a = k b + r`, `0 < r < b`, the exact quotient lies
//! `r / b >= 1 / b` below `k + 1`, while the rounding error is at most `k 2^-53`, smaller
//! because `k b <= |a| < 2^32`. So the truncated double quotient is the integer one.
//! Other targets divide as integers. A divisor shared by many dividends, such as a picture
//! width, is cheaper still as a multiplication on every target: [`Divisor`].

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

/// Division of many dividends by one divisor fixed at run time, as a multiplication and a
/// shift, on every target.
///
/// With `m = ceil(2^31 / d)`, so that `m d = 2^31 + e` with `0 <= e < d`, the product
/// `n m / 2^31 = n / d + n e / (d 2^31)` exceeds the exact quotient by less than `1 / d`
/// whenever `n d < 2^31`, and the fraction `(n mod d) / d` is at most `(d - 1) / d`: the
/// truncated product is `floor(n / d)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Divisor {
    d: u32,
    m: u32,
}

impl Divisor {
    /// The divisor `d`, at least 1 and below 2^31.
    #[must_use]
    pub fn new(d: u32) -> Self {
        assert!(d > 0 && d < 1 << 31, "divisor {d}");
        Self {
            d,
            m: (1_u32 << 31).div_ceil(d),
        }
    }

    /// The divisor.
    #[inline(always)]
    #[must_use]
    pub fn get(self) -> u32 {
        self.d
    }

    /// `(n / d, n % d)` for `n d < 2^31`.
    #[inline(always)]
    #[must_use]
    pub fn div_rem(self, n: u32) -> (u32, u32) {
        debug_assert!(
            u64::from(n) * u64::from(self.d) < 1 << 31,
            "{n} / {}",
            self.d
        );
        let q = ((u64::from(n) * u64::from(self.m)) >> 31) as u32;
        (q, n - q * self.d)
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

    #[test]
    fn divisor_matches_integer_division() {
        for d in (1..=4096).chain([65_535, 65_536, 1 << 20, (1 << 30) - 1]) {
            let divisor = Divisor::new(d);
            let limit = ((1_u64 << 31) - 1) / u64::from(d);
            // Both sides of every multiple up to 1024 d, and the top of the range.
            let near = (0..=1024_u64.min(limit / u64::from(d) + 1))
                .flat_map(|k| [k * u64::from(d), (k * u64::from(d)).wrapping_sub(1)]);
            for n in near
                .chain([limit, limit.saturating_sub(1)])
                .filter(|&n| n <= limit)
            {
                let n = n as u32;
                assert_eq!(divisor.div_rem(n), (n / d, n % d), "{n} / {d}");
            }
        }
    }
}
