//! Typed decoder errors.

use core::fmt;
use triergon_codec::StorageError;

/// A bitstream feature this decoder recognises but does not implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// E-AC-3 enhanced channel coupling (A/52:2018 Annex E 3.5). FFmpeg 9.0.2 also refuses it.
    EnhancedCoupling,
    /// bsid values that A/52:2018 says a decoder shall mute on (bsid > 16).
    BitstreamId(u8),
    /// E-AC-3 reduced sample rates (fscod = 3, Annex E Table E2.3). Decoded only with
    /// `Compat::Specification`: FFmpeg 9.0.2 refuses them ("Reduced sampling rate is not
    /// implemented") and A/52:2018 gives no hearing-threshold column for them.
    Eac3ReducedSampleRate,
    /// fscod2 = 3 (reserved reduced sample rate).
    ReservedSampleRate,
    /// E-AC-3 reserved stream type 3.
    ReservedStreamType,
}

/// Why a syncframe could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The access unit does not start with the 0x0B77 syncword.
    NoSync,
    /// The access unit is shorter than its header (or the frame size it declares).
    Truncated {
        /// Bytes the header or the declared frame size needs.
        needed: usize,
        /// Bytes the access unit holds.
        available: usize,
    },
    /// The access unit is empty.
    Empty,
    /// A header or audio-block field holds a value the specification forbids.
    InvalidData(&'static str),
    /// CRC check failed and the decoder was configured to reject such frames.
    CrcMismatch {
        /// AC-3 crc1 (first 5/8 of the frame) checks; for E-AC-3 the frame CRC.
        crc1_ok: bool,
        /// AC-3 crc2 (whole frame) checks; for E-AC-3 the frame CRC.
        crc2_ok: bool,
    },
    /// Recognised but unimplemented bitstream feature.
    Unsupported(Feature),
    /// The output frame already holds samples with a different rate or layout.
    OutputLayoutMismatch,
    /// The first syncframe of an E-AC-3 access unit is not independent substream 0.
    MissingIndependentSubstream,
    /// Storage the access unit needs does not fit the decoder's budget.
    Budget(StorageError),
}

impl From<StorageError> for DecodeError {
    fn from(error: StorageError) -> Self {
        DecodeError::Budget(error)
    }
}

impl From<DecodeError> for triergon_codec::Error {
    /// The workspace error of the same class. A short or empty access unit is
    /// [`Truncated`](triergon_codec::Error::Truncated) without its byte counts, an unsupported feature keeps its
    /// name and loses the number it carries, and a refused storage charge is the
    /// budget's own error.
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::NoSync => Self::Invalid("AC-3 syncword"),
            DecodeError::Truncated { .. } | DecodeError::Empty => Self::Truncated,
            DecodeError::InvalidData(detail) => Self::Invalid(detail),
            DecodeError::CrcMismatch { .. } => Self::Invalid("syncframe CRC"),
            DecodeError::Unsupported(feature) => Self::Unsupported(match feature {
                Feature::EnhancedCoupling => "enhanced coupling",
                Feature::BitstreamId(_) => "bitstream identification",
                Feature::Eac3ReducedSampleRate => "E-AC-3 reduced sample rate",
                Feature::ReservedSampleRate => "reserved sample rate",
                Feature::ReservedStreamType => "reserved stream type",
            }),
            DecodeError::OutputLayoutMismatch => Self::Invalid("output frame layout"),
            DecodeError::MissingIndependentSubstream => {
                Self::Invalid("access unit without independent substream 0")
            }
            DecodeError::Budget(error) => error,
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::NoSync => write!(f, "no AC-3 syncword"),
            DecodeError::Truncated { needed, available } => {
                write!(
                    f,
                    "truncated syncframe: need {needed} bytes, have {available}"
                )
            }
            DecodeError::Empty => write!(f, "empty access unit"),
            DecodeError::InvalidData(what) => write!(f, "invalid bitstream: {what}"),
            DecodeError::CrcMismatch { crc1_ok, crc2_ok } => {
                write!(f, "CRC mismatch (crc1 ok: {crc1_ok}, crc2 ok: {crc2_ok})")
            }
            DecodeError::Unsupported(feat) => write!(f, "unsupported feature: {feat:?}"),
            DecodeError::OutputLayoutMismatch => {
                write!(f, "output frame holds samples with a different layout")
            }
            DecodeError::MissingIndependentSubstream => {
                write!(f, "access unit does not start with independent substream 0")
            }
            DecodeError::Budget(error) => write!(f, "storage budget: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}
