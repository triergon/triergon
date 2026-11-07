//! The cyclic redundancy checks of the formats in this workspace.
//!
//! | Name | Polynomial | Bit order | Used by |
//! |---|---|---|---|
//! | [`crc8`] | `0x07` | MSB first | FLAC frame headers |
//! | [`crc16_a52`] | `0x8005` | MSB first | ATSC A/52 frames, FLAC frames, MPEG audio frames |
//! | [`Crc32Mpeg`] | `0x04c11db7` | MSB first | ITU-T H.222.0 Annex A sections, Ogg pages |
//! | [`Crc32Ieee`] | `0xedb88320` (reflected) | LSB first | EBML CRC-32 elements |

const fn msb_table_16(polynomial: u16) -> [u16; 256] {
    let mut table = [0; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut register = (byte as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            register = (register << 1)
                ^ if register & 0x8000 != 0 {
                    polynomial
                } else {
                    0
                };
            bit += 1;
        }
        table[byte] = register;
        byte += 1;
    }
    table
}

/// Slicing-by-4 tables: entry `k` of a table holds the contribution of a byte followed by
/// `k` zero bytes.
const fn a52_tables() -> [[u16; 256]; 4] {
    let first = msb_table_16(0x8005);
    let mut tables = [[0; 256]; 4];
    tables[0] = first;
    let mut k = 1;
    while k < 4 {
        let mut byte = 0;
        while byte < 256 {
            let previous = tables[k - 1][byte];
            tables[k][byte] = (previous << 8) ^ first[(previous >> 8) as usize];
            byte += 1;
        }
        k += 1;
    }
    tables
}

static A52: [[u16; 256]; 4] = a52_tables();

static CRC8: [u8; 256] = {
    let mut table = [0; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut register = byte as u8;
        let mut bit = 0;
        while bit < 8 {
            register = (register << 1) ^ if register & 0x80 != 0 { 0x07 } else { 0 };
            bit += 1;
        }
        table[byte] = register;
        byte += 1;
    }
    table
};

/// Shifts `data` through the CRC-8 register (x^8 + x^2 + x + 1) starting from `crc`. A
/// FLAC frame header (RFC 9639 section 9.1.8) starts from zero.
#[must_use]
pub fn crc8(mut crc: u8, data: &[u8]) -> u8 {
    for &byte in data {
        crc = CRC8[usize::from(crc ^ byte)];
    }
    crc
}

/// Shifts `data` through the A/52 register (x^16 + x^15 + x^2 + 1, section 7.10.1)
/// starting from `crc`, and returns the register. An A/52 or FLAC frame (RFC 9639 section
/// 9.3) starts from zero, an MPEG audio frame (ISO/IEC 11172-3 2.4.3.1) from `0xffff`.
#[must_use]
pub fn crc16_a52(mut crc: u16, data: &[u8]) -> u16 {
    let t = &A52;
    let mut chunks = data.chunks_exact(4);
    for c in &mut chunks {
        let hi = (crc >> 8) as u8 ^ c[0];
        let lo = crc as u8 ^ c[1];
        crc = t[3][hi as usize] ^ t[2][lo as usize] ^ t[1][c[2] as usize] ^ t[0][c[3] as usize];
    }
    for &byte in chunks.remainder() {
        crc = (crc << 8) ^ t[0][((crc >> 8) as u8 ^ byte) as usize];
    }
    crc
}

static MPEG: [u32; 256] = {
    let mut table = [0; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut register = (byte as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            register = (register << 1)
                ^ if register & 0x8000_0000 != 0 {
                    0x04c1_1db7
                } else {
                    0
                };
            bit += 1;
        }
        table[byte] = register;
        byte += 1;
    }
    table
};

/// The MPEG-2 section checksum (ITU-T H.222.0 Annex A): MSB first, register preset to all
/// ones, no final inversion. A section with its CRC leaves a zero remainder.
#[derive(Clone, Copy, Debug)]
pub struct Crc32Mpeg(u32);

impl Default for Crc32Mpeg {
    fn default() -> Self {
        Self(0xffff_ffff)
    }
}

impl Crc32Mpeg {
    /// A register preset to `value`; an Ogg page (RFC 3533 section 6) starts from zero.
    #[must_use]
    pub fn from_register(value: u32) -> Self {
        Self(value)
    }

    /// Shifts `bytes` through the register.
    #[inline]
    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.update_byte(byte);
        }
    }

    /// Shifts one byte through the register.
    #[inline]
    pub fn update_byte(&mut self, byte: u8) {
        let index = (self.0 >> 24) as u8 ^ byte;
        self.0 = (self.0 << 8) ^ MPEG[usize::from(index)];
    }

    /// The current register value.
    #[must_use]
    pub fn remainder(self) -> u32 {
        self.0
    }

    /// The remainder of a complete message.
    #[must_use]
    pub fn of(bytes: &[u8]) -> u32 {
        let mut crc = Self::default();
        for &byte in bytes {
            crc.update_byte(byte);
        }
        crc.remainder()
    }
}

static IEEE: [u32; 256] = {
    let mut table = [0; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut register = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            register = (register >> 1) ^ if register & 1 != 0 { 0xedb8_8320 } else { 0 };
            bit += 1;
        }
        table[byte] = register;
        byte += 1;
    }
    table
};

/// The EBML CRC-32: LSB first, register preset to all ones, inverted when finished.
#[derive(Clone, Copy, Debug)]
pub struct Crc32Ieee(u32);

impl Default for Crc32Ieee {
    fn default() -> Self {
        Self(u32::MAX)
    }
}

impl Crc32Ieee {
    /// Shifts `bytes` through the register.
    #[inline]
    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 >> 8) ^ IEEE[usize::from(self.0 as u8 ^ byte)];
        }
    }

    /// The checksum of everything shifted in so far.
    #[must_use]
    pub fn finish(self) -> u32 {
        !self.0
    }

    /// The checksum of a complete message.
    #[must_use]
    pub fn of(bytes: &[u8]) -> u32 {
        let mut crc = Self::default();
        crc.update(bytes);
        crc.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(length: u32) -> Vec<u8> {
        (0..length)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect()
    }

    #[test]
    fn check_values() {
        assert_eq!(crc16_a52(0, b"123456789"), 0xfee8);
        assert_eq!(Crc32Mpeg::of(b"123456789"), 0x0376_e6e7);
        assert_eq!(Crc32Ieee::of(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc8(0, b"123456789"), 0xf4);
        let mut ogg = Crc32Mpeg::from_register(0);
        ogg.update(b"123456789");
        assert_eq!(ogg.remainder(), 0x89a1_897f);
    }

    #[test]
    fn sliced_a52_matches_the_bitwise_register() {
        for length in [0, 1, 3, 4, 5, 8, 1001] {
            let data = message(length);
            let mut register = 0u16;
            for &byte in &data {
                for k in 0..8 {
                    let feedback = ((register >> 15) as u8 ^ (byte >> (7 - k))) & 1;
                    register <<= 1;
                    if feedback != 0 {
                        register ^= 0x8005;
                    }
                }
            }
            assert_eq!(crc16_a52(0, &data), register);
        }
    }

    #[test]
    fn a52_continues_from_a_register() {
        let data = message(1001);
        assert_eq!(
            crc16_a52(0x1234, &data[..3]),
            crc16_a52(crc16_a52(0x1234, &data[..1]), &data[1..3])
        );
    }

    #[test]
    fn appended_checksums_leave_a_zero_remainder() {
        let message = b"triergon crc";
        let crc = crc16_a52(0, message);
        let mut full = message.to_vec();
        full.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(crc16_a52(0, &full), 0);

        let crc = Crc32Mpeg::of(message);
        let mut full = message.to_vec();
        full.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(Crc32Mpeg::of(&full), 0);
    }

    #[test]
    fn ieee_matches_the_bitwise_register_in_pieces() {
        let data = message(1001);
        let mut register = u32::MAX;
        for &byte in &data {
            register ^= u32::from(byte);
            for _ in 0..8 {
                register = (register >> 1) ^ if register & 1 == 1 { 0xedb88320 } else { 0 };
            }
        }
        let mut crc = Crc32Ieee::default();
        crc.update(&data[..17]);
        crc.update(&data[17..]);
        assert_eq!(crc.finish(), !register);
    }
}
