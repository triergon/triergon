//! Small A/52:2018 tables transcribed by hand (each names its source table) plus tables
//! regenerated from the specification's formulas. Large tables live in `spec_tables.rs`,
//! which is generated from the specification text.

/// Table 7.6 slowdec[].
pub(crate) const SLOWDEC: [i32; 4] = [0x0f, 0x11, 0x13, 0x15];
/// Table 7.7 fastdec[].
pub(crate) const FASTDEC: [i32; 4] = [0x3f, 0x53, 0x67, 0x7b];
/// Table 7.8 slowgain[].
pub(crate) const SLOWGAIN: [i32; 4] = [0x540, 0x4d8, 0x478, 0x410];
/// Table 7.9 dbpbtab[].
pub(crate) const DBPBTAB: [i32; 4] = [0x000, 0x700, 0x900, 0xb00];
/// Table 7.10 floortab[] (0xf800 is the 16-bit two's complement value -2048).
pub(crate) const FLOORTAB: [i32; 8] = [0x2f0, 0x2b0, 0x270, 0x230, 0x1f0, 0x170, 0x0f0, -0x800];
/// Table 7.11 fastgain[].
pub(crate) const FASTGAIN: [i32; 8] = [0x080, 0x100, 0x180, 0x200, 0x280, 0x300, 0x380, 0x400];

/// Table 7.17 / 7.18: mantissa bits for the asymmetric quantizers, bap 6..=15.
pub(crate) const BAP_BITS: [u8; 16] = [0, 0, 0, 0, 0, 0, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];

/// Tables 7.25-7.28: rematrixing band edges (first bin of bands 0..3, and the end bound).
pub(crate) const REMAT_BANDS: [usize; 5] = [13, 25, 37, 61, 253];

/// Annex E Table E2.12 (coupling): default coupling band structure, index = sub-band.
pub(crate) const DEF_CPL_BNDSTRC: [bool; 18] = [
    false, false, false, false, false, false, false, false, true, false, true, true, false, true,
    true, true, true, true,
];

/// Annex E Table E2.11: default spectral extension band structure, index = sub-band.
pub(crate) const DEF_SPX_BNDSTRC: [bool; 17] = [
    false, false, false, false, false, false, false, false, true, false, true, false, true, false,
    true, false, true,
];

/// Annex E Table E3.13: first transform coefficient of SPX sub-band n (n = 0..=17).
#[inline]
pub(crate) fn spx_band_start(sbnd: usize) -> usize {
    25 + 12 * sbnd
}

/// Annex E Table E2.10: frame exponent strategy combinations (0 = reuse, 1 = D15, 2 = D25, 3 = D45).
pub(crate) const FRM_EXPSTR: [[u8; 6]; 32] = {
    const R: u8 = 0;
    const A: u8 = 1; // D15
    const B: u8 = 2; // D25
    const C: u8 = 3; // D45
    [
        [A, R, R, R, R, R],
        [A, R, R, R, R, C],
        [A, R, R, R, B, R],
        [A, R, R, R, C, C],
        [B, R, R, B, R, R],
        [B, R, R, B, R, C],
        [B, R, R, C, B, R],
        [B, R, R, C, C, C],
        [B, R, A, R, R, R],
        [B, R, B, R, R, C],
        [B, R, B, R, B, R],
        [B, R, B, R, C, C],
        [B, R, C, B, R, R],
        [B, R, C, B, R, C],
        [B, R, C, C, B, R],
        [B, R, C, C, C, C],
        [C, A, R, R, R, R],
        [C, A, R, R, R, C],
        [C, B, R, R, B, R],
        [C, B, R, R, C, C],
        [C, B, R, B, R, R],
        [C, B, R, B, R, C],
        [C, B, R, C, B, R],
        [C, B, R, C, C, C],
        [C, C, A, R, R, R],
        [C, C, B, R, R, C],
        [C, C, B, R, B, R],
        [C, C, B, R, C, C],
        [C, C, C, B, R, R],
        [C, C, C, B, R, C],
        [C, C, C, C, B, R],
        [C, C, C, C, C, C],
    ]
};

/// Annex E Table E3.2: mantissa bits (m) for GAQ hebap 8..=19 (index = hebap).
pub(crate) const HEBAP_BITS: [u8; 20] = [
    0, 2, 3, 4, 5, 7, 8, 9, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16,
];

/// Annex E Table E3.6: GAQ remapping constants (16-bit two's complement fractions).
/// Index = hebap - 8. Gk = 1: `a` only (b = 0). Gk = 2/4: hebap 8..=16.
pub(crate) const GAQ_A1: [i16; 12] = [
    0x1249, 0x0889, 0x0421, 0x0208, 0x0102, 0x0081, 0x0040, 0x0020, 0x0010, 0x0008, 0x0002, 0x0000,
];
pub(crate) const GAQ_A2: [u16; 9] = [
    0xd555, 0xc925, 0xc444, 0xc211, 0xc104, 0xc081, 0xc040, 0xc020, 0xc010,
];
pub(crate) const GAQ_B2_NEG: [u16; 9] = [
    0xeaab, 0xd249, 0xc889, 0xc421, 0xc208, 0xc102, 0xc081, 0xc040, 0xc020,
];
pub(crate) const GAQ_B2_POS: u16 = 0x4000;
pub(crate) const GAQ_A4: [u16; 9] = [
    0xedb7, 0xe666, 0xe319, 0xe186, 0xe0c2, 0xe060, 0xe030, 0xe018, 0xe00c,
];
pub(crate) const GAQ_B4_NEG: [u16; 9] = [
    0xfb6e, 0xeccd, 0xe632, 0xe30c, 0xe183, 0xe0c1, 0xe060, 0xe030, 0xe018,
];
pub(crate) const GAQ_B4_POS: u16 = 0x2000;

/// Interpret a 16-bit two's complement fraction.
#[inline]
pub(crate) fn frac16(v: u16) -> f32 {
    (v as i16) as f32 / 32768.0
}

/// Annex E Table E3.14 spxattentab[code][bin], regenerated from its closed form
/// 2^(-(code+1)(bin+1)/15) (verified against the printed table in a unit test).
pub(crate) fn spx_atten(code: usize, bin: usize) -> f32 {
    2f64.powf(-(((code + 1) * (bin + 1)) as f64) / 15.0) as f32
}

/// Modified Bessel function of the first kind, order 0 (power series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..200 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// First half (256 values) of the 512-point transform window of A/52:2018 Table 7.33.
/// The printed table is a Kaiser-Bessel-derived window with alpha = 5; it is regenerated
/// here at full precision (checked against the 5-decimal printed values in a test).
pub(crate) fn kbd_window() -> [f32; 256] {
    const M: usize = 256;
    let alpha = 5.0f64;
    let mut kernel = [0f64; M + 1];
    for (j, k) in kernel.iter_mut().enumerate() {
        let r = 2.0 * j as f64 / M as f64 - 1.0;
        *k = bessel_i0(core::f64::consts::PI * alpha * (1.0 - r * r).max(0.0).sqrt());
    }
    let total: f64 = kernel.iter().sum();
    let mut w = [0f32; M];
    let mut acc = 0.0;
    for n in 0..M {
        acc += kernel[n];
        w[n] = (acc / total).sqrt() as f32;
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kbd_window_matches_table_7_33() {
        let w = kbd_window();
        // Spot values from A/52:2018 Table 7.33 (5 decimals).
        let printed = [
            (0usize, 0.00014),
            (1, 0.00024),
            (9, 0.00187),
            (50, 0.07261),
            (100, 0.44030),
            (128, 0.71150),
            (130, 0.72877),
            (150, 0.87160),
            (200, 0.99558),
            (233, 0.99995),
            (255, 1.00000),
        ];
        for (i, v) in printed {
            assert!((w[i] as f64 - v).abs() < 6e-6, "w[{i}] = {} vs {v}", w[i]);
        }
        // Princen-Bradley condition.
        for n in 0..256 {
            let s = w[n] as f64 * w[n] as f64 + w[255 - n] as f64 * w[255 - n] as f64;
            assert!((s - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn spx_atten_matches_table_e3_14() {
        let printed = [
            (0, [0.954841604, 0.911722489, 0.870550563]),
            (4, [0.793700526, 0.629960525, 0.500000000]),
            (17, [0.435275282, 0.189464571, 0.082469244]),
            (31, [0.227930622, 0.051952369, 0.011841536]),
        ];
        for (code, row) in printed {
            for (bin, v) in row.iter().enumerate() {
                assert!((spx_atten(code, bin) as f64 - v).abs() < 1e-7);
            }
        }
    }

    #[test]
    fn generated_tables_have_spec_shape() {
        use crate::spec_tables::*;
        assert_eq!(BNDTAB[49] as usize + BNDSZ[49] as usize, 253);
        for b in 0..49 {
            assert_eq!(BNDTAB[b] + BNDSZ[b], BNDTAB[b + 1]);
        }
        for bin in 0..253 {
            let band = MASKTAB[bin] as usize;
            assert!(BNDTAB[band] as usize <= bin && bin < (BNDTAB[band] + BNDSZ[band]) as usize);
        }
        assert_eq!(LATAB[0], 0x40);
        assert_eq!(HTH_2[49], 0x4e0);
        assert_eq!(BAPTAB[63], 15);
        assert_eq!(HEBAPTAB[63], 19);
        assert_eq!(
            VQ_HEBAP1[0],
            [0x1bff, 0x1283, 0x0452, 0x10ad, 0x28ac, 0x12d4]
        );
        assert_eq!(VQ_HEBAP7[511][5], 0x01f1);
    }
}
