use std::fmt;
use triergon_pixel::Geometry;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Bit rate outside the encodable 18-bit/400 bps sequence-header range.
    BitRate(u64),
    /// An intra period must contain at least one picture.
    Gop(u32),
    /// Input picture geometry differs from the encoder's fixed geometry.
    PictureGeometry {
        expected: Geometry,
        actual: Geometry,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BitRate(bps) => write!(f, "unsupported bit rate {bps} bps"),
            Error::Gop(g) => write!(f, "unsupported intra period {g}"),
            Error::PictureGeometry { expected, actual } => write!(
                f,
                "picture geometry {}x{} does not match {}x{}",
                actual.width(),
                actual.height(),
                expected.width(),
                expected.height()
            ),
        }
    }
}

impl std::error::Error for Error {}
