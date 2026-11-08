//! Conversion of normalized LSFs into a stable short-term prediction filter (RFC 6716
//! sections 4.2.7.5.6 and 4.2.7.5.8). Every step is integer arithmetic so that all decoders
//! reproduce the same filter.
//!
//! Two steps follow FFmpeg rather than the RFC, because the decoded output is compared with
//! FFmpeg's and filters close to instability are sensitive to the last bit: the range
//! limiting of section 4.2.7.5.7 is not applied (the Q12 coefficients are kept in 16 bits,
//! wrapping, and such a filter fails the stability check and is bandwidth expanded like any
//! other), and the bandwidth expansion rounds its products where the RFC truncates.

use super::MAX_ORDER;
use super::tables::COS_Q12;

/// Q12 prediction coefficients; the first `order` entries are used.
pub(super) type LpcQ12 = [i16; MAX_ORDER];

/// The prediction filter for the LSFs `nlsf` (one entry per coefficient), with the
/// cosine evaluation order `ordering`.
pub(super) fn from_nlsf(nlsf: &[i16], ordering: &[u8]) -> LpcQ12 {
    let order = nlsf.len();
    let half = order / 2;

    // Piecewise linear cosine approximation, Q17, reordered for accuracy.
    let mut cos_q17 = [0i64; MAX_ORDER];
    for (&n, &position) in nlsf.iter().zip(ordering) {
        let n = i32::from(n.max(0));
        let (i, f) = ((n >> 8) as usize, i64::from(n & 255));
        let c0 = i64::from(COS_Q12[i]);
        let c1 = i64::from(COS_Q12[i + 1]);
        cos_q17[usize::from(position)] = (c0 * 256 + (c1 - c0) * f + 4) >> 3;
    }

    // P(z) from the even and Q(z) from the odd cosines, Q16.
    let p = polynomial(&cos_q17[..order], 0, half);
    let q = polynomial(&cos_q17[..order], 1, half);
    let mut a32_q17 = [0i64; MAX_ORDER];
    for k in 0..half {
        let p_sum = p[k + 1] + p[k];
        let q_diff = q[k + 1] - q[k];
        a32_q17[k] = -q_diff - p_sum;
        a32_q17[order - k - 1] = q_diff - p_sum;
    }
    let a32_q17 = &mut a32_q17[..order];

    let mut a_q12 = to_q12(a32_q17);
    for round in 0..16 {
        if is_stable(&a_q12[..order]) {
            break;
        }
        bandwidth_expand(a32_q17, 65536 - (2 << round));
        a_q12 = to_q12(a32_q17);
    }
    a_q12
}

/// The first `half + 1` coefficients of the product of `(1 - 2 cos(w) z^-1 + z^-2)` over
/// the cosines `cos_q17[start], cos_q17[start + 2], ...` (the remaining ones follow by
/// symmetry), Q16.
fn polynomial(cos_q17: &[i64], start: usize, half: usize) -> [i64; MAX_ORDER / 2 + 1] {
    let mut out = [0i64; MAX_ORDER / 2 + 1];
    out[0] = 1 << 16;
    out[1] = -cos_q17[start];
    for k in 1..half {
        let c = cos_q17[start + 2 * k];
        out[k + 1] = 2 * out[k - 1] - ((c * out[k] + 32768) >> 16);
        for n in (2..=k).rev() {
            out[n] += out[n - 2] - ((c * out[n - 1] + 32768) >> 16);
        }
        out[1] -= c;
    }
    out
}

/// `silk_bwexpander_32`, rounded: scale coefficient k by `chirp_q16^(k + 1)`.
fn bandwidth_expand(a32_q17: &mut [i64], chirp_q16: i64) {
    let mut chirp = chirp_q16;
    for a in a32_q17 {
        *a = (*a * chirp + 32768) >> 16;
        chirp = (chirp * chirp_q16 + 32768) >> 16;
    }
}

/// The Q12 coefficients of Q17 ones, kept in 16 bits (wrapping, as in FFmpeg).
fn to_q12(a32_q17: &[i64]) -> LpcQ12 {
    let mut a_q12 = [0; MAX_ORDER];
    for (out, &a) in a_q12.iter_mut().zip(a32_q17) {
        *out = ((a + 16) >> 5) as i16;
    }
    a_q12
}

/// Whether the filter with Q12 coefficients `a_q12` is stable with a prediction gain
/// below 10000 (`silk_LPC_inverse_pred_gain`, with the overflow check of RFC 8251
/// section 6).
pub(super) fn is_stable(a_q12: &[i16]) -> bool {
    const LIMIT_Q24: i64 = 16773022;
    const MIN_INV_GAIN_Q30: i64 = 107374;
    let dc: i32 = a_q12.iter().map(|&a| i32::from(a)).sum();
    if dc >= 4096 {
        return false;
    }
    let mut a = [0i64; MAX_ORDER];
    for (q24, &q12) in a.iter_mut().zip(a_q12) {
        *q24 = i64::from(q12) << 12;
    }
    let mut inv_gain_q30: i64 = 1 << 30;
    for k in (0..a_q12.len()).rev() {
        if a[k].abs() > LIMIT_Q24 {
            return false;
        }
        let rc_q31 = -(a[k] << 7);
        let div_q30 = (1 << 30) - ((rc_q31 * rc_q31) >> 32);
        inv_gain_q30 = ((inv_gain_q30 * div_q30) >> 32) << 2;
        if inv_gain_q30 < MIN_INV_GAIN_Q30 {
            return false;
        }
        if k == 0 {
            break;
        }
        // Step down one order: a[n] = (a[n] - a[k-n-1] rc) / (1 - rc^2), with the inverse
        // of the denominator computed to b1 fractional bits.
        let b1 = 64 - div_q30.leading_zeros() as i64;
        let b2 = b1 - 16;
        let inv_qb2 = ((1 << 29) - 1) / (div_q30 >> (b2 + 1));
        let err_q29 = (1 << 29) - (((div_q30 << (15 - b2)) * inv_qb2) >> 16);
        let gain_qb1 = (inv_qb2 << 16) + ((err_q29 * inv_qb2) >> 13);
        let previous = a;
        for n in 0..k {
            let num = (previous[n] - ((previous[k - n - 1] * rc_q31 + (1 << 30)) >> 31))
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX));
            let value = (num * gain_qb1 + (1 << (b1 - 1))) >> b1;
            if value > i64::from(i32::MAX) || value < i64::from(i32::MIN) {
                return false;
            }
            a[n] = value;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::nlsf::{self, NlsfIndices};
    use super::super::tables::{NLSF_ORDERING_NB_MB, NLSF_ORDERING_WB};
    use super::*;

    /// Impulse response energy of the synthesis filter, a direct stability measure.
    fn response_energy(a_q12: &[i16]) -> f64 {
        let a: Vec<f64> = a_q12.iter().map(|&a| f64::from(a) / 4096.0).collect();
        let mut history = vec![0.0; a.len()];
        let mut energy = 0.0;
        for i in 0..20000 {
            let x = if i == 0 { 1.0 } else { 0.0 };
            let y = x + a
                .iter()
                .zip(history.iter().rev())
                .map(|(a, h)| a * h)
                .sum::<f64>();
            history.remove(0);
            history.push(y);
            energy += y * y;
        }
        energy
    }

    #[test]
    fn evenly_spaced_lsfs_give_a_flat_filter() {
        // Uniform LSFs are the roots of 1 +/- z^-(d+1): A(z) = 1.
        for (order, ordering) in [(10, &NLSF_ORDERING_NB_MB[..]), (16, &NLSF_ORDERING_WB[..])] {
            let nlsf: Vec<i16> = (1..=order)
                .map(|k| (k * 32768 / (order + 1)) as i16)
                .collect();
            let a = from_nlsf(&nlsf, ordering);
            assert!(a[..order].iter().all(|&c| c.abs() <= 8), "{a:?}");
        }
    }

    #[test]
    fn every_codebook_vector_gives_a_stable_filter() {
        for (bandwidth, ordering) in [
            (super::super::Bandwidth::Narrow, &NLSF_ORDERING_NB_MB[..]),
            (super::super::Bandwidth::Wide, &NLSF_ORDERING_WB[..]),
        ] {
            let codebook = bandwidth.codebook();
            for stage1 in 0..32u8 {
                for residual in [-10i8, -3, 0, 4, 10] {
                    let indices = NlsfIndices::for_test(stage1, residual);
                    let lsf = indices.decode(codebook);
                    let a = from_nlsf(&lsf[..codebook.order], ordering);
                    assert!(is_stable(&a[..codebook.order]));
                    assert!(response_energy(&a[..codebook.order]) < 1.0e5);
                }
            }
        }
    }

    #[test]
    fn crowded_lsfs_are_bandwidth_expanded_to_stability() {
        // Pairs of nearly coincident LSFs put poles almost on the unit circle.
        let mut lsf = [0i16; 16];
        for (k, value) in lsf.iter_mut().enumerate() {
            *value = (1500 + (k / 2) * 3800 + (k % 2) * 3) as i16;
        }
        nlsf::stabilize(&mut lsf, &super::super::tables::NLSF_MIN_SPACING_WB);
        let a = from_nlsf(&lsf, &NLSF_ORDERING_WB);
        assert!(is_stable(&a));
        assert!(response_energy(&a) < 1.0e5);
    }

    #[test]
    fn unstable_filters_are_detected() {
        assert!(!is_stable(&[4096, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        // A double pole at z = -0.9999: 1 + 1.9998 z^-1 + 0.9998 z^-2 (DC response is fine).
        assert!(!is_stable(&[-8191, -4095, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(is_stable(&[2048, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    }
}
