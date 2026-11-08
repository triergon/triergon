//! A bit reader for runs of Rice codes.
//!
//! Its 64-bit cache holds at least 56 unread bits after a refill, and a refill never
//! branches on the stream: it loads the eight bytes after the cached bits and advances
//! by the whole bytes that fit (the branch-free refill of Fabian Giesen's "Reading bits
//! in far too many ways", variant 4). A Rice code is then one count of leading zeros
//! and two shifts of the cache. Bits past the end of the data read as zero.

#[derive(Clone, Copy, Debug)]
pub(crate) struct RiceReader<'a> {
    data: &'a [u8],
    /// The byte the next refill loads from: the one after the cached bits.
    next: usize,
    /// Unread bits, left-aligned; the bits after them are stream bits or zero.
    cache: u64,
    /// Number of unread bits in `cache`.
    valid: u32,
}

impl<'a> RiceReader<'a> {
    /// A reader at bit `position` of `data`.
    pub(crate) fn at(data: &'a [u8], position: usize) -> Self {
        let skip = (position % 8) as u32;
        RiceReader {
            data,
            next: position / 8 + 7,
            cache: load(data, position / 8) << skip,
            valid: 56 - skip,
        }
    }

    /// Bits consumed since the start of the data.
    pub(crate) fn position(&self) -> usize {
        self.next * 8 - self.valid as usize
    }

    /// Tops the cache up to at least 56 unread bits.
    #[inline(always)]
    pub(crate) fn refill(&mut self) {
        self.cache |= load(self.data, self.next) >> self.valid;
        self.next += (63 - self.valid as usize) >> 3;
        self.valid |= 0x38; // 56 plus the bits short of a whole byte
    }

    /// The next `bits` bits (at most 32, and at most the unread bits).
    #[inline(always)]
    pub(crate) fn take(&mut self, bits: u32) -> u32 {
        let value = ((self.cache >> 1) >> (63 - bits)) as u32;
        self.cache <<= bits;
        self.valid -= bits;
        value
    }

    /// The next Rice code with parameter `k` (at most 30), zigzag folded; `None` past
    /// `end` (a bit position) or above 32 bits.
    #[inline(always)]
    pub(crate) fn rice(&mut self, k: u32, end: usize) -> Option<u32> {
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
        let code = self.cache >> (64 - len);
        self.cache <<= len;
        self.valid -= len;
        Some((zeros << k) | (code as u32 ^ (1 << k)))
    }
}

/// A Rice code longer than the unread bits.
#[cold]
#[inline(never)]
fn long_rice(mut reader: RiceReader<'_>, k: u32, end: usize) -> (RiceReader<'_>, Option<u32>) {
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
        if reader.position() > end {
            return (reader, None);
        }
    }
    reader.refill();
    let folded = (quotient << k) | u64::from(reader.take(k));
    // RFC 9639 section 9.2.7.3: a residual fits 32-bit two's complement.
    (reader, u32::try_from(folded).ok())
}

/// The eight bytes at `byte`, big-endian, zero past the end of `data`.
#[inline(always)]
fn load(data: &[u8], byte: usize) -> u64 {
    match data.get(byte..byte + 8) {
        Some(bytes) => {
            let mut word = [0; 8];
            word.copy_from_slice(bytes);
            u64::from_be_bytes(word)
        }
        None => load_tail(data, byte),
    }
}

#[cold]
fn load_tail(data: &[u8], byte: usize) -> u64 {
    let mut word = [0; 8];
    if let Some(tail) = data.get(byte..) {
        word[..tail.len()].copy_from_slice(tail);
    }
    u64::from_be_bytes(word)
}

#[cfg(test)]
mod tests {
    use super::*;

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
