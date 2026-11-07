//! LSB-first bit packing, as Vorbis I packs its packets.

/// LSB-first bits of a slice: the first bit of a byte is its least significant one, and a
/// field's first bit is its least significant one (Vorbis I section 2). Reads past the end
/// yield zero bits and set [`LsbReader::overrun`], which is the "end of packet" condition
/// a decoder tests at syntax boundaries.
#[derive(Clone, Copy, Debug)]
pub struct LsbReader<'a> {
    data: &'a [u8],
    /// Bits of the stream not yet consumed, LSB first.
    cache: u64,
    /// Number of valid bits in `cache`.
    bits: u32,
    /// Next byte of `data` to enter the cache.
    next: usize,
    overrun: bool,
}

impl<'a> LsbReader<'a> {
    /// A reader at the first bit of `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        let mut reader = LsbReader {
            data,
            cache: 0,
            bits: 0,
            next: 0,
            overrun: false,
        };
        reader.refill();
        reader
    }

    #[inline(always)]
    fn refill(&mut self) {
        if let Some(bytes) = self.data.get(self.next..self.next + 8) {
            let mut word = [0; 8];
            word.copy_from_slice(bytes);
            let word = u64::from_le_bytes(word);
            // Take whole bytes while the cache has room.
            let take = (64 - self.bits) >> 3;
            if take == 0 {
                return;
            }
            let fresh = if take == 8 {
                word
            } else {
                word & ((1u64 << (take * 8)) - 1)
            };
            self.cache |= fresh << self.bits;
            self.bits += take * 8;
            self.next += take as usize;
        } else {
            while self.bits <= 56 {
                let Some(&byte) = self.data.get(self.next) else {
                    break;
                };
                self.cache |= u64::from(byte) << self.bits;
                self.bits += 8;
                self.next += 1;
            }
        }
    }

    /// The next `width` bits (at most 32) without consuming them; missing bits read as zero.
    #[inline(always)]
    pub fn peek(&mut self, width: u32) -> u32 {
        debug_assert!(width <= 32);
        if self.bits < width {
            self.refill();
        }
        if width == 0 {
            return 0;
        }
        (self.cache & (u64::MAX >> (64 - width))) as u32
    }

    /// Consumes `width` bits (at most 32) that may extend past the end.
    #[inline(always)]
    pub fn skip(&mut self, width: u32) {
        debug_assert!(width <= 32);
        if self.bits < width {
            self.refill();
            if self.bits < width {
                self.overrun = true;
                self.cache = 0;
                self.bits = 0;
                return;
            }
        }
        self.cache >>= width;
        self.bits -= width;
    }

    /// Reads an unsigned field of `width` bits, at most 32.
    #[inline(always)]
    pub fn read(&mut self, width: u32) -> u32 {
        let value = self.peek(width);
        self.skip(width);
        if self.overrun { 0 } else { value }
    }

    /// Reads one bit.
    #[inline(always)]
    pub fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    /// Reads an unsigned field of up to 64 bits.
    pub fn read_u64(&mut self, width: u32) -> u64 {
        debug_assert!(width <= 64);
        if width <= 32 {
            return u64::from(self.read(width));
        }
        let low = u64::from(self.read(32));
        let high = u64::from(self.read(width - 32));
        low | (high << 32)
    }

    /// True once a read went past the end of the slice.
    #[must_use]
    pub fn overrun(&self) -> bool {
        self.overrun
    }

    /// Bits consumed so far (the total once overrun).
    #[must_use]
    pub fn position(&self) -> usize {
        if self.overrun {
            return self.data.len() * 8;
        }
        self.next * 8 - self.bits as usize
    }

    /// Bits left before the end of the slice.
    #[must_use]
    pub fn left(&self) -> usize {
        self.data.len() * 8 - self.position()
    }
}

#[cfg(test)]
mod tests {
    use super::LsbReader;

    #[test]
    fn reads_fields_least_significant_bit_first() {
        // Vorbis I section 2.1.6: 4 bits 12, 3 bits -1 as 7, 7 bits 17, 13 bits 6969.
        let mut r = LsbReader::new(&[0b1111_1100, 0b0100_1000, 0b1100_1110, 0b0000_0110]);
        assert_eq!(r.read(4), 12);
        assert_eq!(r.read(3), 7);
        assert_eq!(r.read(7), 17);
        assert_eq!(r.read(13), 6969);
        assert_eq!(r.position(), 27);
        assert!(!r.overrun());
        assert_eq!(r.read(8), 0);
        assert!(r.overrun());
    }

    #[test]
    fn long_streams_refill_across_word_boundaries() {
        let data: Vec<u8> = (0..41u8).map(|i| i.wrapping_mul(37)).collect();
        let mut r = LsbReader::new(&data);
        let mut bits = Vec::new();
        for byte in &data {
            for k in 0..8 {
                bits.push((byte >> k) & 1);
            }
        }
        let mut at = 0;
        for width in (1..=32).cycle().take(19) {
            let want = bits[at..at + width as usize]
                .iter()
                .rev()
                .fold(0u32, |v, &b| (v << 1) | u32::from(b));
            assert_eq!(r.read(width), want, "width {width} at {at}");
            at += width as usize;
        }
        assert_eq!(r.position(), at);
        assert_eq!(r.left(), data.len() * 8 - at);
    }
}
