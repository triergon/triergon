//! MSB-first bit reader over a byte slice.
//!
//! Reads past the end return zero bits and set an overrun condition that callers check
//! at syntax-element boundaries (`overrun()`), so the hot path has no per-read branch on
//! the remaining length beyond the 8-byte fast window test.

#[derive(Clone)]
pub(crate) struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
    end: usize,
    /// 64 bits of `buf` starting at byte `wbyte` (zero padded past the end): a peek is a
    /// shift of this word, so consecutive codewords do not wait for a memory load.
    word: u64,
    wbyte: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        let mut r = BitReader {
            buf,
            pos: 0,
            end: buf.len() * 8,
            word: 0,
            wbyte: 0,
        };
        r.refill();
        r
    }

    #[inline(always)]
    fn refill(&mut self) {
        let byte = self.pos >> 3;
        let mut a = [0u8; 8];
        if byte + 8 <= self.buf.len() {
            a.copy_from_slice(&self.buf[byte..byte + 8]);
        } else if byte < self.buf.len() {
            let n = self.buf.len() - byte;
            a[..n].copy_from_slice(&self.buf[byte..]);
        }
        self.word = u64::from_be_bytes(a);
        self.wbyte = byte;
    }

    /// Next 32 bits, MSB-first, zero padded past the end.
    #[inline(always)]
    pub(crate) fn peek32(&mut self) -> u32 {
        // valid while the 32 bits lie inside the cached word (also catches pos < word start)
        let off = self.pos.wrapping_sub(self.wbyte << 3);
        if off > 32 {
            self.refill();
            return ((self.word << (self.pos & 7)) >> 32) as u32;
        }
        ((self.word << off) >> 32) as u32
    }

    /// Read `n` bits (0..=32).
    #[inline(always)]
    pub(crate) fn read(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let v = if n == 32 {
            self.peek32()
        } else {
            self.peek32() >> (32 - n)
        };
        self.pos += n as usize;
        v
    }

    #[inline(always)]
    pub(crate) fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    #[inline(always)]
    pub(crate) fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Bits consumed so far.
    #[inline(always)]
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// Remaining bits (negative after an overrun).
    #[inline(always)]
    pub(crate) fn left(&self) -> isize {
        self.end as isize - self.pos as isize
    }

    #[inline(always)]
    pub(crate) fn overrun(&self) -> bool {
        self.pos > self.end
    }

    pub(crate) fn end(&self) -> usize {
        self.end
    }

    pub(crate) fn set_end(&mut self, end: usize) {
        self.end = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first_and_pads() {
        let d = [0b1010_1100u8, 0xff, 0x01];
        let mut r = BitReader::new(&d);
        assert_eq!(r.read(1), 1);
        assert_eq!(r.read(3), 0b010);
        assert_eq!(r.read(8), 0b1100_1111);
        assert_eq!(r.read(12), 0b1111_0000_0001);
        assert!(!r.overrun());
        assert_eq!(r.read(5), 0);
        assert!(r.overrun());
    }
}
