//! Codebook selection (ETSI TS 102 114 Tables 5-23..5-26, 6-6..6-8).

use crate::tables::*;

/// THUFF -> TMODE codebook (Table 5-23).
pub(crate) static TMODE_BOOKS: [&HuffBook; 4] = [&HUFF_A4, &HUFF_B4, &HUFF_C4, &HUFF_D4];

/// SHUFF 0..4 -> scale factor difference codebook (Table 5-24).
pub(crate) static SCALE_BOOKS: [&HuffBook; 5] = [
    &HUFF_SA129,
    &HUFF_SB129,
    &HUFF_SC129,
    &HUFF_SD129,
    &HUFF_SE129,
];

/// BHUFF 0..4 -> ABITS codebook (Table 5-25).
pub(crate) static ABITS_BOOKS: [&HuffBook; 5] =
    [&HUFF_A12, &HUFF_B12, &HUFF_C12, &HUFF_D12, &HUFF_E12];

/// X96 BHUFF96 0..6 for HIGHRESFLAG96K = 0 / 1 (Table 6-7).
pub(crate) static ABITS96_BOOKS_LO: [&HuffBook; 7] = [
    &HUFF_A17, &HUFF_B17, &HUFF_C17, &HUFF_D17, &HUFF_E17, &HUFF_F17, &HUFF_G17,
];
pub(crate) static ABITS96_BOOKS_HI: [&HuffBook; 7] = [
    &HUFF_A33, &HUFF_B33, &HUFF_C33, &HUFF_D33, &HUFF_E33, &HUFF_F33, &HUFF_G33,
];

/// Quantization index codebooks by ABITS (1..=10) and SEL (Table 5-26).
pub(crate) fn sample_book(abits: u32, sel: u32) -> Option<&'static HuffBook> {
    let b: &[&'static HuffBook] = match abits {
        1 => &[&HUFF_A3],
        2 => &[&HUFF_A5, &HUFF_B5, &HUFF_C5],
        3 => &[&HUFF_A7, &HUFF_B7, &HUFF_C7],
        4 => &[&HUFF_A9, &HUFF_B9, &HUFF_C9],
        5 => &[&HUFF_A13, &HUFF_B13, &HUFF_C13],
        6 => &[
            &HUFF_A17, &HUFF_B17, &HUFF_C17, &HUFF_D17, &HUFF_E17, &HUFF_F17, &HUFF_G17,
        ],
        7 => &[
            &HUFF_A25, &HUFF_B25, &HUFF_C25, &HUFF_D25, &HUFF_E25, &HUFF_F25, &HUFF_G25,
        ],
        8 => &[
            &HUFF_A33, &HUFF_B33, &HUFF_C33, &HUFF_D33, &HUFF_E33, &HUFF_F33, &HUFF_G33,
        ],
        9 => &[
            &HUFF_A65, &HUFF_B65, &HUFF_C65, &HUFF_D65, &HUFF_E65, &HUFF_F65, &HUFF_G65,
        ],
        10 => &[
            &HUFF_A129, &HUFF_B129, &HUFF_C129, &HUFF_D129, &HUFF_E129, &HUFF_F129, &HUFF_G129,
        ],
        _ => &[],
    };
    b.get(sel as usize).copied()
}

/// Number of SEL values that denote Huffman books for ABITS (the next SEL value
/// denotes block code for ABITS <= 7, or no further encoding above).
#[inline]
pub(crate) fn huffman_sel_count(abits: u32) -> u32 {
    match abits {
        1 => 1,
        2..=5 => 3,
        6..=10 => 7,
        _ => 0,
    }
}

/// Block code (Table D.6) of one ABITS: four base-`levels` quantizer indices packed into
/// a `bits`-bit code, least significant digit first.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockCode {
    levels: u32,
    pub bits: u32,
    /// `ceil(2^32 / levels)`, so that the digit split divides by multiplication.
    recip: u32,
}

impl BlockCode {
    const fn new(levels: u32, bits: u32) -> Self {
        Self {
            levels,
            bits,
            recip: (1u64 << 32).div_ceil(levels as u64) as u32,
        }
    }

    /// The four signed quantizer indices of `code`, or `None` when `code` is not below
    /// `levels^4`.
    ///
    /// Each digit takes `floor(code / levels)` as the high half of `code * recip`. With
    /// `recip = (2^32 + e) / levels` and `0 <= e < levels`, that equals the quotient
    /// whenever `code * e < 2^32` (T. Granlund and P. L. Montgomery, "Division by
    /// invariant integers using multiplication", 1994); codes have at most 19 bits and
    /// `levels` is at most 25.
    #[inline]
    pub(crate) fn split(self, mut code: u32) -> Option<[i32; 4]> {
        let off = (self.levels - 1) / 2;
        let mut q = [0i32; 4];
        for v in &mut q {
            let next = ((u64::from(code) * u64::from(self.recip)) >> 32) as u32;
            *v = (code - next * self.levels) as i32 - off as i32;
            code = next;
        }
        (code == 0).then_some(q)
    }
}

const BLOCK_CODES: [BlockCode; 7] = [
    BlockCode::new(3, 7),
    BlockCode::new(5, 10),
    BlockCode::new(7, 12),
    BlockCode::new(9, 13),
    BlockCode::new(13, 15),
    BlockCode::new(17, 17),
    BlockCode::new(25, 19),
];

/// Block code for ABITS 1..=7.
#[inline]
pub(crate) fn block_code(abits: u32) -> BlockCode {
    BLOCK_CODES[(abits as usize).clamp(1, 7) - 1]
}

#[cfg(test)]
mod tests {
    use super::BLOCK_CODES;

    #[test]
    fn block_code_split_matches_division() {
        for bc in BLOCK_CODES {
            let levels = bc.levels as i32;
            let off = (levels - 1) / 2;
            for code in 0..1u32 << bc.bits {
                let mut c = code as i32;
                let mut want = [0i32; 4];
                for v in &mut want {
                    *v = c % levels - off;
                    c /= levels;
                }
                assert_eq!(bc.split(code), (c == 0).then_some(want), "{levels} {code}");
            }
        }
    }
}
