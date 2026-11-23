//! Helpers the test modules share.

/// The CRC-32 of H.222.0 Annex A over `data`, by polynomial long division rather than the
/// production CRC's table update.
pub fn crc(data: &[u8]) -> u32 {
    let mut value = 0xffff_ffff_u64;
    for byte in data {
        value = (value << 8) ^ (u64::from(*byte) << 32);
        for shift in (0..8).rev() {
            if value & (1_u64 << (32 + shift)) != 0 {
                value ^= 0x1_04c1_1db7_u64 << shift;
            }
        }
    }
    value as u32
}
