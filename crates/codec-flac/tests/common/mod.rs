//! Hand-built FLAC streams for the tests.
#![allow(dead_code)]

use triergon_bitstream::crc::{crc8, crc16_a52};

/// MSB-first bit writer.
#[derive(Default)]
pub struct Bits {
    bytes: Vec<u8>,
    used: u32,
}

impl Bits {
    /// The low `width` bits of `value`.
    pub fn put(&mut self, value: i64, width: u32) -> &mut Self {
        for bit in (0..width).rev() {
            if self.used == 0 {
                self.bytes.push(0);
            }
            let last = self.bytes.last_mut().unwrap();
            *last |= (((value >> bit) & 1) as u8) << (7 - self.used);
            self.used = (self.used + 1) % 8;
        }
        self
    }

    /// A Rice code with parameter `k`.
    pub fn rice(&mut self, value: i32, k: u32) -> &mut Self {
        let folded = if value < 0 {
            (!(value as u32)) * 2 + 1
        } else {
            value as u32 * 2
        };
        let folded = u64::from(folded);
        for _ in 0..folded >> k {
            self.put(0, 1);
        }
        self.put(1, 1).put((folded & ((1 << k) - 1)) as i64, k)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// A subframe body writer: the closure writes everything after the frame header.
pub type Body<'a> = &'a dyn Fn(&mut Bits);

/// A frame: fixed blocking, explicit 16-bit block size, 44.1 kHz, the given channel
/// assignment code and sample size code, frame number 0, then `body` and the CRC-16.
pub fn frame(block_size: u32, assignment: u8, size_code: u8, body: Body<'_>) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.put(0xfff8, 16)
        .put(7, 4)
        .put(9, 4)
        .put(i64::from(assignment), 4)
        .put(i64::from(size_code), 3)
        .put(0, 1)
        .put(0, 8)
        .put(i64::from(block_size - 1), 16);
    let mut bytes = bits.into_bytes();
    bytes.push(crc8(0, &bytes));
    let mut body_bits = Bits::default();
    body(&mut body_bits);
    bytes.extend(body_bits.into_bytes());
    let crc = crc16_a52(0, &bytes);
    bytes.extend(crc.to_be_bytes());
    bytes
}

/// `fLaC` and STREAMINFO (block sizes 16 to `max_block`), then optional further blocks
/// as (type, body).
pub fn configuration(
    rate: u32,
    channels: u8,
    bits: u8,
    max_block: u16,
    more: &[(u8, Vec<u8>)],
) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    let last = |is_last: bool, kind: u8| kind | if is_last { 0x80 } else { 0 };
    out.push(last(more.is_empty(), 0));
    out.extend(&[0, 0, 34]);
    out.extend(16u16.to_be_bytes());
    out.extend(max_block.to_be_bytes());
    out.extend([0; 6]);
    let packed =
        (u64::from(rate) << 44) | (u64::from(channels - 1) << 41) | (u64::from(bits - 1) << 36);
    out.extend(packed.to_be_bytes());
    out.extend([0; 16]);
    for (i, (kind, body)) in more.iter().enumerate() {
        out.push(last(i + 1 == more.len(), *kind));
        out.extend(&(body.len() as u32).to_be_bytes()[1..]);
        out.extend(body);
    }
    out
}

/// Subframe header: padding, type, no wasted bits.
pub fn subframe_header(bits: &mut Bits, kind: u32) {
    bits.put(0, 1).put(i64::from(kind), 6).put(0, 1);
}

/// A residual with one Rice partition of parameter `k`.
pub fn rice_residual(bits: &mut Bits, k: u32, residual: &[i32]) {
    bits.put(0, 2).put(0, 4).put(i64::from(k), 4);
    for &r in residual {
        bits.rice(r, k);
    }
}

/// A VERBATIM subframe of `samples` at `width` bits.
pub fn verbatim(bits: &mut Bits, width: u32, samples: &[i64]) {
    subframe_header(bits, 1);
    for &s in samples {
        bits.put(s, width);
    }
}
