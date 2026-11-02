use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
    Limit { requested: usize, maximum: usize },
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
        }
    }
}

impl std::error::Error for Error {}
