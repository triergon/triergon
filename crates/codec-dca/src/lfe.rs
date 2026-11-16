//! LFE interpolation (ETSI TS 102 114 clause C.3.7, Annex D.8 FIRs) and the X96 2x LFE
//! interpolator (clause 6.2.4.7, Table 6-11).

use crate::tables::{LFE_FIR_64, LFE_FIR_128};

/// Outputs computed together by the interpolator.
const GROUP: usize = 8;

#[derive(Debug)]
pub(crate) struct LfeFir {
    c64: Vec<f32>,
    c128: Vec<f32>,
}

impl LfeFir {
    /// `scale` is folded into the coefficients (output normalization).
    pub(crate) fn new(scale: f64) -> Self {
        Self {
            c64: Self::regroup(&LFE_FIR_64, 64, scale),
            c128: Self::regroup(&LFE_FIR_128, 128, scale),
        }
    }

    /// Prototype `h[factor j + k]` (tap `j` of output phase `k`) in the order the
    /// interpolator reads it: for each group of `GROUP` phases, tap by tap.
    fn regroup(proto: &[f64], factor: usize, scale: f64) -> Vec<f32> {
        let taps = proto.len() / factor;
        (0..factor / GROUP)
            .flat_map(|g| {
                (0..taps).flat_map(move |j| {
                    (0..GROUP).map(move |i| (proto[factor * j + GROUP * g + i] * scale) as f32)
                })
            })
            .collect()
    }
}

/// Decimated-sample history (most recent last).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LfeState {
    hist: [f32; 8],
}

impl LfeState {
    pub(crate) fn reset(&mut self) {
        self.hist = [0.0; 8];
    }

    /// Interpolate `input` decimated samples by `factor` (64 or 128) into `out`
    /// (`input.len() * factor` samples).
    pub(crate) fn interpolate(
        &mut self,
        fir: &LfeFir,
        factor: usize,
        input: &[f32],
        out: &mut [f32],
    ) {
        if factor == 128 {
            Self::run::<4>(&self.hist, &fir.c128, input, out);
        } else {
            Self::run::<8>(&self.hist, &fir.c64, input, out);
        }
        // History for the next frame: the last 8 decimated samples. When the frame holds
        // fewer than 8 (8-block frames), FFmpeg (reference) refills the history by repeating
        // the frame's samples cyclically, end aligned (observed with the `blocks256s` and
        // `lfe128_short` synthetic streams). Frames of 16+ blocks are unaffected.
        let len = input.len();
        if len >= 8 {
            self.hist.copy_from_slice(&input[len - 8..]);
        } else if len > 0 {
            for m in 0..8 {
                self.hist[7 - m] = input[len - 1 - (m % len)];
            }
        }
    }

    /// `out[factor n + k] = sum_j x[n - j] h[factor j + k]` with `TAPS = 512 / factor`.
    fn run<const TAPS: usize>(hist: &[f32; 8], coef: &[f32], input: &[f32], out: &mut [f32]) {
        let factor = 512 / TAPS;
        // x[j] for j = -7..len: window = hist ++ input.
        let mut x = [0f32; 8 + 64];
        x[..8].copy_from_slice(hist);
        x[8..8 + input.len()].copy_from_slice(input);
        for (n, chunk) in out.chunks_exact_mut(factor).enumerate().take(input.len()) {
            let s: [f32; TAPS] = std::array::from_fn(|j| x[8 + n - j]);
            // Each group of outputs accumulates over all taps in registers.
            for (o, c) in chunk
                .chunks_exact_mut(GROUP)
                .zip(coef.chunks_exact(TAPS * GROUP))
            {
                let mut acc = [0f32; GROUP];
                for (&s, c) in s.iter().zip(c.chunks_exact(GROUP)) {
                    for i in 0..GROUP {
                        acc[i] += s * c[i];
                    }
                }
                o.copy_from_slice(&acc);
            }
        }
    }
}

/// X96 2x LFE interpolator.
///
/// Clause 6.2.4.7 specifies the 5-tap filter of Table 6-11. FFmpeg's native decoder (the
/// reference) instead produces `z[2n] = 0.75 x[n-1] + 0.25 x[n]` and
/// `z[2n+1] = 0.25 x[n-1] + 0.75 x[n]` (least-squares fit of its output on the
/// x96_51_24_96_1509 vector: residual 6e-8). This decoder follows the reference and
/// deviates from Table 6-11.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Lfe2x {
    x1: f32,
}

impl Lfe2x {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// `out.len() == 2 * input.len()`.
    pub(crate) fn interpolate(&mut self, input: &[f32], out: &mut [f32]) {
        let mut x1 = self.x1;
        for (n, &x) in input.iter().enumerate() {
            out[2 * n] = 0.75 * x1 + 0.25 * x;
            out[2 * n + 1] = 0.25 * x1 + 0.75 * x;
            x1 = x;
        }
        self.x1 = x1;
    }
}
