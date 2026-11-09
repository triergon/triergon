//! Error protection of teletext bytes (EN 300 706 section 8).
//!
//! Every function takes bytes in teletext transmission order with bit b1 in the
//! least significant position, as [`TeletextUnit::packet`](super::TeletextUnit) holds them.

/// The Hamming 8/4 code words of the sixteen nibbles (EN 300 706 table 3).
const HAMMING_8_4: [u8; 16] = [
    0x15, 0x02, 0x49, 0x5E, 0x64, 0x73, 0x38, 0x2F, 0xD0, 0xC7, 0x8C, 0x9B, 0xA1, 0xB6, 0xFD, 0xEA,
];

/// Decoded nibble of every byte, or `0xFF` for a byte with two or more bit errors.
const HAMMING_8_4_DECODE: [u8; 256] = {
    let mut table = [0xFF; 256];
    let mut nibble = 0;
    while nibble < 16 {
        let word = HAMMING_8_4[nibble];
        table[word as usize] = nibble as u8;
        let mut bit = 0;
        while bit < 8 {
            table[(word ^ (1 << bit)) as usize] = nibble as u8;
            bit += 1;
        }
        nibble += 1;
    }
    table
};

/// The nibble of a Hamming 8/4 byte, correcting a single bit error.
#[must_use]
pub fn hamming_8_4(byte: u8) -> Option<u8> {
    match HAMMING_8_4_DECODE[usize::from(byte)] {
        0xFF => None,
        nibble => Some(nibble),
    }
}

/// The transmission bits covered by parity bits P1..P5 of a Hamming 24/18
/// triplet: P(k + 1) covers every position 1..=23 with bit k set.
const PARITY_GROUPS: [u32; 5] = {
    let mut groups = [0; 5];
    let mut k = 0;
    while k < 5 {
        let mut position = 1;
        while position <= 23 {
            if position & (1 << k) != 0 {
                groups[k] |= 1 << (position - 1);
            }
            position += 1;
        }
        k += 1;
    }
    groups
};

/// The 18 data bits of a Hamming 24/18 triplet, correcting a single bit error.
#[must_use]
pub fn hamming_24_18(triplet: [u8; 3]) -> Option<u32> {
    let word = u32::from(triplet[0]) | u32::from(triplet[1]) << 8 | u32::from(triplet[2]) << 16;
    // Bit n - 1 of `word` is transmission bit n. P1..P5 cover the bits whose
    // position has bit k set; P6 covers all 24; every group has odd parity.
    let mut syndrome = 0;
    for (k, mask) in PARITY_GROUPS.iter().enumerate() {
        if (word & mask).count_ones() % 2 == 0 {
            syndrome |= 1 << k;
        }
    }
    let overall_ok = word.count_ones() % 2 == 1;
    let word = match (syndrome, overall_ok) {
        (0, _) => word,
        (_, true) => return None,
        (position, false) if position <= 23 => word ^ 1 << (position - 1),
        _ => return None,
    };
    Some(
        (word >> 2 & 1)
            | (word >> 4 & 0x7) << 1
            | (word >> 8 & 0x7F) << 4
            | (word >> 16 & 0x7F) << 11,
    )
}

/// The seven-bit code of an odd-parity byte, or `None` on a parity error.
#[must_use]
pub fn odd_parity(byte: u8) -> Option<u8> {
    (byte.count_ones() % 2 == 1).then_some(byte & 0x7F)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_24_18(value: u32) -> [u8; 3] {
        let data_positions = [
            3, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 22, 23,
        ];
        let mut word = 0u32;
        for (i, position) in data_positions.iter().enumerate() {
            word |= (value >> i & 1) << (position - 1);
        }
        for k in 0..5 {
            let covered = (1..=23u32)
                .filter(|p| p & (1 << k) != 0 && *p != 1 << k)
                .map(|p| word >> (p - 1) & 1)
                .sum::<u32>();
            word |= ((covered + 1) & 1) << ((1 << k) - 1);
        }
        word |= ((word.count_ones() + 1) & 1) << 23;
        [word as u8, (word >> 8) as u8, (word >> 16) as u8]
    }

    #[test]
    fn hamming_8_4_corrects_one_error_and_rejects_two() {
        for nibble in 0..16 {
            let word = HAMMING_8_4[usize::from(nibble)];
            assert_eq!(hamming_8_4(word), Some(nibble));
            for bit in 0..8 {
                assert_eq!(hamming_8_4(word ^ 1 << bit), Some(nibble));
                for other in bit + 1..8 {
                    assert_eq!(hamming_8_4(word ^ 1 << bit ^ 1 << other), None);
                }
            }
        }
    }

    #[test]
    fn hamming_24_18_round_trips_and_corrects() {
        for value in [0, 1, 0x3FFFF, 0x2A5A5, 0x12345] {
            let triplet = encode_24_18(value);
            assert_eq!(hamming_24_18(triplet), Some(value));
            for bit in 0..24 {
                let mut damaged = triplet;
                damaged[bit / 8] ^= 1 << (bit % 8);
                assert_eq!(hamming_24_18(damaged), Some(value), "bit {bit}");
            }
            let mut double = triplet;
            double[0] ^= 0x03;
            assert_eq!(hamming_24_18(double), None);
        }
    }

    #[test]
    fn parity() {
        assert_eq!(odd_parity(0x20), Some(0x20));
        assert_eq!(odd_parity(0x80 | 0x20), None);
        assert_eq!(odd_parity(0x41 | 0x80), Some(0x41));
        assert_eq!(odd_parity(0x41), None);
        assert_eq!(odd_parity(0x43), Some(0x43));
        assert_eq!(odd_parity(0x43 | 0x80), None);
    }
}
