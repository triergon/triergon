//! Typed decoder errors.

use core::fmt;

/// A bitstream feature this decoder recognises but does not implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// E-AC-3 enhanced channel coupling (A/52:2018 Annex E 3.5). FFmpeg 9.0.2 also refuses it.
    EnhancedCoupling,
    /// bsid values that A/52:2018 says a decoder shall mute on (bsid > 16).
    BitstreamId(u8),
    /// E-AC-3 reduced sample rates (fscod = 3, Annex E Table E2.3). Decoded only with
    /// `ffmpeg_compat = false`: FFmpeg 9.0.2 refuses them ("Reduced sampling rate is not
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
    Truncated { needed: usize, available: usize },
    /// The access unit is empty.
    Empty,
    /// A header or audio-block field holds a value the specification forbids.
    InvalidData(&'static str),
    /// CRC check failed and the decoder was configured to reject such frames.
    CrcMismatch { crc1_ok: bool, crc2_ok: bool },
    /// Recognised but unimplemented bitstream feature.
    Unsupported(Feature),
    /// The output frame already holds samples with a different rate or layout.
    OutputLayoutMismatch,
    /// The first syncframe of an E-AC-3 access unit is not independent substream 0.
    MissingIndependentSubstream,
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
        }
    }
}

impl std::error::Error for DecodeError {}
