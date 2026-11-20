//! The MSB-first reader of Rice and unary codes, as FLAC codes its residuals.

use crate::reader::{load, tail};

/// MSB-first bits of a slice, read in runs of Rice codes.
///
/// Its 64-bit cache holds at least 56 unread bits after a [`refill`](Self::refill), and a
/// refill never branches on the stream: it loads the eight bytes after the cached bits and
/// advances by the whole bytes that fit (the branch-free refill of Fabian Giesen's
/// "Reading bits in far too many ways", variant 4). A Rice code is then one count of
/// leading zeros and two shifts of the cache, so a decoder refills once for several short
/// codes. Bits past the end of the data read as zero.
#[derive(Clone, Copy, Debug)]
pub struct RiceReader<'a> {
    data: &'a [u8],
    /// The byte the next refill loads from: the one after the cached bits.
    next: usize,
    /// Unread bits, left-aligned; the bits after them are stream bits or zero.
    cache: u64,
    /// Number of unread bits in `cache`.
    valid: u32,
}

impl<'a> RiceReader<'a> {
    /// A reader at bit `position` of `data`, with a full cache.
    #[inline]
    #[must_use]
    pub fn at(data: &'a [u8], position: usize) -> Self {
        let skip = (position % 8) as u32;
        RiceReader {
            data,
            next: position / 8 + 7,
            cache: load_word(data, position / 8) << skip,
            valid: 56 - skip,
        }
    }

    /// Bits consumed since the start of the data.
    #[inline(always)]
    #[must_use]
    pub fn position(&self) -> usize {
        // The cache can extend past usize::MAX bits while its position still fits.
        self.next.wrapping_mul(8).wrapping_sub(self.valid as usize)
    }

    /// Whether the consumed codes or fields extend beyond the exclusive bit limit.
    #[inline]
    #[must_use]
    pub fn past_end(&self, end: usize) -> bool {
        // Compare next * 8 - valid > end without overflowing a bit offset.
        self.next > end / 8 + (end % 8 + self.valid as usize) / 8
    }

    /// Tops the cache up to at least 56 unread bits.
    #[inline(always)]
    pub fn refill(&mut self) {
        self.cache |= load_word(self.data, self.next) >> self.valid;
        self.next += (63 - self.valid as usize) >> 3;
        self.valid |= 0x38; // 56 plus the bits short of a whole byte
    }

    /// The next `bits` bits (at most 32, and at most the unread bits).
    #[inline(always)]
    pub fn take(&mut self, bits: u32) -> u32 {
        let value = ((self.cache >> 1) >> (63 - bits)) as u32;
        self.cache <<= bits;
        self.valid -= bits;
        value
    }

    /// Reads one Rice code ending at or before the exclusive bit limit `end`.
    /// The value is the unary quotient shifted left by `k`, plus the `k` low bits.
    /// Like [`Self::take`], low bits beyond the slice read as zero.
    ///
    /// Returns `None` without advancing if `k >= 32`, the code crosses `end`,
    /// or a code spanning refills exceeds 32 value bits. A code fitting the
    /// cache retains the low 32 bits of its value.
    #[inline]
    pub fn rice(&mut self, k: u32, end: usize) -> Option<u32> {
        if k >= 32 {
            return None;
        }
        let mut next = *self;
        let value = next.rice_in_partition(k, end)?;
        if next.past_end(end) {
            return None;
        }
        *self = next;
        Some(value)
    }

    /// Reads one Rice code with `k < 32` for a group validated as a whole.
    /// `end` bounds the search through runs of zero bits. A successful code can
    /// consume bits past `end`, including a cached stop bit or low bits.
    /// Discard the group on `None` or if [`Self::past_end`] is true after the
    /// final code. Use [`Self::rice`] for a checked code.
    #[inline(always)]
    pub fn rice_in_partition(&mut self, k: u32, end: usize) -> Option<u32> {
        // A short code fits in the high register of the 64-bit cache on 32-bit
        // targets. Count and extract its bits there to avoid double-word shifts.
        #[cfg(target_pointer_width = "32")]
        {
            let head = (self.cache >> 32) as u32;
            let zeros = head.leading_zeros();
            let len = zeros + 1 + k;
            if len <= self.valid.min(32) {
                let code = head >> (32 - len);
                return Some(self.consume_rice(k, zeros, len, code));
            }
        }
        let zeros = self.cache.leading_zeros();
        let len = zeros + 1 + k;
        if len > self.valid {
            // The slow path takes the reader by value, so the hot loop's copy never has
            // its address taken and stays in registers.
            let value;
            (*self, value) = long_rice(*self, k, end);
            return value;
        }
        // The top `len` bits are the stop bit followed by the `k` low bits.
        let code = (self.cache >> (64 - len)) as u32;
        Some(self.consume_rice(k, zeros, len, code))
    }

    #[inline(always)]
    fn consume_rice(&mut self, k: u32, zeros: u32, len: u32, code: u32) -> u32 {
        self.cache <<= len;
        self.valid -= len;
        (zeros << k) | (code ^ (1 << k))
    }
}

/// A Rice code longer than the unread bits.
#[cold]
#[inline(never)]
fn long_rice(mut reader: RiceReader<'_>, k: u32, end: usize) -> (RiceReader<'_>, Option<u32>) {
    // A unary stop bit must come from the slice, even when end permits zero padding.
    let end = end.min(reader.data.len().saturating_mul(8));
    let mut quotient = 0u64;
    loop {
        reader.refill();
        let zeros = reader.cache.leading_zeros().min(32);
        quotient += u64::from(zeros);
        if zeros < 32 {
            reader.take(zeros + 1);
            break;
        }
        reader.take(32);
        if reader.past_end(end) {
            return (reader, None);
        }
    }
    reader.refill();
    let low = reader.take(k);
    let value = (quotient <= u64::from(u32::MAX >> k)).then_some(((quotient as u32) << k) | low);
    (reader, value)
}

/// The eight bytes at `byte`, big-endian, zero past the end of `data`.
#[inline(always)]
fn load_word(data: &[u8], byte: usize) -> u64 {
    u64::from_be_bytes(load(data, byte).unwrap_or_else(|| tail(data, byte)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_codes_respect_the_exact_bit_limit_and_allow_retry() {
        let data = [0xff; 16];
        let mut bits = RiceReader::at(&data, 0);
        assert_eq!(bits.rice(2, 1), None);
        assert_eq!(bits.rice(2, 2), None);
        assert_eq!(bits.position(), 0);
        assert_eq!(bits.rice(2, 3), Some(3));
        assert_eq!(bits.rice(0, 3), None);
        assert_eq!(bits.rice(32, 64), None);
        assert_eq!(bits.position(), 3);

        // Start mid-byte, cross a refill, then end inside the remainder.
        let mut data = [0; 10];
        data[8] = 0b0001_1010;
        let mut bits = RiceReader::at(&data, 5);
        for end in [36, 67, 70] {
            assert_eq!(bits.rice(3, end), None);
            assert_eq!(bits.position(), 5);
        }
        assert_eq!(bits.rice(3, 71), Some((62 << 3) | 5));
        assert_eq!(bits.position(), 71);
    }

    #[test]
    fn cached_codes_cross_the_word_boundary_at_every_bit_alignment() {
        for offset in 0..8 {
            for len in [31, 32, 33] {
                for k in [0, 7, 16, 30] {
                    let zeros = len - k - 1;
                    let stop = offset + zeros;
                    let mut data = [0xff; 8];
                    for bit in offset..stop {
                        data[bit / 8] &= !(0x80 >> (bit % 8));
                    }
                    let expected = ((zeros as u32) << k) | ((1_u32 << k) - 1);
                    let mut reader = RiceReader::at(&data, offset);
                    assert_eq!(reader.rice(k as u32, offset + len - 1), None);
                    assert_eq!(reader.position(), offset);
                    assert_eq!(reader.rice(k as u32, offset + len), Some(expected));
                    assert_eq!(reader.position(), offset + len);
                    assert_eq!(reader.rice(0, offset + len + 1), Some(0));
                }
            }
        }
    }

    #[test]
    fn zero_padding_has_no_unary_stop_bit() {
        assert_eq!(RiceReader::at(&[0; 8], 0).rice(0, usize::MAX), None);
        assert_eq!(RiceReader::at(&[], usize::MAX).rice(0, usize::MAX), None);
        assert_eq!(RiceReader::at(&[1], 0).rice(1, 8), None);
        assert_eq!(RiceReader::at(&[1], 0).rice(1, 9), Some(14));
    }

    #[test]
    fn group_end_check_handles_the_largest_bit_offset() {
        let mut bits = RiceReader::at(&[], usize::MAX - 1);
        bits.take(1);
        assert_eq!(bits.position(), usize::MAX);
        assert!(!bits.past_end(usize::MAX));
        bits.take(1);
        assert!(bits.past_end(usize::MAX));
    }

    #[test]
    fn checked_overflow_preserves_the_cached_and_long_code_rules() {
        // A cached quotient of 2 retains only its low 32 value bits for k=31.
        assert_eq!(RiceReader::at(&[0x20, 0, 0, 0, 0], 0).rice(31, 34), Some(0));
        let mut data = [0; 10];
        data[5] = 0x80;
        let mut bits = RiceReader::at(&data, 0);
        assert_eq!(bits.rice(31, 72), None);
        assert_eq!(bits.position(), 0);
        assert_eq!(bits.rice(0, 41), Some(40));
    }

    #[test]
    fn codes_and_fields_cross_refills_and_the_end() {
        // 1 | 01 0 | 0001 11 | then 60 zeros, a one, and the field 0b101.
        let mut bits = String::from("1");
        bits += "010";
        bits += "000111";
        bits += &"0".repeat(60);
        bits += "1101";
        while bits.len() % 8 != 0 {
            bits.push('0');
        }
        let data: Vec<u8> = bits
            .as_bytes()
            .chunks(8)
            .map(|c| c.iter().fold(0, |b, &d| (b << 1) | (d - b'0')))
            .collect();
        let end = data.len() * 8;
        let mut reader = RiceReader::at(&data, 0);
        assert_eq!(reader.rice(0, end), Some(0));
        assert_eq!(reader.rice(1, end), Some(2));
        reader.refill();
        assert_eq!(reader.rice(2, end), Some(15));
        assert_eq!(reader.position(), 10);
        assert_eq!(reader.rice(0, end), Some(60), "longer than the cache");
        reader.refill();
        assert_eq!(reader.take(3), 0b101);
        assert_eq!(reader.position(), 74);
        let mut past = RiceReader::at(&data, end - 2);
        assert_eq!(past.rice(0, end), None, "zeros past the end");
        let mut odd = RiceReader::at(&data, 3);
        assert_eq!(odd.take(3), 0b000, "starts mid-byte");
        assert_eq!(odd.take(4), 0b0111);
    }
}
