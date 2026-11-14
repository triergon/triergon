//! The interpreter's arithmetic: 26.6 values in 32 bits, unit vectors in 2.14
//! and rounding by the round state.
//!
//! Observed rules (harness/fonts/probes): products and quotients are formed
//! in 64 bits and wrap to 32 (`arithmetic`); MUL rounds half away from zero,
//! DIV truncates (`arithmetic`); control values and other font-unit values
//! scale like outline points, rounding half away from zero (`control-values`).

/// A unit vector in 2.14, x and y.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Vector {
    pub(super) x: i32,
    pub(super) y: i32,
}

impl Vector {
    pub(super) const X: Self = Self { x: 0x4000, y: 0 };
    pub(super) const Y: Self = Self { x: 0, y: 0x4000 };

    /// Rotated a quarter turn counter-clockwise.
    pub(super) const fn perpendicular(self) -> Self {
        Self {
            x: -self.y,
            y: self.x,
        }
    }

    /// The projection of `(dx, dy)` onto the vector, a 26.6 distance.
    pub(super) fn project(self, dx: i32, dy: i32) -> i32 {
        dot14(dx, self.x, dy, self.y)
    }

    /// The dot product of two unit vectors, in 2.14.
    pub(super) fn dot(self, other: Self) -> i32 {
        dot14(self.x, other.x, self.y, other.y)
    }

    /// The unit vector along `(dx, dy)`, `None` for a zero vector.
    pub(super) fn along(dx: i32, dy: i32) -> Option<Self> {
        normalize(i64::from(dx), i64::from(dy))
    }
}

/// `(a b + c d) / 2^14`, rounded half away from zero, in 64 bits and wrapped
/// to 32.
fn dot14(a: i32, b: i32, c: i32, d: i32) -> i32 {
    let sum = i64::from(a) * i64::from(b) + i64::from(c) * i64::from(d);
    round_shift(sum, 14) as i32
}

/// `v / 2^shift` rounded half away from zero.
const fn round_shift(v: i64, shift: u32) -> i64 {
    let half = 1 << (shift - 1);
    if v < 0 {
        -((half - v) >> shift)
    } else {
        (v + half) >> shift
    }
}

/// The unit vector of `(x, y)` in 2.14, `None` for a zero vector: the
/// components are scaled so the larger has 19 significant bits, the length
/// taken as the integer square root of their squares, and each component
/// divided by it in 17 fractional bits, rounded, then truncated to 14. This
/// gives the reference's unit vectors for most directions but not for all:
/// probe `vectors` records some it does not give.
fn normalize(x: i64, y: i64) -> Option<Vector> {
    let largest = x.unsigned_abs().max(y.unsigned_abs());
    if largest == 0 {
        return None;
    }
    let bits = 64 - largest.leading_zeros();
    let shift = 19 - i64::from(bits);
    let scale = |v: i64| {
        if shift >= 0 {
            v << shift
        } else {
            let magnitude = v.unsigned_abs() >> -shift;
            if v < 0 {
                -(magnitude as i64)
            } else {
                magnitude as i64
            }
        }
    };
    let (x, y) = (scale(x), scale(y));
    let length = isqrt((x * x + y * y).unsigned_abs()) as i64;
    let unit = |v: i64| {
        let q = ((v.unsigned_abs() << 17) + (length as u64) / 2) / length as u64;
        let q = (q >> 3) as i32;
        if v < 0 { -q } else { q }
    };
    Some(Vector {
        x: unit(x),
        y: unit(y),
    })
}

/// The integer square root, rounded down.
fn isqrt(n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    let mut x = 1_u64 << (64 - n.leading_zeros()).div_ceil(2);
    loop {
        let next = (x + n / x) / 2;
        if next >= x {
            return x;
        }
        x = next;
    }
}

/// MUL: `a b / 64`, rounded half away from zero.
pub(super) fn mul(a: i32, b: i32) -> i32 {
    round_shift(i64::from(a) * i64::from(b), 6) as i32
}

/// DIV: `a 64 / b`, truncated; `None` for a zero divisor.
pub(super) fn div(a: i32, b: i32) -> Option<i32> {
    (b != 0).then(|| ((i64::from(a) << 6) / i64::from(b)) as i32)
}

/// `n / d`, rounded half away from zero; `d` must not be zero.
pub(super) fn div_round(n: i64, d: i64) -> i64 {
    let q = (n.unsigned_abs() + d.unsigned_abs() / 2) / d.unsigned_abs();
    if (n < 0) != (d < 0) {
        -(q as i64)
    } else {
        q as i64
    }
}

/// `a b / c` in 64 bits, rounded half away from zero and wrapped to 32;
/// `c` must not be zero.
pub(super) fn mul_div(a: i32, b: i32, c: i32) -> i32 {
    div_round(i64::from(a) * i64::from(b), i64::from(c)) as i32
}

/// A font-unit value at a 16.16 scale, in 26.6, rounded half away from zero
/// and wrapped to 32 bits.
pub(super) fn scale(units: i32, scale: i64) -> i32 {
    let product = i128::from(units) * i128::from(scale);
    let half = 1 << 15;
    let scaled = if product < 0 {
        -((half - product) >> 16)
    } else {
        (product + half) >> 16
    };
    scaled as i32
}

/// A round state: the predefined ones, or SROUND's and S45ROUND's period,
/// phase and threshold in 26.6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Round {
    Grid,
    HalfGrid,
    DoubleGrid,
    DownToGrid,
    UpToGrid,
    Off,
    Super {
        period: i32,
        phase: i32,
        threshold: i32,
        /// S45ROUND's, whose periods are not powers of two.
        diagonal: bool,
    },
}

impl Round {
    /// SROUND's (`diagonal` false) or S45ROUND's setting from the low byte of
    /// `n`.
    pub(super) fn custom(n: i32, diagonal: bool) -> Self {
        // One grid period in 26.6: a pixel, or the half diagonal of one.
        let grid = if diagonal { 45 } else { 64 };
        // Observed: the period the specifications reserve (3) is one grid
        // period (`round-extremes`).
        let period = match (n >> 6) & 3 {
            0 => grid / 2,
            2 => grid * 2,
            _ => grid,
        };
        let phase = match (n >> 4) & 3 {
            0 => 0,
            1 => period / 4,
            2 => period / 2,
            _ => period * 3 / 4,
        };
        let threshold = match n & 15 {
            0 => period - 1,
            t => (t - 4) * period / 8,
        };
        Self::Super {
            period,
            phase,
            threshold,
            diagonal,
        }
    }

    /// Rounds a distance, symmetrically about zero: a negative one as the
    /// negation of its magnitude's rounding. Observed: the arithmetic wraps
    /// in 32 bits, and a result whose sign differs from the distance's is
    /// replaced by the state's smallest result of that sign, 0 or the phase;
    /// SROUND takes the value down to a multiple of the period,
    /// S45ROUND towards zero (`round-extremes`).
    pub(super) fn apply(self, d: i32) -> i32 {
        // The smallest result of the state, and the rounding of `m`.
        let rounded = |m: i32| match self {
            Self::Grid => (0, m.wrapping_add(32) & !63),
            Self::HalfGrid => (32, (m & !63).wrapping_add(32)),
            Self::DoubleGrid => (0, m.wrapping_add(16) & !31),
            Self::DownToGrid => (0, m & !63),
            Self::UpToGrid => (0, m.wrapping_add(63) & !63),
            Self::Off => (0, m),
            Self::Super {
                period,
                phase,
                threshold,
                diagonal,
            } => {
                let v = m.wrapping_sub(phase).wrapping_add(threshold);
                let v = if diagonal {
                    v.wrapping_sub(v % period)
                } else {
                    v & -period
                };
                (phase, v.wrapping_add(phase))
            }
        };
        if d >= 0 {
            let (least, r) = rounded(d);
            if r < 0 { least } else { r }
        } else {
            let (least, r) = rounded(d.wrapping_neg());
            let r = r.wrapping_neg();
            if r > 0 { -least } else { r }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Round, Vector, div, mul, mul_div, scale};

    #[test]
    fn products_round_and_quotients_truncate_in_64_bits() {
        // Values the reference gives on 32-bit ARMv7 (probe `arithmetic`).
        assert_eq!(mul(96, 96), 144);
        assert_eq!(mul(1, 32), 1);
        assert_eq!(mul(-1, 32), -1);
        assert_eq!(mul(1, 31), 0);
        assert_eq!(mul(0x7FFF_FFFF, 2), 67_108_864);
        assert_eq!(mul(0x7FFF_FFFF, 0x7FFF_FFFF), -67_108_864);
        assert_eq!(mul(i32::MIN, -64), i32::MIN);
        assert_eq!(div(2, 3), Some(42));
        assert_eq!(div(-2, 3), Some(-42));
        assert_eq!(div(63, 128), Some(31));
        assert_eq!(div(0x7FFF_FFFF, 1), Some(-64));
        assert_eq!(div(1, 0), None);
        assert_eq!(mul_div(-7, 1, 2), -4);
    }

    #[test]
    fn font_units_scale_half_away_from_zero() {
        // Height 1000 at 2048 units per em (probe `control-values`).
        assert_eq!(scale(1234, 32000), 603);
        assert_eq!(scale(-999, 32000), -488);
        assert_eq!(scale(-1, 32768), -1);
    }

    #[test]
    fn round_states_are_symmetric_about_zero() {
        let all = |r: Round| [-97, -96, -32, -31, 0, 31, 32, 95, 97].map(|v| r.apply(v));
        assert_eq!(all(Round::Grid), [-128, -128, -64, 0, 0, 0, 64, 64, 128]);
        assert_eq!(
            all(Round::HalfGrid),
            [-96, -96, -32, -32, 32, 32, 32, 96, 96]
        );
        assert_eq!(
            all(Round::DoubleGrid),
            [-96, -96, -32, -32, 0, 32, 32, 96, 96]
        );
        assert_eq!(all(Round::DownToGrid), [-64, -64, 0, 0, 0, 0, 0, 64, 64]);
        assert_eq!(
            all(Round::UpToGrid),
            [-128, -128, -64, -64, 0, 64, 64, 128, 128]
        );
        assert_eq!(all(Round::Off), [-97, -96, -32, -31, 0, 31, 32, 95, 97]);
        // SROUND 01:00:1000: a pixel period, no phase, half threshold.
        assert_eq!(all(Round::custom(0x48, false)), all(Round::Grid));
        // SROUND 01:01:1000 rounds to quarter past each pixel.
        assert_eq!(Round::custom(0x58, false).apply(70), 80);
    }

    #[test]
    fn rounding_wraps_at_the_ends_of_32_bits() {
        // Values the reference gives on 32-bit ARMv7 (probe `round-extremes`).
        const MAX: i32 = i32::MAX;
        const MIN: i32 = i32::MIN;
        let grid = |v| Round::Grid.apply(v);
        assert_eq!([MAX, MAX - 20, MAX - 63].map(grid), [0, 0, MAX - 63]);
        assert_eq!([MIN, MIN + 1, MIN + 63].map(grid), [MIN, MIN, MIN + 64]);
        let half = |v| Round::HalfGrid.apply(v);
        assert_eq!([MAX, MIN, MIN + 1].map(half), [MAX - 31, -32, MIN + 32]);
        assert_eq!(Round::UpToGrid.apply(MIN + 63), MIN);
        assert_eq!(Round::custom(0x48, false).apply(MIN + 1), MIN);
        assert_eq!(Round::custom(0xC8, false).apply(MIN), MIN);
        assert_eq!(Round::custom(0xB0, false).apply(MAX), 96);
        assert_eq!(Round::custom(0xF8, false).apply(MAX), MAX - 15);
        assert_eq!(Round::custom(0xF8, false).apply(MIN), MIN + 16);
        let s45 = |n, v| Round::custom(n, true).apply(v);
        assert_eq!(
            [s45(0xB0, MAX), s45(0xB0, MIN), s45(0xB0, MIN + 63)],
            [67, -67, MIN + 61]
        );
        assert_eq!([s45(0xB8, MAX), s45(0xB8, MAX - 20)], [67, MAX - 60]);
        assert_eq!([s45(0x40, MAX - 20), s45(0x40, MIN + 20)], [0, 0]);
        assert_eq!(s45(0x40, MAX - 63), MAX - 37);
    }

    #[test]
    fn vectors_project_and_turn() {
        let diagonal = Vector { x: 11585, y: 11585 };
        // Probe `projections`: (2400, 800) on the diagonal.
        assert_eq!(diagonal.project(2400, 800), 2263);
        assert_eq!(diagonal.project(-1600, 400), -849);
        assert_eq!(Vector::X.perpendicular(), Vector::Y);
        // Probe `vectors`: lines in exact directions.
        assert_eq!(Vector::along(2400, 800), Some(Vector { x: 15543, y: 5181 }));
        assert_eq!(
            Vector::along(-5, 5),
            Some(Vector {
                x: -11585,
                y: 11585
            })
        );
        assert_eq!(Vector::along(0, -6), Some(Vector { x: 0, y: -16384 }));
        assert_eq!(Vector::along(0, 0), None);
    }
}
