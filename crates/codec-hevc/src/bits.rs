//! RBSP bit reader (H.265 clause 7.2 functions: read_bits, ue(v), se(v), byte_aligned,
//! more_rbsp_data). Operates on an RBSP whose emulation prevention bytes were removed.

use crate::error::{DecodeError, Result};

pub struct BitReader<'a> {
    data: &'a [u8],
    /// Bit position of the next unread bit.
    pos: usize,
    len_bits: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            len_bits: data.len() * 8,
        }
    }

    #[inline]
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    /// Read n bits (n <= 32), MSB first.
    #[inline]
    pub fn u(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if self.pos + n as usize > self.len_bits {
            return Err(DecodeError::InvalidData("bitstream overrun"));
        }
        let mut v: u64 = 0;
        let byte = self.pos >> 3;
        // Gather up to 5 bytes.
        for i in 0..5 {
            v <<= 8;
            if let Some(&b) = self.data.get(byte + i) {
                v |= b as u64;
            }
        }
        let shift = 40 - (self.pos & 7) as u32 - n;
        self.pos += n as usize;
        Ok(((v >> shift) & ((1u64 << n) - 1)) as u32)
    }

    #[inline]
    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.u(1)? != 0)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.len_bits {
            return Err(DecodeError::InvalidData("bitstream overrun"));
        }
        self.pos += n;
        Ok(())
    }

    /// ue(v), clause 9.2. Values above 2^32-2 are rejected.
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(DecodeError::InvalidData("exp-golomb code too long"));
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.u(zeros)? as u64;
        let v = (1u64 << zeros) - 1 + rest;
        if v > u32::MAX as u64 - 1 {
            return Err(DecodeError::InvalidData("exp-golomb value out of range"));
        }
        Ok(v as u32)
    }

    /// se(v), clause 9.2.2.
    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()? as i64;
        let v = if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) };
        Ok(v as i32)
    }

    /// ue(v) with an inclusive upper bound check.
    pub fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32> {
        let v = self.ue()?;
        if v > max {
            return Err(DecodeError::InvalidData(what));
        }
        Ok(v)
    }

    /// se(v) with an inclusive range check.
    pub fn se_range(&mut self, min: i32, max: i32, what: &'static str) -> Result<i32> {
        let v = self.se()?;
        if v < min || v > max {
            return Err(DecodeError::InvalidData(what));
        }
        Ok(v)
    }

    /// Skip byte_alignment() / trailing bits to the next byte boundary.
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    pub fn byte_pos(&self) -> usize {
        self.pos >> 3
    }
}
