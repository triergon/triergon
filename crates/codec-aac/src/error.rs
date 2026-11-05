//! Typed decoder errors.

use std::fmt;

/// A named feature the decoder deliberately does not implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// ADTS profile 2 / AOT 3 (Scalable Sample Rate) and its gain control tool.
    SsrGainControl,
    /// Audio object types outside Main / LC / LTP (+ implicit SBR / PS). Carries the AOT.
    ObjectType(u8),
    /// 960-sample frames (not expressible in ADTS).
    FrameLength960,
    /// Coupling channel element whose targets cannot be resolved.
    CouplingTarget,
    /// Channel layouts that need more channels than the decoder supports.
    TooManyChannels,
    /// Reserved / escape sampling frequency index.
    SampleRateIndex(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The input uses a feature outside the implemented set (named).
    Unsupported(Feature),
    /// Malformed bitstream; the string names the failing syntax element.
    InvalidData(&'static str),
    /// A resource limit would be exceeded (channels, elements, ...).
    ResourceLimit(&'static str),
    /// The access unit is shorter than its header requires.
    Truncated,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Unsupported(x) => write!(f, "unsupported feature: {x:?}"),
            DecodeError::InvalidData(s) => write!(f, "invalid data: {s}"),
            DecodeError::ResourceLimit(s) => write!(f, "resource limit: {s}"),
            DecodeError::Truncated => write!(f, "truncated access unit"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub(crate) type Result<T> = std::result::Result<T, DecodeError>;
