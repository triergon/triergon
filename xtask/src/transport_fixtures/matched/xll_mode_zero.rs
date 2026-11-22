//! Construction-14 mode-zero controls authored from ETSI TS 102 114 Tables 7-7/7-15.
use super::xll::{Bits, crc};

fn set(bytes: &mut [u8], start: usize, width: usize, value: u32) {
    for (at, bit) in (start..start + width).zip((0..width).rev()) {
        let mask = 1 << (7 - at % 8);
        bytes[at / 8] = (bytes[at / 8] & !mask) | (((value >> bit) as u8 & 1) * mask);
    }
}

// This payload tests framing/recovery metadata, not XLL sample decoding.
// Reuse the short ExSS header with a 16-byte XLL frame (Table 8-1).
fn frame(tag: u8) -> Vec<u8> {
    let mut bytes = super::xll::frame(false, tag, false);
    bytes.truncate(44);
    set(&mut bytes, 51, 16, 43); // Complete ExSS size minus one.
    set(&mut bytes, 90, 16, 15); // Asset size minus one.
    set(&mut bytes, 147, 16, 15); // XLL size minus one.
    crc(&mut bytes[5..28]);
    bytes[28..].fill(0);
    let mut bits = Bits { bytes, at: 28 * 8 };
    for (n, value) in [
        (32, 0x41a2_9547),
        (4, 0),
        (8, 11),
        (5, 5),
        (6, 15),
        (4, 0),
        (4, 0),
        (4, 9),
        (5, 7),
        (2, 0),
        (1, 0),
        (5, 1),
    ] {
        bits.put(n, value);
    }
    assert_eq!(bits.at, 38 * 8);
    crc(&mut bits.bytes[32..40]);
    bits.bytes[40..].fill(tag);
    bits.bytes
}

// Author mode-zero navigation from the same Tables 7-7/7-15 fields. The
// additional mask needs a larger descriptor and DWORD-aligned ExSS header.
pub(super) fn frame_with_core(tag: u8) -> Vec<u8> {
    let original = frame(tag);
    let mut bits = Bits {
        bytes: vec![0; 48],
        at: 0,
    };
    for at in 0..145 {
        bits.put(1, u32::from((original[at / 8] >> (7 - at % 8)) & 1));
    }
    bits.put(2, 0);
    bits.put(12, 0x201);
    for at in 147..198 {
        bits.put(1, u32::from((original[at / 8] >> (7 - at % 8)) & 1));
    }
    bits.at = 218; // 14-byte descriptor beginning at bit 106.
    bits.put(1, 1); // Backward-compatible separate core.
    bits.put(5, 0); // Extension zero, asset zero.
    set(&mut bits.bytes, 43, 8, 31);
    set(&mut bits.bytes, 51, 16, 47);
    set(&mut bits.bytes, 106, 9, 13);
    bits.bytes[32..].copy_from_slice(&original[28..]);
    crc(&mut bits.bytes[5..32]);
    bits.bytes
}
