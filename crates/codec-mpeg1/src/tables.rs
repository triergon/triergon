//! Normative ISO/IEC 11172-2 Annex B variable-length codes, written as
//! (code, length) pairs. The same tables appear in ITU-T H.262 Annex B
//! (B.1, B.3, B.4, B.12, B.13, B.14), which is the freely available text used
//! as the source. `tests` checks prefix-freedom and the Kraft sums.
//!
//! Binary literals are grouped in fours from the most significant bit, as the
//! standard prints the codes, so they can be checked against the tables.
#![allow(clippy::unusual_byte_groupings)]

/// B.1 macroblock_address_increment, index = increment (1..=33).
pub(crate) const ADDRESS_INCREMENT: [(u32, u32); 34] = [
    (0, 0),
    (0b1, 1),
    (0b011, 3),
    (0b010, 3),
    (0b0011, 4),
    (0b0010, 4),
    (0b0001_1, 5),
    (0b0001_0, 5),
    (0b0000_111, 7),
    (0b0000_110, 7),
    (0b0000_1011, 8),
    (0b0000_1010, 8),
    (0b0000_1001, 8),
    (0b0000_1000, 8),
    (0b0000_0111, 8),
    (0b0000_0110, 8),
    (0b0000_0101_11, 10),
    (0b0000_0101_10, 10),
    (0b0000_0101_01, 10),
    (0b0000_0101_00, 10),
    (0b0000_0100_11, 10),
    (0b0000_0100_10, 10),
    (0b0000_0100_011, 11),
    (0b0000_0100_010, 11),
    (0b0000_0100_001, 11),
    (0b0000_0100_000, 11),
    (0b0000_0011_111, 11),
    (0b0000_0011_110, 11),
    (0b0000_0011_101, 11),
    (0b0000_0011_100, 11),
    (0b0000_0011_011, 11),
    (0b0000_0011_010, 11),
    (0b0000_0011_001, 11),
    (0b0000_0011_000, 11),
];

/// macroblock_escape: adds 33 to the increment.
pub(crate) const ADDRESS_ESCAPE: (u32, u32) = (0b0000_0001_000, 11);

/// B.2a/B.2b macroblock_type codes used by this encoder.
pub(crate) const MB_I_INTRA: (u32, u32) = (0b1, 1);
pub(crate) const MB_I_INTRA_QUANT: (u32, u32) = (0b01, 2);
pub(crate) const MB_P_MC_CODED: (u32, u32) = (0b1, 1);
pub(crate) const MB_P_NOMC_CODED: (u32, u32) = (0b01, 2);
pub(crate) const MB_P_MC_NOT_CODED: (u32, u32) = (0b001, 3);
pub(crate) const MB_P_INTRA: (u32, u32) = (0b0001_1, 5);
pub(crate) const MB_P_MC_CODED_QUANT: (u32, u32) = (0b0001_0, 5);
pub(crate) const MB_P_NOMC_CODED_QUANT: (u32, u32) = (0b0000_1, 5);
pub(crate) const MB_P_INTRA_QUANT: (u32, u32) = (0b0000_01, 6);

/// B.3 coded_block_pattern, index = cbp (1..=63; 0 is MPEG-2 only).
pub(crate) const CODED_BLOCK_PATTERN: [(u32, u32); 64] = [
    (0b0000_0000_1, 9),
    (0b0101_1, 5),
    (0b0100_1, 5),
    (0b0011_01, 6),
    (0b1101, 4),
    (0b0010_111, 7),
    (0b0010_011, 7),
    (0b0001_1111, 8),
    (0b1100, 4),
    (0b0010_110, 7),
    (0b0010_010, 7),
    (0b0001_1110, 8),
    (0b1001_1, 5),
    (0b0001_1011, 8),
    (0b0001_0111, 8),
    (0b0001_0011, 8),
    (0b1011, 4),
    (0b0010_101, 7),
    (0b0010_001, 7),
    (0b0001_1101, 8),
    (0b1000_1, 5),
    (0b0001_1001, 8),
    (0b0001_0101, 8),
    (0b0001_0001, 8),
    (0b0011_11, 6),
    (0b0000_1111, 8),
    (0b0000_1101, 8),
    (0b0000_0001_1, 9),
    (0b0111_1, 5),
    (0b0000_1011, 8),
    (0b0000_0111, 8),
    (0b0000_0011_1, 9),
    (0b1010, 4),
    (0b0010_100, 7),
    (0b0010_000, 7),
    (0b0001_1100, 8),
    (0b0011_10, 6),
    (0b0000_1110, 8),
    (0b0000_1100, 8),
    (0b0000_0001_0, 9),
    (0b1000_0, 5),
    (0b0001_1000, 8),
    (0b0001_0100, 8),
    (0b0001_0000, 8),
    (0b0111_0, 5),
    (0b0000_1010, 8),
    (0b0000_0110, 8),
    (0b0000_0011_0, 9),
    (0b1001_0, 5),
    (0b0001_1010, 8),
    (0b0001_0110, 8),
    (0b0001_0010, 8),
    (0b0110_1, 5),
    (0b0000_1001, 8),
    (0b0000_0101, 8),
    (0b0000_0010_1, 9),
    (0b0110_0, 5),
    (0b0000_1000, 8),
    (0b0000_0100, 8),
    (0b0000_0010_0, 9),
    (0b111, 3),
    (0b0101_0, 5),
    (0b0100_0, 5),
    (0b0011_00, 6),
];

/// B.4 motion_code magnitude 0..=16, without the trailing sign bit.
pub(crate) const MOTION_CODE: [(u32, u32); 17] = [
    (0b1, 1),
    (0b01, 2),
    (0b001, 3),
    (0b0001, 4),
    (0b0000_11, 6),
    (0b0000_101, 7),
    (0b0000_100, 7),
    (0b0000_011, 7),
    (0b0000_0101_1, 9),
    (0b0000_0101_0, 9),
    (0b0000_0100_1, 9),
    (0b0000_0100_01, 10),
    (0b0000_0100_00, 10),
    (0b0000_0011_11, 10),
    (0b0000_0011_10, 10),
    (0b0000_0011_01, 10),
    (0b0000_0011_00, 10),
];

/// B.5a dct_dc_size_luminance, index = size (0..=8).
pub(crate) const DC_SIZE_LUMA: [(u32, u32); 9] = [
    (0b100, 3),
    (0b00, 2),
    (0b01, 2),
    (0b101, 3),
    (0b110, 3),
    (0b1110, 4),
    (0b1111_0, 5),
    (0b1111_10, 6),
    (0b1111_110, 7),
];

/// B.5b dct_dc_size_chrominance, index = size (0..=8).
pub(crate) const DC_SIZE_CHROMA: [(u32, u32); 9] = [
    (0b00, 2),
    (0b01, 2),
    (0b10, 2),
    (0b110, 3),
    (0b1110, 4),
    (0b1111_0, 5),
    (0b1111_10, 6),
    (0b1111_110, 7),
    (0b1111_1110, 8),
];

pub(crate) const END_OF_BLOCK: (u32, u32) = (0b10, 2);
pub(crate) const DCT_ESCAPE: (u32, u32) = (0b0000_01, 6);

/// B.5c–f dct_coeff_next as (run, level, code-without-sign, length-without-sign).
/// Run 0 level 1 is listed with its `dct_coeff_next` form `11`; the first
/// coefficient of a non-intra block uses `1` instead (dct_coeff_first).
const DCT_COEFFICIENTS: [(u8, u8, u32, u32); 111] = [
    (0, 1, 0b11, 2),
    (1, 1, 0b011, 3),
    (0, 2, 0b0100, 4),
    (2, 1, 0b0101, 4),
    (0, 3, 0b0010_1, 5),
    (3, 1, 0b0011_1, 5),
    (4, 1, 0b0011_0, 5),
    (1, 2, 0b0001_10, 6),
    (5, 1, 0b0001_11, 6),
    (6, 1, 0b0001_01, 6),
    (7, 1, 0b0001_00, 6),
    (0, 4, 0b0000_110, 7),
    (2, 2, 0b0000_100, 7),
    (8, 1, 0b0000_111, 7),
    (9, 1, 0b0000_101, 7),
    (0, 5, 0b0010_0110, 8),
    (0, 6, 0b0010_0001, 8),
    (1, 3, 0b0010_0101, 8),
    (3, 2, 0b0010_0100, 8),
    (10, 1, 0b0010_0111, 8),
    (11, 1, 0b0010_0011, 8),
    (12, 1, 0b0010_0010, 8),
    (13, 1, 0b0010_0000, 8),
    (0, 7, 0b0000_0010_10, 10),
    (1, 4, 0b0000_0011_00, 10),
    (2, 3, 0b0000_0010_11, 10),
    (4, 2, 0b0000_0011_11, 10),
    (5, 2, 0b0000_0010_01, 10),
    (14, 1, 0b0000_0011_10, 10),
    (15, 1, 0b0000_0011_01, 10),
    (16, 1, 0b0000_0010_00, 10),
    (0, 8, 0b0000_0001_1101, 12),
    (0, 9, 0b0000_0001_1000, 12),
    (0, 10, 0b0000_0001_0011, 12),
    (0, 11, 0b0000_0001_0000, 12),
    (1, 5, 0b0000_0001_1011, 12),
    (2, 4, 0b0000_0001_0100, 12),
    (3, 3, 0b0000_0001_1100, 12),
    (4, 3, 0b0000_0001_0010, 12),
    (6, 2, 0b0000_0001_1110, 12),
    (7, 2, 0b0000_0001_0101, 12),
    (8, 2, 0b0000_0001_0001, 12),
    (17, 1, 0b0000_0001_1111, 12),
    (18, 1, 0b0000_0001_1010, 12),
    (19, 1, 0b0000_0001_1001, 12),
    (20, 1, 0b0000_0001_0111, 12),
    (21, 1, 0b0000_0001_0110, 12),
    (0, 12, 0b0000_0000_1101_0, 13),
    (0, 13, 0b0000_0000_1100_1, 13),
    (0, 14, 0b0000_0000_1100_0, 13),
    (0, 15, 0b0000_0000_1011_1, 13),
    (1, 6, 0b0000_0000_1011_0, 13),
    (1, 7, 0b0000_0000_1010_1, 13),
    (2, 5, 0b0000_0000_1010_0, 13),
    (3, 4, 0b0000_0000_1001_1, 13),
    (5, 3, 0b0000_0000_1001_0, 13),
    (9, 2, 0b0000_0000_1000_1, 13),
    (10, 2, 0b0000_0000_1000_0, 13),
    (22, 1, 0b0000_0000_1111_1, 13),
    (23, 1, 0b0000_0000_1111_0, 13),
    (24, 1, 0b0000_0000_1110_1, 13),
    (25, 1, 0b0000_0000_1110_0, 13),
    (26, 1, 0b0000_0000_1101_1, 13),
    (0, 16, 0b0000_0000_0111_11, 14),
    (0, 17, 0b0000_0000_0111_10, 14),
    (0, 18, 0b0000_0000_0111_01, 14),
    (0, 19, 0b0000_0000_0111_00, 14),
    (0, 20, 0b0000_0000_0110_11, 14),
    (0, 21, 0b0000_0000_0110_10, 14),
    (0, 22, 0b0000_0000_0110_01, 14),
    (0, 23, 0b0000_0000_0110_00, 14),
    (0, 24, 0b0000_0000_0101_11, 14),
    (0, 25, 0b0000_0000_0101_10, 14),
    (0, 26, 0b0000_0000_0101_01, 14),
    (0, 27, 0b0000_0000_0101_00, 14),
    (0, 28, 0b0000_0000_0100_11, 14),
    (0, 29, 0b0000_0000_0100_10, 14),
    (0, 30, 0b0000_0000_0100_01, 14),
    (0, 31, 0b0000_0000_0100_00, 14),
    (0, 32, 0b0000_0000_0011_000, 15),
    (0, 33, 0b0000_0000_0010_111, 15),
    (0, 34, 0b0000_0000_0010_110, 15),
    (0, 35, 0b0000_0000_0010_101, 15),
    (0, 36, 0b0000_0000_0010_100, 15),
    (0, 37, 0b0000_0000_0010_011, 15),
    (0, 38, 0b0000_0000_0010_010, 15),
    (0, 39, 0b0000_0000_0010_001, 15),
    (0, 40, 0b0000_0000_0010_000, 15),
    (1, 8, 0b0000_0000_0011_111, 15),
    (1, 9, 0b0000_0000_0011_110, 15),
    (1, 10, 0b0000_0000_0011_101, 15),
    (1, 11, 0b0000_0000_0011_100, 15),
    (1, 12, 0b0000_0000_0011_011, 15),
    (1, 13, 0b0000_0000_0011_010, 15),
    (1, 14, 0b0000_0000_0011_001, 15),
    (1, 15, 0b0000_0000_0001_0011, 16),
    (1, 16, 0b0000_0000_0001_0010, 16),
    (1, 17, 0b0000_0000_0001_0001, 16),
    (1, 18, 0b0000_0000_0001_0000, 16),
    (6, 3, 0b0000_0000_0001_0100, 16),
    (11, 2, 0b0000_0000_0001_1010, 16),
    (12, 2, 0b0000_0000_0001_1001, 16),
    (13, 2, 0b0000_0000_0001_1000, 16),
    (14, 2, 0b0000_0000_0001_0111, 16),
    (15, 2, 0b0000_0000_0001_0110, 16),
    (16, 2, 0b0000_0000_0001_0101, 16),
    (27, 1, 0b0000_0000_0001_1111, 16),
    (28, 1, 0b0000_0000_0001_1110, 16),
    (29, 1, 0b0000_0000_0001_1101, 16),
    (30, 1, 0b0000_0000_0001_1100, 16),
    (31, 1, 0b0000_0000_0001_1011, 16),
];

/// Largest level per run with a dedicated code (runs 0..=31).
const MAX_LEVEL: [u8; 32] = [
    40, 18, 5, 4, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1,
];

/// Dense (run, level) → (code with sign slot, length including sign) lookup.
/// Entries are packed as `code << 5 | length`; zero means "use escape".
pub(crate) struct CoefficientTable {
    entries: [[u32; 41]; 32],
}

impl CoefficientTable {
    pub(crate) const fn new() -> CoefficientTable {
        let mut entries = [[0u32; 41]; 32];
        let mut i = 0;
        while i < DCT_COEFFICIENTS.len() {
            let (run, level, code, len) = DCT_COEFFICIENTS[i];
            entries[run as usize][level as usize] = (code << 1) << 5 | (len + 1);
            i += 1;
        }
        CoefficientTable { entries }
    }

    /// (code, length) with the sign bit clear, or `None` for escape coding.
    #[inline]
    pub(crate) fn lookup(&self, run: usize, level: u32) -> Option<(u32, u32)> {
        if run < 32 && level <= MAX_LEVEL[run] as u32 {
            let e = self.entries[run][level as usize];
            Some((e >> 5, e & 31))
        } else {
            None
        }
    }
}

impl Default for CoefficientTable {
    fn default() -> Self {
        CoefficientTable::new()
    }
}

pub(crate) static COEFFICIENTS: CoefficientTable = CoefficientTable::new();

/// Zig-zag scan order (scan index → raster index).
pub(crate) const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Raster index → zig-zag scan index.
pub(crate) const SCAN_INDEX: [usize; 64] = {
    let mut inverse = [0usize; 64];
    let mut k = 0;
    while k < 64 {
        inverse[ZIGZAG[k]] = k;
        k += 1;
    }
    inverse
};

/// Default intra quantiser matrix, raster order.
pub(crate) const INTRA_MATRIX: [u16; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, //
    16, 16, 22, 24, 27, 29, 34, 37, //
    19, 22, 26, 27, 29, 34, 34, 38, //
    22, 22, 26, 27, 29, 34, 37, 40, //
    22, 26, 27, 29, 32, 35, 40, 48, //
    26, 27, 29, 32, 35, 40, 48, 58, //
    26, 27, 29, 34, 38, 46, 56, 69, //
    27, 29, 35, 38, 46, 56, 69, 83,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix_free(codes: &[(u32, u32)]) -> bool {
        for (i, &(a, la)) in codes.iter().enumerate() {
            for &(b, lb) in &codes[i + 1..] {
                let l = la.min(lb);
                if a >> (la - l) == b >> (lb - l) {
                    return false;
                }
            }
        }
        true
    }

    fn kraft(codes: &[(u32, u32)]) -> f64 {
        codes.iter().map(|&(_, l)| 0.5f64.powi(l as i32)).sum()
    }

    #[test]
    fn address_increment_is_prefix_free() {
        let mut codes: Vec<_> = ADDRESS_INCREMENT[1..].to_vec();
        codes.push(ADDRESS_ESCAPE);
        codes.push((0b0000_0001_111, 11)); // MPEG-1 macroblock_stuffing
        assert!(prefix_free(&codes));
    }

    #[test]
    fn coded_block_pattern_is_complete() {
        assert!(prefix_free(&CODED_BLOCK_PATTERN));
        // All 64 codes (with cbp 0) leave only 0000 0000 0 unused.
        assert!((kraft(&CODED_BLOCK_PATTERN) - (1.0 - 0.5f64.powi(9))).abs() < 1e-12);
    }

    #[test]
    fn motion_codes_with_sign_are_complete() {
        let mut codes = vec![MOTION_CODE[0]];
        for &(c, l) in &MOTION_CODE[1..] {
            codes.push((c << 1, l + 1));
            codes.push((c << 1 | 1, l + 1));
        }
        assert!(prefix_free(&codes));
        assert!(kraft(&codes) <= 1.0);
    }

    #[test]
    fn dc_sizes_are_prefix_free() {
        assert!(prefix_free(&DC_SIZE_LUMA));
        assert!(prefix_free(&DC_SIZE_CHROMA));
    }

    #[test]
    fn coefficient_codes_are_prefix_free_and_dense() {
        let mut codes = vec![END_OF_BLOCK, DCT_ESCAPE];
        for &(run, level, code, len) in &DCT_COEFFICIENTS {
            assert!(level <= MAX_LEVEL[run as usize]);
            codes.push((code << 1, len + 1));
            codes.push((code << 1 | 1, len + 1));
            assert_eq!(
                COEFFICIENTS.lookup(run as usize, level as u32),
                Some((code << 1, len + 1))
            );
        }
        assert!(prefix_free(&codes));
        assert!(kraft(&codes) <= 1.0);
        for (run, &max) in MAX_LEVEL.iter().enumerate() {
            for level in 1..=max as u32 {
                assert!(
                    COEFFICIENTS.lookup(run, level).is_some(),
                    "({run},{level}) missing"
                );
            }
        }
    }

    #[test]
    fn zigzag_is_a_permutation() {
        let mut seen = [false; 64];
        for &i in &ZIGZAG {
            assert!(!seen[i]);
            seen[i] = true;
        }
    }
}
