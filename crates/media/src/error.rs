use std::{fmt, io};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Truncated,
    Invalid(&'static str),
    Unsupported(&'static str),
    Limit(&'static str),
    Backpressure { requested: usize, available: usize },
    Core(triergon_core::Error),
    Overflow,
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O: {e}"),
            Self::Truncated => f.write_str("truncated element"),
            Self::Invalid(s) => write!(f, "invalid data: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::Limit(s) => write!(f, "resource limit: {s}"),
            Self::Backpressure {
                requested,
                available,
            } => write!(
                f,
                "output storage needs {requested} bytes; {available} available"
            ),
            Self::Core(e) => write!(f, "{e}"),
            Self::Overflow => f.write_str("arithmetic overflow"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl From<triergon_core::Error> for Error {
    fn from(value: triergon_core::Error) -> Self {
        use triergon_core::Error as C;
        match value {
            C::Invalid(reason) => Self::Invalid(reason),
            C::Unsupported(reason) => Self::Unsupported(reason),
            C::Limit { .. } => Self::Limit("live storage capacity"),
            C::Backpressure {
                requested,
                available,
            } => Self::Backpressure {
                requested,
                available,
            },
            C::Allocation => Self::Limit("allocation"),
            C::Overflow => Self::Overflow,
            C::Truncated => Self::Truncated,
            C::Cancelled => Self::Cancelled,
            other => Self::Core(other),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        if value.kind() == io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(value)
        }
    }
}
