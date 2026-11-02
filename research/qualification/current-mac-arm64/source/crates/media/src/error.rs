use std::{fmt, io};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Truncated,
    Invalid(&'static str),
    Unsupported(&'static str),
    Limit(&'static str),
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
            Self::Overflow => f.write_str("arithmetic overflow"),
            Self::Cancelled => f.write_str("cancelled"),
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
