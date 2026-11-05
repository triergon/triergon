//! Typed decoder errors.

use core::fmt;

/// Decoder error. Unsupported features are named so callers can tell a gap in the
/// selected feature set from corrupt input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// A valid H.265 feature outside the selected Main / Main 10 / Main Still Picture set.
    Unsupported(&'static str),
    /// The bitstream violates a syntax or semantic constraint.
    InvalidData(&'static str),
    /// A resource cap (picture size, DPB size, level limit) refused the stream.
    ResourceLimit(&'static str),
    /// A referenced parameter set has not been received.
    MissingParameterSet(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Unsupported(s) => write!(f, "unsupported HEVC feature: {s}"),
            DecodeError::InvalidData(s) => write!(f, "invalid HEVC data: {s}"),
            DecodeError::ResourceLimit(s) => write!(f, "HEVC resource limit: {s}"),
            DecodeError::MissingParameterSet(s) => write!(f, "missing HEVC parameter set: {s}"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub type Result<T> = core::result::Result<T, DecodeError>;
