//! Reads that fail at the end of the data, for headers and parameter sets.

use std::fmt;

/// Why a checked read stopped. Parsers map this to their own error type and text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitError {
    /// The source ended before the requested bits.
    End,
    /// An Exp-Golomb prefix had more leading zeros than the caller allows.
    LongPrefix,
    /// `00 00` was followed by `00`, `01` or `02` inside a NAL unit.
    MissingPrevention,
    /// An emulation prevention byte was followed by a byte above `03`.
    InvalidPrevention,
}

impl fmt::Display for BitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::End => "bit source ended",
            Self::LongPrefix => "Exp-Golomb prefix too long",
            Self::MissingPrevention => "missing emulation prevention byte",
            Self::InvalidPrevention => "invalid emulation prevention byte",
        })
    }
}

impl std::error::Error for BitError {}

/// Up to 32 bits of `data`, most significant first, starting `position` bits in. `None`
/// when they do not lie inside `data`. A `width` above 32 is a caller bug.
#[inline]
#[must_use]
pub fn bits_at(data: &[u8], position: usize, width: u32) -> Option<u32> {
    debug_assert!(width <= 32);
    let end = position.checked_add(width as usize)?;
    let bytes = data.get(position / 8..end.div_ceil(8))?;
    let word = bytes
        .iter()
        .fold(0u64, |word, &byte| (word << 8) | u64::from(byte));
    let mask = (1u64 << width) - 1;
    Some(((word >> ((8 - end % 8) % 8)) & mask) as u32)
}

/// A checked MSB-first source of bits.
pub trait BitSource {
    /// The next bit.
    fn bit(&mut self) -> Result<u32, BitError>;

    /// The next `width` bits (at most 32) as an unsigned value.
    fn read(&mut self, width: u32) -> Result<u32, BitError> {
        let mut value = 0;
        for _ in 0..width {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    /// The next bit as a flag.
    fn flag(&mut self) -> Result<bool, BitError> {
        Ok(self.bit()? != 0)
    }

    /// An Exp-Golomb code number with at most `max_zeros` (at most 32) leading zeros.
    /// Fails with [`BitError::LongPrefix`] on the first zero beyond the limit, before any
    /// later bit is read.
    fn exp_golomb(&mut self, max_zeros: u32) -> Result<u64, BitError> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > max_zeros {
                return Err(BitError::LongPrefix);
            }
        }
        Ok((1u64 << zeros) - 1 + u64::from(self.read(zeros)?))
    }

    /// ue(v): the code numbers that fit 32 bits, up to 2^32 - 2.
    fn ue(&mut self) -> Result<u32, BitError> {
        Ok(self.exp_golomb(31)? as u32)
    }

    /// se(v) over the code numbers of [`BitSource::ue`].
    fn se(&mut self) -> Result<i32, BitError> {
        let code = i64::from(self.ue()?);
        Ok(if code & 1 == 1 {
            (code + 1) / 2
        } else {
            -(code / 2)
        } as i32)
    }
}

/// Bits of a byte slice, failing at the end of the slice.
#[derive(Clone, Copy, Debug)]
pub struct BitCursor<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> BitCursor<'a> {
    /// A cursor at the first bit of `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self::at(data, 0)
    }

    /// A cursor `position` bits into `data`.
    #[must_use]
    pub fn at(data: &'a [u8], position: usize) -> Self {
        Self { data, position }
    }

    /// The whole slice, including the bits already read.
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Bits consumed since the start of the slice.
    #[must_use]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Bits left in the slice.
    #[must_use]
    pub fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.position)
    }

    /// Passes over `count` bits, which must lie inside the slice.
    pub fn skip(&mut self, count: usize) -> Result<(), BitError> {
        if count > self.remaining() {
            return Err(BitError::End);
        }
        self.position += count;
        Ok(())
    }

    /// Moves to the next byte boundary, if not on one.
    pub fn align(&mut self) {
        self.position = self.position.next_multiple_of(8);
    }
}

impl BitSource for BitCursor<'_> {
    #[inline]
    fn bit(&mut self) -> Result<u32, BitError> {
        self.read(1)
    }

    #[inline]
    fn read(&mut self, width: u32) -> Result<u32, BitError> {
        let value = bits_at(self.data, self.position, width).ok_or(BitError::End)?;
        self.position += width as usize;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_at_matches_a_scalar_read_at_each_alignment() {
        let bytes = [0x86, 0x1d, 0xc7, 0x30, 0xa5, 0x5a, 0xff];
        for position in 0..=bytes.len() * 8 {
            for width in 0..=32 {
                let expected = (position + width <= bytes.len() * 8).then(|| {
                    (position..position + width).fold(0, |value, at| {
                        (value << 1) | u32::from((bytes[at / 8] >> (7 - at % 8)) & 1)
                    })
                });
                assert_eq!(bits_at(&bytes, position, width as u32), expected);
            }
        }
        assert_eq!(bits_at(&bytes, usize::MAX, 1), None);
    }

    #[test]
    fn failed_read_consumes_nothing() {
        let mut bits = BitCursor::at(&[0xff], 4);
        assert_eq!(bits.read(5), Err(BitError::End));
        assert_eq!(bits.position(), 4);
        assert_eq!(bits.skip(5), Err(BitError::End));
        assert_eq!(bits.read(4), Ok(15));
        assert_eq!(bits.remaining(), 0);
        assert_eq!(bits.bit(), Err(BitError::End));
    }

    #[test]
    fn exp_golomb_tables() {
        // 1 | 010 | 011 | 00100 | 00101
        let mut bits = BitCursor::new(&[0b1010_0110, 0b0100_0010, 0b1000_0000]);
        assert_eq!(bits.ue(), Ok(0));
        assert_eq!(bits.ue(), Ok(1));
        assert_eq!(bits.ue(), Ok(2));
        assert_eq!(bits.ue(), Ok(3));
        assert_eq!(bits.se(), Ok(-2));
        assert_eq!(bits.position(), 17);
    }

    #[test]
    fn exp_golomb_limits() {
        // 31 zeros, then a one, then 31 ones: the largest ue(v).
        let mut bytes = [0xff; 8];
        bytes[..3].fill(0);
        bytes[3] = 1;
        let mut bits = BitCursor::new(&bytes);
        assert_eq!(bits.ue(), Ok(u32::MAX - 1));
        assert_eq!(BitCursor::new(&bytes).se(), Ok(-(i32::MAX)));

        // The prefix limit wins over the end of the data only when reached first.
        let zeros = [0; 4];
        assert_eq!(BitCursor::new(&zeros).ue(), Err(BitError::LongPrefix));
        assert_eq!(BitCursor::new(&zeros[..3]).ue(), Err(BitError::End));
        assert_eq!(
            BitCursor::new(&[0, 0, 0, 0, 0x80, 0, 0, 0, 0xff]).exp_golomb(32),
            Ok(1 << 32)
        );
    }
}
