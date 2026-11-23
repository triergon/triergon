use crate::{Backpressure, Step};
use std::{fmt, io};

/// The error of every format, codec, playback and core operation.
///
/// It is `Copy` and small: a class, with the numbers or the `&'static str` that
/// tell one cause from another. Building one allocates and formats nothing.
///
/// Use [`Limit`](Self::Limit) for a permanent byte bound, including configured
/// byte limits, with the requested and maximum byte counts. Use
/// [`Policy`](Self::Policy) to name a configured bound measured in other units,
/// such as a count, depth or amount of work. Use [`Backpressure`](Self::Backpressure)
/// only when the caller can release capacity and retry the same operation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The input breaks its format's syntax or a caller broke a precondition;
    /// the text names what.
    Invalid(&'static str),
    /// A byte request above the applicable capacity.
    Limit {
        /// Bytes asked for.
        requested: usize,
        /// The most a single request may take.
        maximum: usize,
    },
    /// A named limit measured in units other than bytes.
    Policy(&'static str),
    /// The requested bytes and the space currently available for them.
    Backpressure(Backpressure),
    /// The allocator refused memory the pool had granted.
    Allocation,
    /// A size, offset or time does not fit its integer type.
    Overflow,
    /// The source ended inside a structure, or before its declared length.
    Truncated,
    /// The caller reported a failed source ([`SourceFailure::Transport`](crate::SourceFailure)).
    Transport,
    /// The cancellation flag was set, or the caller reported
    /// [`SourceFailure::Cancelled`](crate::SourceFailure).
    Cancelled,
    /// An input response carries the token of an earlier
    /// [`InputCursor::reset`](crate::InputCursor::reset) or of another cursor of
    /// the same pool.
    StaleGeneration {
        /// The cursor's generation.
        expected: u64,
        /// The response's generation.
        received: u64,
    },
    /// An input response answers a range request the cursor has since replaced.
    StaleRequest {
        /// The cursor's request number.
        expected: u64,
        /// The response's request number.
        received: u64,
    },
    /// An input response's token or bytes belong to another [`Budget`](crate::Budget).
    ForeignPool,
    /// An input response names another [`SourceId`](crate::SourceId) than the
    /// cursor reads.
    SourceMismatch,
    /// Valid input that uses a feature this implementation does not have; the
    /// text names it.
    Unsupported(&'static str),
    /// A [`blocking`](crate::blocking) adapter's source failed. An unexpected
    /// end of file is [`Error::Truncated`] instead.
    Io(io::ErrorKind),
    /// The owner was closed, or an earlier failure left it unusable.
    Closed,
}

/// A result whose error is [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "invalid data: {reason}"),
            Self::Limit { requested, maximum } => {
                write!(f, "capacity request {requested} exceeds policy {maximum}")
            }
            Self::Backpressure(Backpressure {
                requested,
                available,
            }) => {
                write!(
                    f,
                    "capacity request {requested} needs released output; {available} available"
                )
            }
            Self::Policy(limit) => write!(f, "resource limit: {limit}"),
            Self::Allocation => f.write_str("allocation failed"),
            Self::Overflow => f.write_str("arithmetic overflow"),
            Self::Truncated => f.write_str("truncated input"),
            Self::Transport => f.write_str("source transport failed"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::StaleGeneration { expected, received } => {
                write!(f, "stale input generation {received}; expected {expected}")
            }
            Self::StaleRequest { expected, received } => {
                write!(f, "stale input request {received}; expected {expected}")
            }
            Self::ForeignPool => f.write_str("input belongs to another allocation pool"),
            Self::SourceMismatch => f.write_str("input belongs to another source"),
            Self::Unsupported(feature) => write!(f, "unsupported: {feature}"),
            Self::Io(kind) => write!(f, "I/O: {kind}"),
            Self::Closed => f.write_str("closed"),
        }
    }
}

// Debug prints what a derive would print for `Backpressure` as a struct variant,
// `Backpressure { requested, available }`, the text error traces carry: formatted
// through a derived twin of the enum.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Shown::from(*self).fmt(f)
    }
}

/// [`Error`] as its `Debug` shows it.
#[derive(Debug)]
#[expect(dead_code, reason = "read only through the derived Debug")]
enum Shown {
    Invalid(&'static str),
    Limit { requested: usize, maximum: usize },
    Policy(&'static str),
    Backpressure { requested: usize, available: usize },
    Allocation,
    Overflow,
    Truncated,
    Transport,
    Cancelled,
    StaleGeneration { expected: u64, received: u64 },
    StaleRequest { expected: u64, received: u64 },
    ForeignPool,
    SourceMismatch,
    Unsupported(&'static str),
    Io(io::ErrorKind),
    Closed,
}

impl From<Error> for Shown {
    fn from(error: Error) -> Self {
        match error {
            Error::Invalid(reason) => Self::Invalid(reason),
            Error::Limit { requested, maximum } => Self::Limit { requested, maximum },
            Error::Policy(limit) => Self::Policy(limit),
            Error::Backpressure(Backpressure {
                requested,
                available,
            }) => Self::Backpressure {
                requested,
                available,
            },
            Error::Allocation => Self::Allocation,
            Error::Overflow => Self::Overflow,
            Error::Truncated => Self::Truncated,
            Error::Transport => Self::Transport,
            Error::Cancelled => Self::Cancelled,
            Error::StaleGeneration { expected, received } => {
                Self::StaleGeneration { expected, received }
            }
            Error::StaleRequest { expected, received } => Self::StaleRequest { expected, received },
            Error::ForeignPool => Self::ForeignPool,
            Error::SourceMismatch => Self::SourceMismatch,
            Error::Unsupported(feature) => Self::Unsupported(feature),
            Error::Io(kind) => Self::Io(kind),
            Error::Closed => Self::Closed,
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// The step a reader returns for this error: backpressure becomes
    /// [`Step::Backpressure`], which the caller lifts by releasing output;
    /// every other error stays an error.
    #[inline]
    pub fn into_step<T>(self) -> Result<Step<T, Backpressure>> {
        match self {
            Self::Backpressure(pressure) => Ok(Step::Backpressure(pressure)),
            error => Err(error),
        }
    }
}

impl From<Backpressure> for Error {
    #[inline]
    fn from(pressure: Backpressure) -> Self {
        Self::Backpressure(pressure)
    }
}

impl From<crate::SourceFailure> for Error {
    /// The error every later read of a failed input returns.
    #[inline]
    fn from(failure: crate::SourceFailure) -> Self {
        match failure {
            crate::SourceFailure::Transport => Self::Transport,
            crate::SourceFailure::Cancelled => Self::Cancelled,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        match value.kind() {
            io::ErrorKind::UnexpectedEof => Self::Truncated,
            kind => Self::Io(kind),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use crate::{Backpressure, Step};

    #[test]
    fn backpressure_keeps_its_text_and_becomes_a_step() {
        let pressure = Backpressure {
            requested: 3,
            available: 4,
        };
        let error = Error::from(pressure);
        assert_eq!(
            format!("{error:?}"),
            "Backpressure { requested: 3, available: 4 }"
        );
        assert_eq!(
            error.to_string(),
            "capacity request 3 needs released output; 4 available"
        );
        assert!(matches!(
            error.into_step::<()>(),
            Ok(Step::Backpressure(step)) if step == pressure
        ));
        assert_eq!(
            Error::Truncated.into_step::<()>().err(),
            Some(Error::Truncated)
        );
    }

    #[test]
    fn an_error_is_24_bytes_at_most_and_adds_none_to_a_result() {
        assert!(size_of::<Error>() <= 24);
        assert_eq!(size_of::<crate::Result<()>>(), size_of::<Error>());
        assert_eq!(size_of::<crate::Result<usize>>(), size_of::<Error>());
    }
}
