//! MSB-first bit reader over a zero-padded frame buffer.

/// Bytes of zero padding kept after the frame so 8-byte loads never leave the buffer.
pub(crate) const PAD: usize = 16;

pub(crate) struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// `buf` must contain the frame followed by at least [`PAD`] zero bytes.
    #[inline]
    pub(crate) fn new(buf: &'a [u8], start_bit: usize) -> Self {
        BitReader {
            buf,
            pos: start_bit,
        }
    }

    #[inline]
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    #[inline(always)]
    fn peek64(&self) -> u64 {
        let byte = self.pos >> 3;
        match self.buf.get(byte..byte + 8) {
            Some(s) => {
                let mut w = [0u8; 8];
                w.copy_from_slice(s);
                u64::from_be_bytes(w) << (self.pos & 7)
            }
            None => 0,
        }
    }

    /// Read `n` bits, 1 <= n <= 32.
    #[inline(always)]
    pub(crate) fn get(&mut self, n: u32) -> u32 {
        debug_assert!((1..=32).contains(&n));
        let v = (self.peek64() >> (64 - n)) as u32;
        self.pos += n as usize;
        v
    }

    /// Read `n` bits as a two's complement signed value, 1 <= n <= 32.
    #[inline(always)]
    pub(crate) fn get_signed(&mut self, n: u32) -> i32 {
        debug_assert!((1..=32).contains(&n));
        let v = ((self.peek64() as i64) >> (64 - n)) as i32;
        self.pos += n as usize;
        v
    }

    #[inline(always)]
    pub(crate) fn bit(&mut self) -> bool {
        self.get(1) != 0
    }

    #[inline]
    pub(crate) fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_reads_msb_first_and_signed() {
        let mut buf = vec![0b1011_0000u8, 0xff, 0x00, 0x80];
        buf.extend_from_slice(&[0; PAD]);
        let mut r = BitReader::new(&buf, 0);
        assert_eq!(r.get(1), 1);
        assert_eq!(r.get(2), 0b01);
        assert_eq!(r.get(5), 0b10000);
        assert_eq!(r.get_signed(4), -1);
        assert_eq!(r.get(12), 0xf00);
        assert_eq!(r.get_signed(8), -128);
        // Reading past the padded buffer yields zeros instead of panicking.
        r.skip(10_000);
        assert_eq!(r.get(32), 0);
    }
}
