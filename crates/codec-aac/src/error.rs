//! Typed decoder errors.

use std::fmt;
use triergon_codec::StorageError;

/// A named feature the decoder deliberately does not implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// ADTS profile 2 / AOT 3 (Scalable Sample Rate) and its gain control tool.
    SsrGainControl,
    /// Audio object types outside Main / LC / LTP (+ implicit SBR / PS). Carries the AOT.
    ObjectType(u8),
    /// 960-sample frames (not expressible in ADTS).
    FrameLength960,
    /// An in-band SBR payload in a raw access unit configured for AAC-LC.
    RawSbr,
    /// Coupling channel element whose targets cannot be resolved.
    CouplingTarget,
    /// Channel layouts that need more channels than the decoder supports.
    TooManyChannels,
    /// A raw AAC program whose speaker positions cannot be represented unambiguously.
    RawProgramLayout,
    /// Reserved / escape sampling frequency index.
    SampleRateIndex(u8),
}

/// Why the decoder refused an access unit or a configuration.
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
    /// Storage the access unit needs does not fit the decoder's budget.
    Budget(StorageError),
}

impl From<StorageError> for DecodeError {
    fn from(error: StorageError) -> Self {
        DecodeError::Budget(error)
    }
}

impl From<DecodeError> for triergon_codec::Error {
    /// The workspace error of the same class. An unsupported feature keeps its name and
    /// loses the number it carries; a refused storage charge is the budget's own error.
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Unsupported(feature) => Self::Unsupported(match feature {
                Feature::SsrGainControl => "SSR gain control",
                Feature::ObjectType(_) => "audio object type",
                Feature::FrameLength960 => "960-sample frames",
                Feature::RawSbr => "SBR in a raw AAC-LC access unit",
                Feature::CouplingTarget => "coupling channel target",
                Feature::TooManyChannels => "channel count",
                Feature::RawProgramLayout => "raw program speaker layout",
                Feature::SampleRateIndex(_) => "sampling frequency index",
            }),
            DecodeError::InvalidData(detail) => Self::Invalid(detail),
            DecodeError::ResourceLimit(limit) => Self::Policy(limit),
            DecodeError::Truncated => Self::Truncated,
            DecodeError::Budget(error) => error,
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Unsupported(x) => write!(f, "unsupported feature: {x:?}"),
            DecodeError::InvalidData(s) => write!(f, "invalid data: {s}"),
            DecodeError::ResourceLimit(s) => write!(f, "resource limit: {s}"),
            DecodeError::Truncated => write!(f, "truncated access unit"),
            DecodeError::Budget(error) => write!(f, "storage budget: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub(crate) type Result<T> = std::result::Result<T, DecodeError>;
