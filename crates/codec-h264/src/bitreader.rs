//! RBSP bit reader (ITU-T H.264 clause 7.2 descriptors u(n), ue(v), se(v)).
//!
//! The reader works on an RBSP buffer (emulation prevention bytes already removed) that the
//! caller pads with at least 8 zero bytes past `len_bits`, so 64-bit big-endian window loads
//! never go out of bounds. Reads past the logical end return zero bits and set no panic;
//! callers check `overrun()` at syntax checkpoints.

pub(crate) const RBSP_PAD: usize = 16;

#[derive(Clone, Copy)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    len_bits: usize,
    /// Next byte to load into the cache.
    next: usize,
    /// Left-aligned bit cache; the top `avail` bits are valid stream bits.
    cache: u64,
    avail: u32,
}

impl<'a> BitReader<'a> {
    /// `data` must contain `len_bytes` payload bytes followed by at least `RBSP_PAD` zero bytes.
    pub(crate) fn new(data: &'a [u8], len_bytes: usize) -> Self {
        debug_assert!(data.len() >= len_bytes + 8);
        let mut r = BitReader {
            data,
            len_bits: len_bytes * 8,
            next: 0,
            cache: 0,
            avail: 0,
        };
        r.refill();
        r
    }

    /// Top up the cache to at least 57 valid bits (zeros past the end of the buffer).
    #[inline(always)]
    fn refill(&mut self) {
        let nb = self.next;
        let w = if nb + 8 <= self.data.len() {
            u64::from_be_bytes([
                self.data[nb],
                self.data[nb + 1],
                self.data[nb + 2],
                self.data[nb + 3],
                self.data[nb + 4],
                self.data[nb + 5],
                self.data[nb + 6],
                self.data[nb + 7],
            ])
        } else {
            0
        };
        self.cache |= w >> self.avail;
        let add = (63 - self.avail) >> 3;
        self.next += add as usize;
        self.avail += add << 3;
    }

    #[inline(always)]
    pub(crate) fn peek(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32 && n > 0);
        if self.avail < n {
            self.refill();
        }
        (self.cache >> (64 - n)) as u32
    }

    /// Skip `n` bits that were made available by a preceding `peek` of at least `n` bits.
    #[inline(always)]
    pub(crate) fn skip(&mut self, n: u32) {
        debug_assert!(n <= self.avail);
        self.cache <<= n;
        self.avail -= n;
    }

    #[inline(always)]
    pub(crate) fn u(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.skip(n);
        v
    }

    #[inline(always)]
    pub(crate) fn flag(&mut self) -> bool {
        self.u(1) != 0
    }

    /// ue(v), clause 9.1. Values up to 2^32 - 2.
    #[inline(always)]
    pub(crate) fn ue(&mut self) -> u32 {
        if self.avail < 32 {
            self.refill();
        }
        let lz = self.cache.leading_zeros();
        if lz <= 15 {
            let len = 2 * lz + 1;
            let v = (self.cache >> (64 - len)) as u32 - 1;
            self.skip(len);
            v
        } else {
            self.ue_slow()
        }
    }

    #[cold]
    fn ue_slow(&mut self) -> u32 {
        let mut lz = 0u32;
        while self.u(1) == 0 {
            lz += 1;
            if lz > 32 || self.pos() > self.len_bits + 64 {
                return u32::MAX;
            }
        }
        if lz == 32 {
            return u32::MAX;
        }
        let hi = if lz > 16 { self.u(lz - 16) as u64 } else { 0 };
        let lo = self.u(lz.min(16)) as u64;
        let rest = (hi << lz.min(16)) | lo;
        (((1u64 << lz) - 1) + rest).min(u32::MAX as u64) as u32
    }

    /// se(v), clause 9.1.1.
    #[inline(always)]
    pub(crate) fn se(&mut self) -> i32 {
        let k = self.ue() as i64;
        if k & 1 == 1 {
            ((k + 1) >> 1) as i32
        } else {
            (-(k >> 1)) as i32
        }
    }

    /// te(v) with range > 1 is ue(v); range 1 is the inverted single bit.
    #[inline(always)]
    pub(crate) fn te(&mut self, max: u32) -> u32 {
        if max > 1 {
            self.ue()
        } else {
            (!self.flag()) as u32
        }
    }

    #[inline(always)]
    pub(crate) fn pos(&self) -> usize {
        self.next * 8 - self.avail as usize
    }

    pub(crate) fn set_pos(&mut self, pos: usize) {
        self.next = pos / 8;
        self.cache = 0;
        self.avail = 0;
        self.refill();
        let r = (pos % 8) as u32;
        if r > 0 {
            self.skip(r);
        }
    }

    pub(crate) fn len_bits(&self) -> usize {
        self.len_bits
    }

    pub(crate) fn data(&self) -> &'a [u8] {
        self.data
    }

    pub(crate) fn bits_left(&self) -> isize {
        self.len_bits as isize - self.pos() as isize
    }

    pub(crate) fn overrun(&self) -> bool {
        self.pos() > self.len_bits
    }

    pub(crate) fn align(&mut self) {
        let p = self.pos();
        let a = (p + 7) & !7;
        if a != p {
            self.set_pos(a);
        }
    }

    /// more_rbsp_data(), clause 7.2: true when there is RBSP data before the rbsp_trailing_bits.
    pub(crate) fn more_rbsp_data(&self) -> bool {
        let pos = self.pos();
        if pos >= self.len_bits {
            return false;
        }
        // Find the last 1 bit (the rbsp_stop_one_bit) in the payload.
        let mut last = self.len_bits;
        let mut i = self.len_bits / 8;
        while i > 0 {
            let b = self.data[i - 1];
            if b != 0 {
                last = (i - 1) * 8 + (7 - b.trailing_zeros() as usize);
                break;
            }
            i -= 1;
        }
        if i == 0 {
            return false;
        }
        pos < last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 | 010 | 011 | 00100 | 00101 -> 0,1,2,3,4
        let mut v = vec![0b1010_0110u8, 0b0100_0010, 0b1000_0000];
        let n = v.len();
        v.resize(n + RBSP_PAD, 0);
        let mut r = BitReader::new(&v, n);
        assert_eq!(r.ue(), 0);
        assert_eq!(r.ue(), 1);
        assert_eq!(r.ue(), 2);
        assert_eq!(r.ue(), 3);
        assert_eq!(r.se(), -2); // codeNum 4 -> -2 (Table 9-3)
        assert_eq!(r.pos(), 17);
    }

    #[test]
    fn long_ue() {
        // 30 leading zeros then 1 then 30 bits all ones => 2^31-1 + 2^30-1
        let mut bits = String::new();
        bits.push_str(&"0".repeat(30));
        bits.push('1');
        bits.push_str(&"1".repeat(30));
        while !bits.len().is_multiple_of(8) {
            bits.push('0');
        }
        let mut v: Vec<u8> = (0..bits.len() / 8)
            .map(|i| u8::from_str_radix(&bits[i * 8..i * 8 + 8], 2).unwrap())
            .collect();
        let n = v.len();
        v.resize(n + RBSP_PAD, 0);
        let mut r = BitReader::new(&v, n);
        assert_eq!(r.ue(), (1u32 << 30) - 1 + (1u32 << 30) - 1);
    }
}
