//! The decoder boundary the sessions drive. The codec crates implement these
//! through small adapters; tests use scripted decoders.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    Ac3,
    Eac3,
    Aac,
    Dts,
}

/// One plane of a decoded picture; samples at the picture's bit depth.
///
/// Visible sample (x, y) is `data[offset + y * stride + x]`. Decoders should
/// hand over the whole decoded plane: `data` starting at the decoded origin
/// and `offset = crop_top * stride + crop_left` (chroma: both halved), so the
/// samples left of the crop window stay reachable. The letterbox needs them:
/// the reference decoder applies a left crop only as far as its buffer
/// alignment allows (`scale::Scaler` reproduces that rule from the offset).
/// A view whose `data` starts at the visible origin (`offset < stride`) is
/// still valid but is treated as uncropped on the left.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeFailure(pub String);

pub trait VideoDecode: Send {
    /// One Annex-B access unit with its caller timestamp.
    fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), DecodeFailure>;
    /// The next picture in output order, if ready.
    fn receive(&mut self, sink: &mut dyn FnMut(PictureView<'_>)) -> bool;
    /// End of stream: every remaining picture becomes receivable.
    fn flush(&mut self);
    /// Discontinuity: drop all decoder state except parameter sets.
    fn reset(&mut self);
}

/// Speaker position of one decoded plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    FrontLeft,
    FrontRight,
    FrontCenter,
    LowFrequency,
    BackLeft,
    BackRight,
    BackCenter,
    SideLeft,
    SideRight,
    FrontLeftOfCenter,
    FrontRightOfCenter,
    TopFrontLeft,
    TopFrontRight,
    LowFrequency2,
    /// Any other FFmpeg channel id. The reference stereo downmix drops the ones the
    /// decoders can emit (top centre/front centre/back/side, bottom front, wide).
    Other(u8),
}

impl Speaker {
    /// From the FFmpeg channel index used by every codec crate's layout.
    pub fn from_index(index: u8) -> Self {
        match index {
            0 => Self::FrontLeft,
            1 => Self::FrontRight,
            2 => Self::FrontCenter,
            3 => Self::LowFrequency,
            4 => Self::BackLeft,
            5 => Self::BackRight,
            6 => Self::FrontLeftOfCenter,
            7 => Self::FrontRightOfCenter,
            8 => Self::BackCenter,
            9 => Self::SideLeft,
            10 => Self::SideRight,
            12 => Self::TopFrontLeft,
            14 => Self::TopFrontRight,
            35 => Self::LowFrequency2,
            other => Self::Other(other),
        }
    }
}

/// One decoded audio access unit, planar float at full scale 1.0.
#[derive(Clone, Copy, Debug)]
pub struct AudioView<'a> {
    pub sample_rate: u32,
    pub speakers: &'a [Speaker],
    pub planes: &'a [Vec<f32>],
    pub samples: usize,
    /// Stream downmix metadata (AC-3/E-AC-3); `None` uses the defaults.
    pub levels: Option<crate::mix::MixLevels>,
}

pub trait AudioDecode: Send {
    fn decode(
        &mut self,
        au: &[u8],
        sink: &mut dyn FnMut(AudioView<'_>),
    ) -> Result<(), DecodeFailure>;
    fn reset(&mut self);
}
