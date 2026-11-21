//! The decoder's typed errors.

use triergon_codec::StorageError;

/// Typed decode failure. The decoder never panics on malformed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A valid but unimplemented feature (named).
    Unsupported(&'static str),
    /// Malformed or inconsistent bitstream data (named check).
    InvalidData(&'static str),
    /// Input exceeds a resource bound.
    ResourceLimit(&'static str),
    /// Storage the frame needs does not fit the decoder's budget.
    Budget(StorageError),
}

impl From<StorageError> for DecodeError {
    fn from(error: StorageError) -> Self {
        DecodeError::Budget(error)
    }
}

impl From<DecodeError> for triergon_codec::Error {
    /// The workspace error of the same class; a refused storage charge is the budget's
    /// own error.
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Unsupported(feature) => Self::Unsupported(feature),
            DecodeError::InvalidData(detail) => Self::Invalid(detail),
            DecodeError::ResourceLimit(limit) => Self::Policy(limit),
            DecodeError::Budget(error) => error,
        }
    }
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::Unsupported(s) => write!(f, "unsupported DTS feature: {s}"),
            DecodeError::InvalidData(s) => write!(f, "invalid DTS data: {s}"),
            DecodeError::ResourceLimit(s) => write!(f, "DTS resource limit: {s}"),
            DecodeError::Budget(error) => write!(f, "storage budget: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}
