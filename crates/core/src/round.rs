//! Rounding of floating-point values to integers.

/// The integer nearest to `x`, ties to even: C `lrint` in the default rounding
/// mode, narrowed to `i32`. Values beyond the `i32` range saturate and NaN is 0,
/// on every target. A coordinate in 26.6 fixed point is
/// `round_ties_even_i32(x * 64.0)`.
#[inline]
pub fn round_ties_even_i32(x: f64) -> i32 {
    x.round_ties_even() as i32
}

#[cfg(test)]
mod tests {
    use super::round_ties_even_i32;

    #[test]
    fn ties_go_to_even_and_extremes_saturate() {
        assert_eq!(
            [2.5, -3.5, 0.5, 1.5].map(round_ties_even_i32),
            [2, -4, 0, 2]
        );
        assert_eq!(round_ties_even_i32(1e300), i32::MAX);
        assert_eq!(round_ties_even_i32(-1e300), i32::MIN);
        assert_eq!(round_ties_even_i32(f64::NAN), 0);
    }
}
