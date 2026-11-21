//! Decoder errors.
use triergon_codec::StorageError;

/// Why a decoder refused a configuration or an access unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A feature this decoder does not implement.
    Unsupported(&'static str),
    /// Malformed data.
    Invalid(&'static str),
    /// The frame's CRC-16 does not match (only with [`CrcPolicy::Reject`](crate::CrcPolicy::Reject)).
    CrcMismatch,
    /// The storage budget refused an allocation.
    Budget(StorageError),
}

impl From<StorageError> for DecodeError {
    fn from(error: StorageError) -> Self {
        DecodeError::Budget(error)
    }
}

impl From<DecodeError> for triergon_codec::Error {
    /// The workspace error of the same class. A frame CRC mismatch is invalid data; a
    /// refused storage charge is the budget's own error.
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Unsupported(feature) => Self::Unsupported(feature),
            DecodeError::Invalid(detail) => Self::Invalid(detail),
            DecodeError::CrcMismatch => Self::Invalid("frame CRC-16"),
            DecodeError::Budget(error) => error,
        }
    }
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::Unsupported(what) => write!(f, "unsupported: {what}"),
            DecodeError::Invalid(what) => write!(f, "invalid data: {what}"),
            DecodeError::CrcMismatch => write!(f, "frame CRC-16 mismatch"),
            DecodeError::Budget(error) => write!(f, "storage budget: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}
