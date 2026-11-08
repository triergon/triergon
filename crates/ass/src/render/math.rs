//! libass's 26.6 conversion back to pixels; pixels to 26.6 is
//! [`triergon_raster::pixels_26_6`].

/// `d6_to_double`: 26.6 to pixels.
pub(super) fn d6_to_double(x: i32) -> f64 {
    f64::from(x) / 64.0
}
