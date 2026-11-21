//! Typed decoder errors.

use core::fmt;
use triergon_bitstream::cabac::InvalidOffset;
use triergon_codec::StorageError;

/// Why the decoder refused an access unit: the classes of [`triergon_codec::DecodeError`]
/// and a missing parameter set. Unsupported features are named so callers can tell a gap
/// in the selected feature set from corrupt input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A valid H.265 feature outside the selected Main / Main 10 / Main Still Picture set.
    Unsupported(&'static str),
    /// The bitstream violates a syntax or semantic constraint.
    Invalid(&'static str),
    /// A resource cap (picture size, DPB size, level limit) refused the stream.
    Policy(&'static str),
    /// A referenced parameter set has not been received.
    MissingParameterSet(&'static str),
    /// Storage the stream needs does not fit the decoder's budget.
    Budget(StorageError),
}

impl From<InvalidOffset> for DecodeError {
    fn from(_: InvalidOffset) -> Self {
        DecodeError::Invalid("arithmetic code offset out of range")
    }
}

impl From<StorageError> for DecodeError {
    fn from(error: StorageError) -> Self {
        DecodeError::Budget(error)
    }
}

impl From<DecodeError> for triergon_codec::Error {
    /// The workspace error of the same class. A missing parameter set is invalid data
    /// named by the set; a refused storage charge is the budget's own error.
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Unsupported(feature) => Self::Unsupported(feature),
            DecodeError::Invalid(detail) | DecodeError::MissingParameterSet(detail) => {
                Self::Invalid(detail)
            }
            DecodeError::Policy(limit) => Self::Policy(limit),
            DecodeError::Budget(error) => error,
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Unsupported(s) => write!(f, "unsupported HEVC feature: {s}"),
            DecodeError::Invalid(s) => write!(f, "invalid HEVC data: {s}"),
            DecodeError::Policy(s) => write!(f, "HEVC resource limit: {s}"),
            DecodeError::MissingParameterSet(s) => write!(f, "missing HEVC parameter set: {s}"),
            DecodeError::Budget(error) => write!(f, "storage budget: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub(crate) type Result<T> = core::result::Result<T, DecodeError>;

#[cfg(test)]
mod tests {
    use super::DecodeError;

    #[test]
    fn display_identifies_the_codec_and_error_class() {
        for (error, expected) in [
            (
                DecodeError::Unsupported("unequal luma/chroma bit depth"),
                "unsupported HEVC feature: unequal luma/chroma bit depth",
            ),
            (
                DecodeError::Invalid("bitstream overrun"),
                "invalid HEVC data: bitstream overrun",
            ),
            (
                DecodeError::Policy("picture size"),
                "HEVC resource limit: picture size",
            ),
            (
                DecodeError::MissingParameterSet("SPS"),
                "missing HEVC parameter set: SPS",
            ),
        ] {
            assert_eq!(error.to_string(), expected);
        }
    }
}
