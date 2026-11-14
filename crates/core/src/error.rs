use std::{fmt, io};

/// The one error type of every format, playback and core operation. Variants
/// keep their typed detail; nothing converts it away.
#[derive(Clone, Copy, PartialEq, Eq)]
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

// The derived format, written out: a derived `fmt` is compiled into every
// crate that formats an error, this one once.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => f.debug_tuple("Invalid").field(reason).finish(),
            Self::Limit { requested, maximum } => f
                .debug_struct("Limit")
                .field("requested", requested)
                .field("maximum", maximum)
                .finish(),
            Self::Policy(limit) => f.debug_tuple("Policy").field(limit).finish(),
            Self::Backpressure {
                requested,
                available,
            } => f
                .debug_struct("Backpressure")
                .field("requested", requested)
                .field("available", available)
                .finish(),
            Self::Allocation => f.write_str("Allocation"),
            Self::Overflow => f.write_str("Overflow"),
            Self::Truncated => f.write_str("Truncated"),
            Self::Transport => f.write_str("Transport"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::StaleGeneration { expected, received } => f
                .debug_struct("StaleGeneration")
                .field("expected", expected)
                .field("received", received)
                .finish(),
            Self::StaleRequest { expected, received } => f
                .debug_struct("StaleRequest")
                .field("expected", expected)
                .field("received", received)
                .finish(),
            Self::ForeignPool => f.write_str("ForeignPool"),
            Self::SourceMismatch => f.write_str("SourceMismatch"),
            Self::Unsupported(feature) => f.debug_tuple("Unsupported").field(feature).finish(),
            Self::Io(kind) => f.debug_tuple("Io").field(kind).finish(),
            Self::Closed => f.write_str("Closed"),
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

#[cfg(test)]
mod tests {
    use super::Error;
    use std::io::ErrorKind;

    /// The variants as `#[derive(Debug)]` formats them.
    #[derive(Debug)]
    #[expect(dead_code, reason = "read only through the derived Debug")]
    enum Derived {
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
        Io(ErrorKind),
        Closed,
    }

    /// The derived twin of `error`. The match is exhaustive, so a variant added
    /// to [`Error`] fails to compile here until its twin is added.
    fn twin(error: Error) -> Derived {
        match error {
            Error::Invalid(reason) => Derived::Invalid(reason),
            Error::Limit { requested, maximum } => Derived::Limit { requested, maximum },
            Error::Policy(limit) => Derived::Policy(limit),
            Error::Backpressure {
                requested,
                available,
            } => Derived::Backpressure {
                requested,
                available,
            },
            Error::Allocation => Derived::Allocation,
            Error::Overflow => Derived::Overflow,
            Error::Truncated => Derived::Truncated,
            Error::Transport => Derived::Transport,
            Error::Cancelled => Derived::Cancelled,
            Error::StaleGeneration { expected, received } => {
                Derived::StaleGeneration { expected, received }
            }
            Error::StaleRequest { expected, received } => {
                Derived::StaleRequest { expected, received }
            }
            Error::ForeignPool => Derived::ForeignPool,
            Error::SourceMismatch => Derived::SourceMismatch,
            Error::Unsupported(feature) => Derived::Unsupported(feature),
            Error::Io(kind) => Derived::Io(kind),
            Error::Closed => Derived::Closed,
        }
    }

    #[test]
    fn debug_is_the_derived_format() {
        let errors = [
            Error::Invalid("a \"b\""),
            Error::Limit {
                requested: 1,
                maximum: 2,
            },
            Error::Policy("depth"),
            Error::Backpressure {
                requested: 3,
                available: 4,
            },
            Error::Allocation,
            Error::Overflow,
            Error::Truncated,
            Error::Transport,
            Error::Cancelled,
            Error::StaleGeneration {
                expected: 5,
                received: 6,
            },
            Error::StaleRequest {
                expected: 7,
                received: u64::MAX,
            },
            Error::ForeignPool,
            Error::SourceMismatch,
            Error::Unsupported("x"),
            Error::Io(ErrorKind::UnexpectedEof),
            Error::Closed,
        ];
        for error in errors {
            assert_eq!(format!("{error:?}"), format!("{:?}", twin(error)));
            assert_eq!(format!("{error:#?}"), format!("{:#?}", twin(error)));
        }
    }
}
