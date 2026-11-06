//! LFE interpolation (ETSI TS 102 114 clause C.3.7, Annex D.8 FIRs) and the X96 2x LFE
//! interpolator (clause 6.2.4.7, Table 6-11).

use crate::tables::{LFE_FIR_64, LFE_FIR_128};

pub(crate) struct LfeFir {
    c64: Vec<f32>,
    c128: Vec<f32>,
}

impl LfeFir {
    /// `scale` is folded into the coefficients (output normalization).
    pub(crate) fn new(scale: f64) -> Self {
        Self {
            c64: LFE_FIR_64.iter().map(|&c| (c * scale) as f32).collect(),
            c128: LFE_FIR_128.iter().map(|&c| (c * scale) as f32).collect(),
        }
    }
}

/// Decimated-sample history (most recent last).
#[derive(Clone, Copy, Default)]
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
        let coef: &[f32] = if factor == 128 { &fir.c128 } else { &fir.c64 };
        let taps = 512 / factor;
        // x[j] for j = -7..len: window = hist ++ input.
        let mut x = [0f32; 8 + 64];
        x[..8].copy_from_slice(&self.hist);
        x[8..8 + input.len()].copy_from_slice(input);
        for (n, chunk) in out.chunks_exact_mut(factor).enumerate().take(input.len()) {
            chunk.iter_mut().for_each(|v| *v = 0.0);
            for j in 0..taps {
                let s = x[8 + n - j];
                let c = &coef[j * factor..j * factor + factor];
                for k in 0..factor {
                    chunk[k] += s * c[k];
                }
            }
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
}

/// X96 2x LFE interpolator.
///
/// Clause 6.2.4.7 specifies the 5-tap filter of Table 6-11. FFmpeg's native decoder (the
/// reference) instead produces `z[2n] = 0.75 x[n-1] + 0.25 x[n]` and
/// `z[2n+1] = 0.25 x[n-1] + 0.75 x[n]` (least-squares fit of its output on the
/// x96_51_24_96_1509 vector: residual 6e-8). This decoder follows the reference and
/// deviates from Table 6-11.
#[derive(Clone, Copy, Default)]
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
