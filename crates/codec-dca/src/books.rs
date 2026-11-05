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

/// Block code (Table D.6): (levels, code bits) for ABITS 1..=7.
#[inline]
pub(crate) fn block_code(abits: u32) -> (i32, u32) {
    match abits {
        1 => (3, 7),
        2 => (5, 10),
        3 => (7, 12),
        4 => (9, 13),
        5 => (13, 15),
        6 => (17, 17),
        _ => (25, 19),
    }
}
