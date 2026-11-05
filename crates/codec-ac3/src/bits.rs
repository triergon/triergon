//! MSB-first bit reader over a zero-padded frame buffer, and the A/52 CRC.

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

/// CRC-16 with generator x^16 + x^15 + x^2 + 1 (A/52 7.10.1), MSB first, zero initial value.
const fn crc_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

/// Slicing-by-4 tables: T[k][b] = CRC contribution of byte b followed by k zero bytes.
const fn crc_tables() -> [[u16; 256]; 4] {
    let t0 = crc_table();
    let mut t = [[0u16; 256]; 4];
    t[0] = t0;
    let mut k = 1;
    while k < 4 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev << 8) ^ t0[(prev >> 8) as usize];
            i += 1;
        }
        k += 1;
    }
    t
}

static CRC_TABLES: [[u16; 256]; 4] = crc_tables();

/// Running CRC register after shifting `data` through the A/52 LFSR starting from `crc`.
pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    let t = &CRC_TABLES;
    let mut chunks = data.chunks_exact(4);
    for c in &mut chunks {
        let hi = (crc >> 8) as u8 ^ c[0];
        let lo = crc as u8 ^ c[1];
        crc = t[3][hi as usize] ^ t[2][lo as usize] ^ t[1][c[2] as usize] ^ t[0][c[3] as usize];
    }
    for &b in chunks.remainder() {
        crc = (crc << 8) ^ t[0][((crc >> 8) as u8 ^ b) as usize];
    }
    crc
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

    #[test]
    fn sliced_crc_matches_bitwise_lfsr() {
        let data: Vec<u8> = (0..1001u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let mut reg = 0u16;
        for &b in &data {
            for k in 0..8 {
                let bit = (b >> (7 - k)) & 1;
                let fb = ((reg >> 15) as u8 ^ bit) & 1;
                reg <<= 1;
                if fb != 0 {
                    reg ^= 0x8005;
                }
            }
        }
        assert_eq!(crc16(0, &data), reg);
        assert_eq!(
            crc16(0x1234, &data[..3]),
            crc16(crc16(0x1234, &data[..1]), &data[1..3])
        );
    }

    #[test]
    fn crc_of_message_with_appended_crc_is_zero() {
        let msg = b"triergon ac3 crc";
        let c = crc16(0, msg);
        let mut full = msg.to_vec();
        full.extend_from_slice(&c.to_be_bytes());
        assert_eq!(crc16(0, &full), 0);
    }
}
