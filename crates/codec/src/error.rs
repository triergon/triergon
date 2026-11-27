//! The refusal of a decoder whose errors need no detail of their own codec.

use core::fmt;

use triergon_core::Error;

/// Why a decoder refused a configuration or an access unit. Its variants are the
/// workspace [`Error`]'s classes of the same names; a codec crate whose decoder has more
/// to say (byte counts, a named feature) has its own enum, which names these classes the
/// same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A feature the decoder does not implement; the text names it.
    Unsupported(&'static str),
    /// The input breaks its format's syntax or semantics; the text names the failing
    /// syntax element or check.
    Invalid(&'static str),
    /// The stream exceeds a cap the decoder sets (picture size, reference pictures,
    /// channels); the text names it.
    Policy(&'static str),
    /// Storage the stream needs does not fit the decoder's budget.
    Budget(Error),
}

impl From<Error> for DecodeError {
    /// A refused storage charge.
    #[inline]
    fn from(error: Error) -> Self {
        DecodeError::Budget(error)
    }
}

impl From<DecodeError> for Error {
    /// The workspace error of the same class; a refused storage charge is the budget's
    /// own error.
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Unsupported(feature) => Error::Unsupported(feature),
            DecodeError::Invalid(detail) => Error::Invalid(detail),
            DecodeError::Policy(limit) => Error::Policy(limit),
            DecodeError::Budget(error) => error,
        }
    }
}

impl fmt::Display for DecodeError {
    /// The workspace error's text, a refused storage charge prefixed `storage budget: `.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DecodeError::Budget(error) => write!(f, "storage budget: {error}"),
            error => Error::from(error).fmt(f),
        }
    }
}

impl std::error::Error for DecodeError {}
