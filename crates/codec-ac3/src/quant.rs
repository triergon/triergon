//! Mantissa dequantization tables (A/52:2018 7.3, Tables 7.19-7.23) and the Annex E
//! AHT inverse transform (3.4.5).

/// Ungrouped symmetric quantizer levels for bap 1..=5 (Tables 7.19-7.23):
/// value(code) = (2 * code - (levels - 1)) / levels.
#[cfg(test)]
pub(crate) fn sym_level(levels: i32, code: i32) -> f32 {
    if code < 0 || code >= levels {
        // Codes outside the quantizer cannot be produced by an encoder; decode as zero.
        return 0.0;
    }
    (2 * code - (levels - 1)) as f32 / levels as f32
}

/// Symmetric quantizer level as a Q23 fixed-point integer (1.0 = 2^23), truncated toward
/// zero. Mantissas are kept in this form so that the exponent shift (7.3.2/7.3.3,
/// "mantissa >> exponent") is an arithmetic shift, as the specification writes it.
#[inline]
/// Codes outside the quantizer (which a conforming encoder never emits) are evaluated
/// with the same formula, as FFmpeg 9.0.2 does (observed on corrupted data).
pub(crate) fn sym_level_q23(levels: i32, code: i32) -> i32 {
    (((2 * code - (levels - 1)) as i64 * (1 << 23)) / levels as i64) as i32
}

/// Grouped / ungrouped symmetric mantissa tables (7.3.5), Q23.
#[derive(Debug)]
pub(crate) struct GroupTables {
    pub g1: [[i32; 3]; 32],
    pub g2: [[i32; 3]; 128],
    pub g4: [[i32; 2]; 128],
    pub b3: [i32; 8],
    pub b5: [i32; 16],
}

impl GroupTables {
    pub(crate) fn new() -> Self {
        let q = sym_level_q23;
        let mut g1 = [[0i32; 3]; 32];
        for (code, e) in g1.iter_mut().enumerate() {
            let c = code as i32;
            *e = [q(3, c / 9), q(3, (c % 9) / 3), q(3, c % 3)];
        }
        let mut g2 = [[0i32; 3]; 128];
        for (code, e) in g2.iter_mut().enumerate() {
            let c = code as i32;
            *e = [q(5, c / 25), q(5, (c % 25) / 5), q(5, c % 5)];
        }
        let mut g4 = [[0i32; 2]; 128];
        for (code, e) in g4.iter_mut().enumerate() {
            let c = code as i32;
            *e = [q(11, c / 11), q(11, c % 11)];
        }
        // Ungrouped quantizers: the unused top code (7 for bap 3, 15 for bap 5) decodes
        // as zero (FFmpeg 9.0.2 behaviour on corrupted data, black-box).
        let mut b3 = [0i32; 8];
        for (c, e) in b3.iter_mut().enumerate().take(7) {
            *e = q(7, c as i32);
        }
        let mut b5 = [0i32; 16];
        for (c, e) in b5.iter_mut().enumerate().take(15) {
            *e = q(15, c as i32);
        }
        GroupTables { g1, g2, g4, b3, b5 }
    }
}

/// 2^-e for e = 0..=24 (and beyond, saturating to tiny values).
pub(crate) const fn exp_scale_table() -> [f32; 32] {
    let mut t = [0f32; 32];
    let mut e = 0;
    let mut v = 1.0f32;
    while e < 32 {
        t[e] = v;
        v *= 0.5;
        e += 1;
    }
    t
}

pub(crate) static EXP_SCALE: [f32; 32] = exp_scale_table();

/// Annex E 3.4.5 inverse DCT matrix: C(k,m) = sqrt(2) * sum_j R_j X(k,j) cos(j(2m+1)pi/12),
/// R_0 = 1/sqrt(2), R_j = 1 otherwise. Row m, column j.
pub(crate) fn aht_idct_matrix() -> [[f32; 6]; 6] {
    let mut t = [[0f32; 6]; 6];
    for (m, row) in t.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            let r = if j == 0 {
                core::f64::consts::FRAC_1_SQRT_2
            } else {
                1.0
            };
            let a = (j * (2 * m + 1)) as f64 * core::f64::consts::PI / 12.0;
            *v = (core::f64::consts::SQRT_2 * r * a.cos()) as f32;
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_levels_match_tables_7_19_to_7_23() {
        assert_eq!(sym_level(3, 0), -2.0 / 3.0);
        assert_eq!(sym_level(3, 2), 2.0 / 3.0);
        assert_eq!(sym_level(5, 1), -2.0 / 5.0);
        assert_eq!(sym_level(7, 6), 6.0 / 7.0);
        assert_eq!(sym_level(11, 5), 0.0);
        assert_eq!(sym_level(15, 0), -14.0 / 15.0);
        let g = GroupTables::new();
        let q = |v: f64| (v * 8388608.0) as i32;
        // group_code = 9a + 3b + c
        assert_eq!(g.g1[9 * 2 + 1], [q(2.0 / 3.0), q(-2.0 / 3.0), 0]);
        assert_eq!(g.g4[11 * 10 + 3], [q(10.0 / 11.0), q(-4.0 / 11.0)]);
    }

    #[test]
    fn aht_idct_is_orthonormal() {
        let t = aht_idct_matrix();
        // C = sqrt(2) R cos(...): columns are orthogonal with squared norm 6.
        for a in 0..6 {
            for b in 0..6 {
                let dot: f32 = (0..6).map(|m| t[m][a] * t[m][b]).sum();
                let want = if a == b { 6.0 } else { 0.0 };
                assert!((dot - want).abs() < 1e-5, "{a} {b} {dot}");
            }
        }
    }
}
