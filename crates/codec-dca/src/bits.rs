//! Big-endian bit reader over a normalized (16-bit big-endian word order) DTS bitstream.
//!
//! Reads past the end of the buffer return zero bits; callers check `overrun()` at
//! structural checkpoints so that a truncated frame is reported as a typed error rather
//! than a panic.
//!
//! The decoder keeps this reader instead of `triergon-bitstream`: both shared readers are
//! slower on the subframe parser.

use crate::tables::HuffBook;

pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            end: data.len() * 8,
        }
    }

    #[inline]
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    #[inline]
    pub(crate) fn seek(&mut self, bit: usize) {
        self.pos = bit;
    }

    #[inline]
    pub(crate) fn overrun(&self) -> bool {
        self.pos > self.end
    }

    /// Next 32 bits, left aligned (zero padded past the end).
    #[inline]
    pub(crate) fn peek32(&self) -> u32 {
        let byte = self.pos >> 3;
        let shift = (self.pos & 7) as u32;
        let word = if byte + 8 <= self.data.len() {
            let mut b = [0u8; 8];
            b.copy_from_slice(&self.data[byte..byte + 8]);
            u64::from_be_bytes(b)
        } else {
            let mut b = [0u8; 8];
            if byte < self.data.len() {
                let tail = &self.data[byte..];
                b[..tail.len()].copy_from_slice(tail);
            }
            u64::from_be_bytes(b)
        };
        ((word << shift) >> 32) as u32
    }

    #[inline]
    pub(crate) fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Read `n` (0..=32) bits as an unsigned value.
    #[inline]
    pub(crate) fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek32() >> (32 - n);
        self.pos += n as usize;
        v
    }

    #[inline]
    pub(crate) fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    /// Read `n` (1..=32) bits as a two's complement signed value.
    #[inline]
    pub(crate) fn read_signed(&mut self, n: u32) -> i32 {
        let v = self.read(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    /// Decode one symbol from a two-level Huffman lookup table.
    #[inline]
    pub(crate) fn huff(&mut self, book: &HuffBook) -> i32 {
        let w = self.peek32();
        let pb = book.primary_bits;
        let (sym, len) = book.lut[(w >> (32 - pb)) as usize];
        if len > 0 {
            self.pos += len as usize;
            return i32::from(sym);
        }
        let sb = (-len) as u32;
        let idx = ((w << pb) >> (32 - sb)) as usize + sym as usize;
        let (sym2, len2) = book.lut[idx];
        self.pos += len2 as usize;
        i32::from(sym2)
    }

    /// Align to the next multiple of `n` bits (relative to the buffer start).
    #[inline]
    pub(crate) fn align(&mut self, n: usize) {
        self.pos = self.pos.div_ceil(n) * n;
    }
}
