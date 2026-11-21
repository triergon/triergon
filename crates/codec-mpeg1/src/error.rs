use std::fmt;
use triergon_core::Error as StorageError;
use triergon_pixel::Geometry;

/// The result of building an encoder or coding a picture.
pub type Result<T> = std::result::Result<T, Error>;

/// Why an encoder refused a configuration or a picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// `bit_rate` is zero, or `max_rate` is zero or above the 104,856,800 bit/s the
    /// sequence header's bit_rate field states as a rate; carries the refused rate in
    /// bit/s.
    BitRate(u32),
    /// `vbv_bits` is zero or above the 16,760,832 bits the sequence header's
    /// vbv_buffer_size field holds; carries the refused size.
    VbvBuffer(u32),
    /// `gop` is zero: an intra period holds at least one picture.
    Gop(u32),
    /// The input picture's geometry differs from the configured one.
    PictureGeometry {
        /// The configured geometry.
        expected: Geometry,
        /// The input picture's geometry.
        actual: Geometry,
    },
    /// Storage the encoder needs does not fit its budget: a limit when it is built,
    /// backpressure before a picture is coded.
    Budget(StorageError),
}

impl From<StorageError> for Error {
    fn from(error: StorageError) -> Self {
        Error::Budget(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BitRate(bps) => write!(f, "unsupported bit rate {bps} bps"),
            Error::VbvBuffer(bits) => write!(f, "unsupported VBV buffer size {bits} bits"),
            Error::Gop(g) => write!(f, "unsupported intra period {g}"),
            Error::PictureGeometry { expected, actual } => write!(
                f,
                "picture geometry {}x{} does not match {}x{}",
                actual.width(),
                actual.height(),
                expected.width(),
                expected.height()
            ),
            Error::Budget(error) => write!(f, "storage budget: {error}"),
        }
    }
}

impl std::error::Error for Error {}
