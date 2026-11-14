//! Units and comparisons as libass's renderer writes them.

/// `d6_to_double`: 26.6 to pixels.
pub(super) fn d6_to_double(x: i32) -> f64 {
    f64::from(x) / 64.0
}

/// libass's `FFMAX` on doubles: `a > b ? a : b`, which keeps `b` when either
/// is NaN.
pub(super) fn fmax(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}
