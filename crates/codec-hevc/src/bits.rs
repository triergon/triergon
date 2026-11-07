//! HEVC syntax helpers on top of the shared checked cursor (H.265 clause 7.2 and 9.2). The
//! cursor runs over an RBSP whose emulation prevention bytes were removed.

use crate::error::{DecodeError, Result};
use triergon_bitstream::BitError;
pub(crate) use triergon_bitstream::{BitCursor, BitSource};

impl From<BitError> for DecodeError {
    fn from(error: BitError) -> Self {
        DecodeError::InvalidData(match error {
            BitError::LongPrefix => "exp-golomb code too long",
            BitError::End | BitError::MissingPrevention | BitError::InvalidPrevention => {
                "bitstream overrun"
            }
        })
    }
}

/// Exp-Golomb fields with a range check.
pub(crate) trait Ranged {
    /// ue(v) with an inclusive upper bound check.
    fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32>;
    /// se(v) with an inclusive range check.
    fn se_range(&mut self, min: i32, max: i32, what: &'static str) -> Result<i32>;
}

impl Ranged for BitCursor<'_> {
    fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32> {
        let v = self.ue()?;
        if v > max {
            return Err(DecodeError::InvalidData(what));
        }
        Ok(v)
    }

    fn se_range(&mut self, min: i32, max: i32, what: &'static str) -> Result<i32> {
        let v = self.se()?;
        if v < min || v > max {
            return Err(DecodeError::InvalidData(what));
        }
        Ok(v)
    }
}
