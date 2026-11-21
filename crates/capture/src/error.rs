use std::fmt;

/// The result of opening a session or capturing a frame.
pub type Result<T> = std::result::Result<T, CaptureError>;

/// Why a capture session refused to open or to capture a frame. Nothing is retried
/// or replaced silently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// The frame geometry, or an RGBA frame shorter than it.
    Pixel(triergon_pixel::Error),
    /// Rates or an intra period the encoder refuses, or encoder storage refused.
    Encoder(triergon_codec_mpeg1::Error),
    /// A multiplex field the writer cannot represent.
    Mux(triergon_core::Error),
    /// The sink refused or only partly wrote a frame's bytes.
    Sink,
    /// An earlier sink failure closed the session; open a new one.
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
