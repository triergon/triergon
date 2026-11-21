//! Borrowed views of decoded pictures, as decoders hand them to sessions.

/// One plane of a decoded picture; samples at the picture's bit depth.
///
/// Visible sample (x, y) is `data[offset + y * stride + x]`. Decoders should
/// hand over the whole decoded plane: `data` starting at the decoded origin
/// and `offset = crop_top * stride + crop_left` (chroma: both halved), so the
/// samples left of the crop window stay reachable. The letterbox needs them:
/// the reference decoder applies a left crop only as far as its buffer
/// alignment allows ([`crate::letterbox::Scaler`] reproduces that rule from the
/// offset). A view whose `data` starts at the visible origin (`offset < stride`)
/// is still valid but is treated as uncropped on the left.
#[derive(Clone, Copy, Debug)]
pub enum Plane<'a> {
    /// Samples of up to 8 bits.
    U8 {
        /// The decoded plane.
        data: &'a [u8],
        /// Samples from one row to the next.
        stride: usize,
        /// Index of visible sample (0, 0).
        offset: usize,
    },
    /// Samples of up to 16 bits, in the low bits.
    U16 {
        /// The decoded plane.
        data: &'a [u16],
        /// Samples from one row to the next.
        stride: usize,
        /// Index of visible sample (0, 0).
        offset: usize,
    },
}

impl Plane<'_> {
    /// Visible sample (x, y). Panics when it lies outside `data`.
    #[inline]
    pub fn sample(&self, x: usize, y: usize) -> u16 {
        match *self {
            Plane::U8 {
                data,
                stride,
                offset,
            } => u16::from(data[offset + y * stride + x]),
            Plane::U16 {
                data,
                stride,
                offset,
            } => data[offset + y * stride + x],
        }
    }
}

/// Colour description needed for YUV to RGB conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colour {
    /// ITU-T H.273 MatrixCoefficients (2 = unspecified).
    pub matrix: u8,
    /// Full range (0 to 255 at 8 bits) rather than studio range (luma 16 to
    /// 235, chroma 16 to 240).
    pub full_range: bool,
}

/// A decoded 4:2:0 picture in output order, borrowed from the decoder.
#[derive(Clone, Copy, Debug)]
pub struct PictureView<'a> {
    /// Visible width in luma samples, after cropping.
    pub width: usize,
    /// Visible height in luma rows, after cropping.
    pub height: usize,
    /// Bits per sample, 8 to 16.
    pub bit_depth: u8,
    /// Y, Cb and Cr; each chroma plane has half the width and height, rounded
    /// up.
    pub planes: [Plane<'a>; 3],
    /// Presentation time in the caller's ticks, passed through.
    pub pts: Option<i64>,
    /// Sample aspect ratio as (numerator, denominator); (0, 0) or (1, 1) when
    /// square or unknown.
    pub sar: (u32, u32),
    /// How to convert the samples to RGB.
    pub colour: Colour,
}
