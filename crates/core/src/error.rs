use std::{fmt, io};

/// The one error type of every format, playback and core operation. Variants
/// keep their typed detail; nothing converts it away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
    /// Live capacity request above the budget policy.
    Limit {
        requested: usize,
        maximum: usize,
    },
    /// A named format policy limit (element count, depth, scan bytes, ...).
    Policy(&'static str),
    Backpressure {
        requested: usize,
        available: usize,
    },
    Allocation,
    Overflow,
    Truncated,
    Transport,
    Cancelled,
    StaleGeneration {
        expected: u64,
        received: u64,
    },
    StaleRequest {
        expected: u64,
        received: u64,
    },
    ForeignPool,
    SourceMismatch,
    Unsupported(&'static str),
    /// A blocking adapter's reader failed; end of file is [`Error::Truncated`].
    Io(io::ErrorKind),
    /// The owner was closed or poisoned by an earlier failure.
    Closed,
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "invalid data: {reason}"),
            Self::Limit { requested, maximum } => {
                write!(f, "capacity request {requested} exceeds policy {maximum}")
            }
            Self::Backpressure {
                requested,
                available,
            } => {
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

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        match value.kind() {
            io::ErrorKind::UnexpectedEof => Self::Truncated,
            kind => Self::Io(kind),
        }
    }
}
