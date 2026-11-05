//! Constant tables transcribed from Rec. ITU-T H.264 (08/2024). Each table names its source.

/// Table 8-13 (zig-zag column): idx -> raster position (row * 4 + column) in a 4x4 block.
pub(crate) const ZIGZAG4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
/// Table 8-13 (field column).
pub(crate) const FIELD4: [u8; 16] = [0, 4, 1, 8, 12, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15];
/// Table 8-14 (8x8 zig-zag): idx -> raster position (row * 8 + column).
pub(crate) const ZIGZAG8: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
/// Table 8-14 (8x8 field scan).
pub(crate) const FIELD8: [u8; 64] = [
    0, 8, 16, 1, 9, 24, 32, 17, 2, 25, 40, 48, 56, 33, 10, 3, 18, 41, 49, 57, 26, 11, 4, 19, 34,
    42, 50, 58, 27, 12, 5, 20, 35, 43, 51, 59, 28, 13, 6, 21, 36, 44, 52, 60, 29, 14, 22, 37, 45,
    53, 61, 30, 7, 15, 38, 46, 54, 62, 23, 31, 39, 47, 55, 63,
];

const fn interleave(scan: &[u8; 64], i4: usize) -> [u8; 16] {
    let mut o = [0u8; 16];
    let mut k = 0;
    while k < 16 {
        o[k] = scan[4 * k + i4];
        k += 1;
    }
    o
}

/// CAVLC 8x8 residual: the 4x4 block i4 holds 8x8 scan positions 4 * k + i4 (7.3.5.3.2).
pub(crate) const ZIGZAG8_I: [[u8; 16]; 4] = [
    interleave(&ZIGZAG8, 0),
    interleave(&ZIGZAG8, 1),
    interleave(&ZIGZAG8, 2),
    interleave(&ZIGZAG8, 3),
];
pub(crate) const FIELD8_I: [[u8; 16]; 4] = [
    interleave(&FIELD8, 0),
    interleave(&FIELD8, 1),
    interleave(&FIELD8, 2),
    interleave(&FIELD8, 3),
];
/// Identity map for chroma DC.
pub(crate) const IDENT4: [u8; 4] = [0, 1, 2, 3];

/// Table 7-3.
pub(crate) const DEFAULT_4X4_INTRA: [u8; 16] = [
    6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42,
];
pub(crate) const DEFAULT_4X4_INTER: [u8; 16] = [
    10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34,
];
/// Table 7-4.
pub(crate) const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25,
    25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
pub(crate) const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

/// Equation 8-315: v for normAdjust4x4.
pub(crate) const V4: [[i32; 3]; 6] = [
    [10, 16, 13],
    [11, 18, 14],
    [13, 20, 16],
    [14, 23, 18],
    [16, 25, 20],
    [18, 29, 23],
];
/// Equation 8-318: v for normAdjust8x8.
pub(crate) const V8: [[i32; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];

/// normAdjust4x4(m, i, j), equation 8-314 (i = row, j = column).
pub(crate) fn norm_adjust4(m: usize, i: usize, j: usize) -> i32 {
    if i % 2 == 0 && j % 2 == 0 {
        V4[m][0]
    } else if i % 2 == 1 && j % 2 == 1 {
        V4[m][1]
    } else {
        V4[m][2]
    }
}

/// normAdjust8x8(m, i, j), equation 8-317.
pub(crate) fn norm_adjust8(m: usize, i: usize, j: usize) -> i32 {
    if i % 4 == 0 && j % 4 == 0 {
        V8[m][0]
    } else if i % 2 == 1 && j % 2 == 1 {
        V8[m][1]
    } else if i % 4 == 2 && j % 4 == 2 {
        V8[m][2]
    } else if (i % 4 == 0 && j % 2 == 1) || (i % 2 == 1 && j % 4 == 0) {
        V8[m][3]
    } else if (i % 4 == 0 && j % 4 == 2) || (i % 4 == 2 && j % 4 == 0) {
        V8[m][4]
    } else {
        V8[m][5]
    }
}

/// Table 8-15: QPc as a function of qPI (0..=51).
pub(crate) const QPC_TABLE: [u8; 52] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39,
    39, 39,
];

/// Table 8-16: alpha' by indexA.
pub(crate) const ALPHA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20,
    22, 25, 28, 32, 36, 40, 45, 50, 56, 63, 71, 80, 90, 101, 113, 127, 144, 162, 182, 203, 226,
    255, 255,
];
/// Table 8-16: beta' by indexB.
pub(crate) const BETA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, 8, 8,
    9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18,
];
/// Table 8-17: t'C0 by [bS - 1][indexA].
pub(crate) const TC0: [[u8; 52]; 3] = [
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 6, 6, 7, 8, 9, 10, 11, 13,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 2, 2, 2, 2, 3, 3, 3, 4, 4, 5, 5, 6, 7, 8, 8, 10, 11, 12, 13, 15, 17,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2,
        2, 3, 3, 3, 4, 4, 4, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 23, 25,
    ],
];

/// Table 9-4 (a): codeNum -> coded_block_pattern for ChromaArrayType 1 or 2, [intra, inter].
pub(crate) const CBP_420: [[u8; 2]; 48] = [
    [47, 0],
    [31, 16],
    [15, 1],
    [0, 2],
    [23, 4],
    [27, 8],
    [29, 32],
    [30, 3],
    [7, 5],
    [11, 10],
    [13, 12],
    [14, 15],
    [39, 47],
    [43, 7],
    [45, 11],
    [46, 13],
    [16, 14],
    [3, 6],
    [5, 9],
    [10, 31],
    [12, 35],
    [19, 37],
    [21, 42],
    [26, 44],
    [28, 33],
    [35, 34],
    [37, 36],
    [42, 40],
    [44, 39],
    [1, 43],
    [2, 45],
    [4, 46],
    [8, 17],
    [17, 18],
    [18, 20],
    [20, 24],
    [24, 19],
    [6, 21],
    [9, 26],
    [22, 28],
    [25, 23],
    [32, 27],
    [33, 29],
    [34, 30],
    [36, 22],
    [40, 25],
    [38, 38],
    [41, 41],
];
/// Table 9-4 (b): codeNum -> coded_block_pattern for ChromaArrayType 0 or 3, [intra, inter].
pub(crate) const CBP_400: [[u8; 2]; 16] = [
    [15, 0],
    [0, 1],
    [7, 2],
    [11, 4],
    [13, 8],
    [14, 3],
    [3, 5],
    [5, 10],
    [10, 12],
    [12, 15],
    [1, 7],
    [2, 11],
    [4, 13],
    [8, 14],
    [6, 6],
    [9, 9],
];

/// Clause 6.4.3: luma4x4BlkIdx -> raster 4x4 index (y4 * 4 + x4) inside the macroblock.
pub(crate) const BLK_TO_RASTER: [u8; 16] = [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];
/// Inverse of BLK_TO_RASTER: raster index -> luma4x4BlkIdx.
pub(crate) const RASTER_TO_BLK: [u8; 16] = [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];

/// Sub-partition prediction kinds.
pub(crate) const PRED_L0: u8 = 1;
pub(crate) const PRED_L1: u8 = 2;
pub(crate) const PRED_BI: u8 = 3;

/// Table 7-14: B mb_type 1..=21 -> (partition shape 0=16x16 1=16x8 2=8x16, pred part0, pred part1).
pub(crate) const B_MB_TYPES: [(u8, u8, u8); 22] = [
    (0, 0, 0), // 0 B_Direct_16x16 (handled separately)
    (0, PRED_L0, 0),
    (0, PRED_L1, 0),
    (0, PRED_BI, 0),
    (1, PRED_L0, PRED_L0),
    (2, PRED_L0, PRED_L0),
    (1, PRED_L1, PRED_L1),
    (2, PRED_L1, PRED_L1),
    (1, PRED_L0, PRED_L1),
    (2, PRED_L0, PRED_L1),
    (1, PRED_L1, PRED_L0),
    (2, PRED_L1, PRED_L0),
    (1, PRED_L0, PRED_BI),
    (2, PRED_L0, PRED_BI),
    (1, PRED_L1, PRED_BI),
    (2, PRED_L1, PRED_BI),
    (1, PRED_BI, PRED_L0),
    (2, PRED_BI, PRED_L0),
    (1, PRED_BI, PRED_L1),
    (2, PRED_BI, PRED_L1),
    (1, PRED_BI, PRED_BI),
    (2, PRED_BI, PRED_BI),
];

/// Sub-macroblock partition shape: 0 = 8x8, 1 = 8x4, 2 = 4x8, 3 = 4x4.
/// Table 7-18: B sub_mb_type 0..=12 -> (shape, pred). Shape for direct (0) is 4x4 / per-direct.
pub(crate) const B_SUB_TYPES: [(u8, u8); 13] = [
    (3, 0),
    (0, PRED_L0),
    (0, PRED_L1),
    (0, PRED_BI),
    (1, PRED_L0),
    (2, PRED_L0),
    (1, PRED_L1),
    (2, PRED_L1),
    (1, PRED_BI),
    (2, PRED_BI),
    (3, PRED_L0),
    (3, PRED_L1),
    (3, PRED_BI),
];

/// Number of partitions for sub shape.
pub(crate) const SUB_PARTS: [usize; 4] = [1, 2, 2, 4];

/// Table E-1: aspect_ratio_idc -> SAR.
pub(crate) const SAR_TABLE: [(u32, u32); 17] = [
    (0, 0),
    (1, 1),
    (12, 11),
    (10, 11),
    (16, 11),
    (40, 33),
    (24, 11),
    (20, 11),
    (32, 11),
    (80, 33),
    (18, 11),
    (15, 11),
    (64, 33),
    (160, 99),
    (4, 3),
    (3, 2),
    (2, 1),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_are_permutations() {
        for t in [&ZIGZAG4[..], &FIELD4[..], &ZIGZAG8[..], &FIELD8[..]] {
            let mut v: Vec<u8> = t.to_vec();
            v.sort();
            assert!(v.iter().enumerate().all(|(i, &x)| i == x as usize));
        }
        let mut seen = [false; 48];
        for e in CBP_420 {
            assert!(!seen[e[0] as usize]);
            seen[e[0] as usize] = true;
        }
        for i in 0..16 {
            assert_eq!(RASTER_TO_BLK[BLK_TO_RASTER[i] as usize] as usize, i);
        }
    }
}
