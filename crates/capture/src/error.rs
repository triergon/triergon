use std::fmt;

pub type Result<T> = std::result::Result<T, CaptureError>;

/// Every failure is explicit; nothing is retried or replaced silently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// Geometry or RGBA input length.
    Pixel(triergon_pixel::Error),
    /// Rate, intra period or picture geometry.
    Encoder(triergon_codec_mpeg1::Error),
    /// A declared multiplex field cannot be represented by the supported wire profile.
    Mux(triergon_core::Error),
    /// The output sink refused or partially accepted the bytes of a frame.
    Sink,
    /// A previous failure poisoned the session; the caller must open a new one.
    Closed,
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Pixel(e) => e.fmt(f),
            CaptureError::Encoder(e) => e.fmt(f),
            CaptureError::Mux(e) => write!(f, "multiplex configuration: {e}"),
            CaptureError::Sink => f.write_str("output sink failed"),
            CaptureError::Closed => f.write_str("session closed after an earlier failure"),
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<triergon_pixel::Error> for CaptureError {
    fn from(e: triergon_pixel::Error) -> Self {
        Self::Pixel(e)
    }
}
impl From<triergon_codec_mpeg1::Error> for CaptureError {
    fn from(e: triergon_codec_mpeg1::Error) -> Self {
        Self::Encoder(e)
    }
}
impl From<triergon_core::Error> for CaptureError {
    fn from(e: triergon_core::Error) -> Self {
        Self::Mux(e)
    }
}
