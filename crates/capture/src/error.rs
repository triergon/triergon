use std::fmt;

pub type Result<T> = std::result::Result<T, CaptureError>;

/// Every failure is explicit; nothing is retried or replaced silently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// Width/height zero, odd, or beyond the 12-bit MPEG-1 size fields.
    Geometry { width: u32, height: u32 },
    /// The RGBA slice is shorter than `width * height * 4`.
    InputLength { expected: usize, actual: usize },
    /// Bit rate outside the encodable 18-bit/400 bps sequence-header range.
    BitRate(u64),
    /// An intra period must contain at least one picture.
    Gop(u32),
    /// Input picture geometry differs from the encoder's fixed geometry.
    PictureGeometry {
        expected: crate::Geometry,
        actual: crate::Geometry,
    },
    /// A declared multiplex field cannot be represented by the supported wire profile.
    MuxConfig(&'static str),
    /// The output sink refused or partially accepted the bytes of a frame.
    Sink,
    /// A previous failure poisoned the session; the caller must open a new one.
    Closed,
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Geometry { width, height } => {
                write!(f, "unsupported geometry {width}x{height}")
            }
            CaptureError::InputLength { expected, actual } => {
                write!(f, "RGBA input holds {actual} bytes, {expected} required")
            }
            CaptureError::BitRate(bps) => write!(f, "unsupported bit rate {bps} bps"),
            CaptureError::Gop(g) => write!(f, "unsupported intra period {g}"),
            CaptureError::PictureGeometry { expected, actual } => write!(
                f,
                "picture geometry {}x{} does not match {}x{}",
                actual.width(),
                actual.height(),
                expected.width(),
                expected.height()
            ),
            CaptureError::MuxConfig(field) => write!(f, "unsupported mux configuration: {field}"),
            CaptureError::Sink => f.write_str("output sink failed"),
            CaptureError::Closed => f.write_str("session closed after an earlier failure"),
        }
    }
}

impl std::error::Error for CaptureError {}
