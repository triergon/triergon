//! HEVC syntax reads over an RBSP with emulation prevention bytes removed.

use crate::{DecodeError, Result};
use triergon_bitstream::{BitCursor as Cursor, BitError, BitSource};

pub(crate) struct BitCursor<'a>(Cursor<'a>);

fn invalid_bits(error: BitError) -> DecodeError {
    DecodeError::Invalid(match error {
        BitError::LongPrefix => "exp-golomb code too long",
        BitError::End | BitError::MissingPrevention | BitError::InvalidPrevention => {
            "bitstream overrun"
        }
    })
}

impl<'a> BitCursor<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self(Cursor::new(data))
    }

    pub(crate) fn read(&mut self, width: u32) -> Result<u32> {
        self.0.read(width).map_err(invalid_bits)
    }

    pub(crate) fn flag(&mut self) -> Result<bool> {
        self.0.flag().map_err(invalid_bits)
    }

    pub(crate) fn skip(&mut self, count: usize) -> Result<()> {
        self.0.skip(count).map_err(invalid_bits)
    }

    pub(crate) fn align(&mut self) {
        self.0.align();
    }

    pub(crate) fn position(&self) -> usize {
        self.0.position()
    }

    pub(crate) fn ue(&mut self) -> Result<u32> {
        self.0.ue().map_err(invalid_bits)
    }

    pub(crate) fn se(&mut self) -> Result<i32> {
        self.0.se().map_err(invalid_bits)
    }

    pub(crate) fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32> {
        let v = self.ue()?;
        if v > max {
            return Err(DecodeError::Invalid(what));
        }
        Ok(v)
    }

    pub(crate) fn se_range(&mut self, min: i32, max: i32, what: &'static str) -> Result<i32> {
        let v = self.se()?;
        if v < min || v > max {
            return Err(DecodeError::Invalid(what));
        }
        Ok(v)
    }
}
