//! Intermediate values for comparing Parametric Stereo.

use crate::Decoder;

/// The last QMF input captured for an active Parametric Stereo element.
///
/// Floats are interleaved real and imaginary values, by subband within each of 38
/// time slots. No slice is returned until an element has supplied those values.
pub fn ps_input(decoder: &Decoder) -> Option<&[f32]> {
    decoder
        .core
        .elements
        .iter()
        .filter_map(|element| element.sbr.as_ref())
        .map(|sbr| sbr.ps_input.as_slice())
        .find(|slots| !slots.is_empty())
}
