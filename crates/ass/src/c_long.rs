//! C `long`, as the target's libass has it.
//!
//! libass reads a few values through `long`: `atoi` (the parameters of a
//! `Banner` or `Scroll` effect and a Matroska packet's ReadOrder) and the
//! `%d` conversions of `sscanf` (event times) go through `strtol`,
//! `parse_bool` calls `strtol` itself, and the karaoke split rounds with
//! `lrint`. `long` has 64 bits on LP64 targets and 32 on 32-bit ones, so a
//! value beyond `int` saturates at different bounds before it is narrowed.
//! These are the only results of the crate that depend on the width of
//! `long`, and every conversion through `long` is here. (Conversions from
//! `double` to `int` out of range are undefined in C and follow libass on
//! 32-bit ARM with GCC everywhere; see the crate's documentation.)

use core::ffi::c_long;

use crate::number::strtoll;

/// C's `strtol` in base 10: blanks, a sign and digits, saturating at `long`'s
/// range.
pub(crate) fn strtol(s: &[u8]) -> (c_long, usize) {
    let (value, used) = strtoll(s, 10);
    let saturated = if value < 0 { c_long::MIN } else { c_long::MAX };
    (c_long::try_from(value).unwrap_or(saturated), used)
}

/// A `long` narrowed to `int` as C compilers narrow it: its low 32 bits.
pub(crate) fn to_int(value: c_long) -> i32 {
    value as i32
}

/// C's `atoi`: `strtol` narrowed to `int`.
pub(crate) fn atoi(s: &[u8]) -> i32 {
    to_int(strtol(s).0)
}

/// C's `lrint` in the default rounding mode: the nearest integer, ties to
/// even, saturating at `long`'s range, and 0 for NaN.
pub(crate) fn lrint(value: f64) -> c_long {
    value.round_ties_even() as c_long
}

#[cfg(test)]
mod tests {
    use super::{atoi, c_long, lrint, strtol};

    /// What the target's `atoi` gives for `value`: saturated at `long`, then
    /// its low 32 bits.
    fn expected(value: i128) -> i32 {
        let long = value.clamp(i128::from(c_long::MIN), i128::from(c_long::MAX));
        long as i32
    }

    #[test]
    fn values_beyond_int_saturate_at_long_then_narrow() {
        assert_eq!(strtol(b" -42z"), (-42, 4));
        for (text, value) in [
            ("2147483647", 2_147_483_647_i128),
            ("4294967296", 4_294_967_296),
            ("99999999999", 99_999_999_999),
            ("-99999999999", -99_999_999_999),
            ("99999999999999999999", 99_999_999_999_999_999_999),
        ] {
            assert_eq!(atoi(text.as_bytes()), expected(value), "{text}");
        }
        assert_eq!(lrint(2.5), 2);
        assert_eq!(lrint(f64::INFINITY), c_long::MAX);
        assert_eq!(lrint(f64::NAN), 0);
    }
}
