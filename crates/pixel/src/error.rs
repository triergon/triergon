use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Width/height zero, odd, or beyond the 12-bit MPEG-1 size fields.
    Geometry { width: u32, height: u32 },
    /// The RGBA slice is shorter than `width * height * 4`.
    InputLength { expected: usize, actual: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Geometry { width, height } => {
                write!(f, "unsupported geometry {width}x{height}")
            }
            Error::InputLength { expected, actual } => {
                write!(f, "RGBA input holds {actual} bytes, {expected} required")
            }
        }
    }
}

impl std::error::Error for Error {}
