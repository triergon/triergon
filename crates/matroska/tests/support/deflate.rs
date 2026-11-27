//! zlib streams for the Matroska tests, written bit by bit from RFC 1950 and RFC 1951
//! independently of the decoder; shared by the unit and integration tests.
// Each test binary uses part of this file.
#![allow(dead_code)]

/// A DEFLATE bit writer: values go least significant bit first, as RFC 1951 §3.1.1
/// packs them.
pub(crate) struct Bits {
    pub(crate) data: Vec<u8>,
    pub(crate) used: usize,
}

impl Bits {
    pub(crate) fn new() -> Self {
        Self {
            data: Vec::new(),
            used: 0,
        }
    }

    pub(crate) fn put(&mut self, value: u32, width: usize) {
        for bit in 0..width {
            if self.used.is_multiple_of(8) {
                self.data.push(0);
            }
            if value & (1 << bit) != 0 {
                let last = self.data.len() - 1;
                self.data[last] |= 1 << (self.used % 8);
            }
            self.used += 1;
        }
    }

    /// One symbol of the fixed literal/length alphabet (RFC 1951 §3.2.6).
    pub(crate) fn fixed(&mut self, symbol: u32) {
        let (code, width) = match symbol {
            0..=143 => (0x30 + symbol, 8),
            144..=255 => (0x190 + symbol - 144, 9),
            256..=279 => (symbol - 256, 7),
            280..=287 => (0xc0 + symbol - 280, 8),
            _ => panic!("fixture symbol"),
        };
        self.put(code.reverse_bits() >> (32 - width), width);
    }
}

/// `raw` DEFLATE data in a zlib stream: header without dictionary, then the
/// Adler-32 of `expected`.
pub(crate) fn wrap(raw: Vec<u8>, expected: &[u8]) -> Vec<u8> {
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in expected {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    [
        vec![0x78, 0x01],
        raw,
        ((b << 16) | a).to_be_bytes().to_vec(),
    ]
    .concat()
}

/// One final fixed block: a literal `a` followed by `repeats` matches of length 258
/// at distance 1, and the decoded bytes.
pub(crate) fn repeated(repeats: usize) -> (Vec<u8>, Vec<u8>) {
    let mut bits = Bits::new();
    bits.put(1, 1);
    bits.put(1, 2);
    bits.fixed(u32::from(b'a'));
    for _ in 0..repeats {
        bits.fixed(285);
        bits.put(0, 5);
    }
    bits.fixed(256);
    let expected = vec![b'a'; 1 + 258 * repeats];
    (wrap(bits.data, &expected), expected)
}
