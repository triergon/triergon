use std::fmt;

/// A result whose error is [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Why a picture could not be described or converted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A width or height that is zero, odd or above 4095, the limit of the
    /// 12-bit MPEG-1 size fields.
    Geometry {
        /// The width asked for, in pixels.
        width: u32,
        /// The height asked for, in pixels.
        height: u32,
    },
    /// The RGBA slice is shorter than `width * height * 4` bytes.
    InputLength {
        /// Bytes the geometry needs.
        expected: usize,
        /// Bytes given.
        actual: usize,
    },
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
