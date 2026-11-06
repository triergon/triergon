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
    U8 {
        data: &'a [u8],
        stride: usize,
        offset: usize,
    },
    U16 {
        data: &'a [u16],
        stride: usize,
        offset: usize,
    },
}

impl Plane<'_> {
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
    pub full_range: bool,
}

/// A decoded 4:2:0 picture in output order, borrowed from the decoder.
#[derive(Clone, Copy, Debug)]
pub struct PictureView<'a> {
    /// Cropped display size.
    pub width: usize,
    pub height: usize,
    pub bit_depth: u8,
    pub planes: [Plane<'a>; 3],
    pub pts: Option<i64>,
    /// Sample aspect ratio; (0, 0) or (1, 1) when square/unknown.
    pub sar: (u32, u32),
    pub colour: Colour,
}
