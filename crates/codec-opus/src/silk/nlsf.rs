//! Normalized line spectral frequencies (RFC 6716 sections 4.2.7.5.1 to 4.2.7.5.5): the
//! two-stage vector quantizer, reconstruction, stabilization and interpolation.

use super::frame::FrameType;
use super::tables::*;
use super::{Bandwidth, MAX_ORDER};
use crate::range::RangeDecoder;

/// The LSF codebook of one bandwidth group.
pub(super) struct Codebook {
    pub(super) order: usize,
    /// Stage-2 quantization step, Q16.
    step_q16: i32,
    stage1: &'static [[u8; 32]; 2],
    stage2: &'static [[u8; 9]; 8],
    /// Stage-1 vectors, `order` Q8 coefficients for each of the 32 indices.
    vectors: &'static [u8],
    /// Stage-2 codebook of each coefficient for each stage-1 index.
    select: &'static [u8],
    /// Backward prediction weight of the first `order - 1` coefficients, Q8.
    prediction: &'static [u8],
    min_spacing: &'static [i16],
    /// Order in which the cosines enter the polynomial evaluation (Table 27).
    pub(super) ordering: &'static [u8],
}

static NB_MB: Codebook = Codebook {
    order: 10,
    step_q16: 11796,
    stage1: &NLSF_STAGE1_NB_MB,
    stage2: &NLSF_STAGE2_NB_MB,
    vectors: &NLSF_VECTORS_NB_MB,
    select: &NLSF_SELECT_NB_MB,
    prediction: &NLSF_PREDICTION_NB_MB,
    min_spacing: &NLSF_MIN_SPACING_NB_MB,
    ordering: &NLSF_ORDERING_NB_MB,
};

static WB: Codebook = Codebook {
    order: 16,
    step_q16: 9830,
    stage1: &NLSF_STAGE1_WB,
    stage2: &NLSF_STAGE2_WB,
    vectors: &NLSF_VECTORS_WB,
    select: &NLSF_SELECT_WB,
    prediction: &NLSF_PREDICTION_WB,
    min_spacing: &NLSF_MIN_SPACING_WB,
    ordering: &NLSF_ORDERING_WB,
};

impl Bandwidth {
    pub(super) fn codebook(self) -> &'static Codebook {
        match self {
            Bandwidth::Narrow | Bandwidth::Medium => &NB_MB,
            Bandwidth::Wide => &WB,
        }
    }
}

/// Q15 normalized LSFs; the first `order` entries are used.
pub(super) type Nlsf = [i16; MAX_ORDER];

/// The coded LSF indices of one SILK frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NlsfIndices {
    stage1: u8,
    /// Stage-2 residual indices, -10 to 10.
    stage2: [i8; MAX_ORDER],
}

impl NlsfIndices {
    pub(super) fn parse(
        rc: &mut RangeDecoder<'_>,
        bandwidth: Bandwidth,
        frame_type: FrameType,
    ) -> NlsfIndices {
        let codebook = bandwidth.codebook();
        let order = codebook.order;
        let stage1 = rc.icdf(&codebook.stage1[frame_type.voicing()], 8);
        let select = &codebook.select[stage1 * order..][..order];
        let mut stage2 = [0; MAX_ORDER];
        for (index, &table) in stage2.iter_mut().zip(select) {
            let mut value = rc.icdf(&codebook.stage2[usize::from(table)], 8) as i8 - 4;
            if value == -4 {
                value -= rc.icdf(&NLSF_EXTENSION, 8) as i8;
            } else if value == 4 {
                value += rc.icdf(&NLSF_EXTENSION, 8) as i8;
            }
            *index = value;
        }
        NlsfIndices {
            stage1: stage1 as u8,
            stage2,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(stage1: u8, residual: i8) -> NlsfIndices {
        NlsfIndices {
            stage1,
            stage2: [residual; MAX_ORDER],
        }
    }

    /// The stabilized LSFs these indices code (sections 4.2.7.5.2 to 4.2.7.5.4).
    pub(super) fn decode(&self, codebook: &Codebook) -> Nlsf {
        let order = codebook.order;
        let stage1 = usize::from(self.stage1);
        let vector = &codebook.vectors[stage1 * order..][..order];
        let prediction = &codebook.prediction[stage1 * (order - 1)..][..order - 1];

        // Undo the backward prediction of the residual, last coefficient first.
        let mut residual_q10 = [0i32; MAX_ORDER];
        let mut next = 0;
        for k in (0..order).rev() {
            let index = i32::from(self.stage2[k]);
            let predicted = if k + 1 < order {
                (next * i32::from(prediction[k])) >> 8
            } else {
                0
            };
            let level = (index << 10) - index.signum() * 102;
            next = predicted + ((level * codebook.step_q16) >> 16);
            residual_q10[k] = next;
        }

        let mut nlsf = [0; MAX_ORDER];
        for k in 0..order {
            let cb = i32::from(vector[k]);
            let below = if k == 0 { 0 } else { i32::from(vector[k - 1]) };
            let above = if k + 1 == order {
                256
            } else {
                i32::from(vector[k + 1])
            };
            let weight_q9 = inverse_weight(cb - below, above - cb);
            let value = (cb << 7) + (residual_q10[k] << 14) / weight_q9;
            nlsf[k] = value.clamp(0, 32767) as i16;
        }
        stabilize(&mut nlsf[..order], codebook.min_spacing);
        nlsf
    }
}

/// The square root of the inverse harmonic mean weight of a codebook coefficient whose
/// neighbours are `below` and `above` away, Q9 (section 4.2.7.5.3).
fn inverse_weight(below: i32, above: i32) -> i32 {
    // The codebook vectors increase strictly, so both distances are at least 1.
    let w2_q18 = (1024 / below.max(1) + 1024 / above.max(1)) << 16;
    let i = 32 - w2_q18.leading_zeros() as i32;
    let f = (w2_q18 >> (i - 8)) & 127;
    let y = (if i & 1 != 0 { 32768 } else { 46214 }) >> ((32 - i) >> 1);
    y + ((213 * f * y) >> 16)
}

/// Enforce the minimum spacing between LSFs (section 4.2.7.5.4 with the overflow fix of
/// RFC 8251 section 7). `min_spacing` has one entry more than `nlsf`.
pub(super) fn stabilize(nlsf: &mut [i16], min_spacing: &[i16]) {
    let order = nlsf.len();
    let spacing = |k: usize| i32::from(min_spacing[k]);
    for _ in 0..20 {
        // The position whose spacing falls furthest below its minimum, lowest first.
        let mut worst = 0;
        let mut worst_margin = i32::from(nlsf[0]) - spacing(0);
        for k in 1..=order {
            let upper = if k == order {
                32768
            } else {
                i32::from(nlsf[k])
            };
            let margin = upper - i32::from(nlsf[k - 1]) - spacing(k);
            if margin < worst_margin {
                worst_margin = margin;
                worst = k;
            }
        }
        if worst_margin >= 0 {
            return;
        }
        if worst == 0 {
            nlsf[0] = min_spacing[0];
        } else if worst == order {
            nlsf[order - 1] = (32768 - spacing(order)) as i16;
        } else {
            let half = spacing(worst) >> 1;
            let min_center = min_spacing[..worst]
                .iter()
                .map(|&d| i32::from(d))
                .sum::<i32>()
                + half;
            let max_center = 32768
                - min_spacing[worst + 1..]
                    .iter()
                    .map(|&d| i32::from(d))
                    .sum::<i32>()
                - half;
            let center = ((i32::from(nlsf[worst - 1]) + i32::from(nlsf[worst]) + 1) >> 1)
                .clamp(min_center, max_center);
            nlsf[worst - 1] = (center - half) as i16;
            nlsf[worst] = (center - half + spacing(worst)) as i16;
        }
    }

    // Fallback: sort, then push the values apart from both ends.
    nlsf.sort_unstable();
    let mut floor = 0;
    for k in 0..order {
        let value = i32::from(nlsf[k])
            .max(floor + spacing(k))
            .min(i32::from(i16::MAX));
        nlsf[k] = value as i16;
        floor = value;
    }
    let mut ceiling = 32768;
    for k in (0..order).rev() {
        let value = i32::from(nlsf[k]).min(ceiling - spacing(k + 1));
        nlsf[k] = value as i16;
        ceiling = value;
    }
}

/// The LSFs of the first half of a 20 ms frame, `w_q2 / 4` of the way from `prev` to `cur`
/// (section 4.2.7.5.5).
pub(super) fn interpolate(prev: &Nlsf, cur: &Nlsf, w_q2: u8, order: usize) -> Nlsf {
    let mut out = [0; MAX_ORDER];
    for k in 0..order {
        let (n0, n2) = (i32::from(prev[k]), i32::from(cur[k]));
        out[k] = (n0 + ((i32::from(w_q2) * (n2 - n0)) >> 2)) as i16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spaced(nlsf: &[i16], min_spacing: &[i16]) -> bool {
        let order = nlsf.len();
        (0..=order).all(|k| {
            let lower = if k == 0 { 0 } else { i32::from(nlsf[k - 1]) };
            let upper = if k == order {
                32768
            } else {
                i32::from(nlsf[k])
            };
            upper - lower >= i32::from(min_spacing[k])
        })
    }

    #[test]
    fn weights_stay_in_the_documented_range() {
        for codebook in [&NB_MB, &WB] {
            for vector in codebook.vectors.chunks_exact(codebook.order) {
                for k in 0..codebook.order {
                    let cb = i32::from(vector[k]);
                    let below = if k == 0 { 0 } else { i32::from(vector[k - 1]) };
                    let above = vector.get(k + 1).map_or(256, |&v| i32::from(v));
                    let weight = inverse_weight(cb - below, above - cb);
                    assert!((1819..=5227).contains(&weight), "{weight}");
                }
            }
        }
    }

    #[test]
    fn stabilization_enforces_the_minimum_spacing() {
        for codebook in [&NB_MB, &WB] {
            let order = codebook.order;
            let cases: [&[i16]; 4] = [
                &[0; 16],
                &[32767; 16],
                &[
                    3000, 2900, 2800, 2700, 10, 20, 30, 32000, 31000, 32767, 5, 6, 7, 8, 9, 10,
                ],
                &[
                    100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115,
                ],
            ];
            for case in cases {
                let mut nlsf = case[..order].to_vec();
                stabilize(&mut nlsf, codebook.min_spacing);
                assert!(spaced(&nlsf, codebook.min_spacing), "{nlsf:?}");
            }
        }
    }

    #[test]
    fn decoded_codebook_vectors_are_stable() {
        for (bandwidth, codebook) in [(Bandwidth::Narrow, &NB_MB), (Bandwidth::Wide, &WB)] {
            assert_eq!(bandwidth.codebook().order, codebook.order);
            for stage1 in 0..32 {
                for residual in [-10, 0, 10] {
                    let indices = NlsfIndices {
                        stage1,
                        stage2: [residual; MAX_ORDER],
                    };
                    let nlsf = indices.decode(codebook);
                    assert!(spaced(&nlsf[..codebook.order], codebook.min_spacing));
                }
            }
        }
    }

    #[test]
    fn interpolation_moves_a_quarter_per_step() {
        let prev = [1000; MAX_ORDER];
        let cur = [2000; MAX_ORDER];
        assert_eq!(interpolate(&prev, &cur, 1, 10)[0], 1250);
        assert_eq!(interpolate(&prev, &cur, 0, 10)[9], 1000);
    }
}
