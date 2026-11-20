//! Long-block analysis transform from A/52:2018 8.2.3.

use super::{Fft, cmul, kbd_window};
use core::f64::consts::PI;

#[derive(Debug)]
pub(crate) struct Analysis {
    fft: Fft<128>,
    cos: [f32; 128],
    sin: [f32; 128],
    window: [f32; 256],
    re: [f32; 128],
    im: [f32; 128],
}

impl Analysis {
    pub(crate) fn new() -> Self {
        let mut cos = [0.0; 128];
        let mut sin = [0.0; 128];
        for (k, (c, s)) in cos.iter_mut().zip(&mut sin).enumerate() {
            let angle = 2.0 * PI * (8 * k + 1) as f64 / (8.0 * 512.0);
            *c = -angle.cos() as f32;
            *s = -angle.sin() as f32;
        }
        Self {
            fft: Fft::new(),
            cos,
            sin,
            window: kbd_window(),
            re: [0.0; 128],
            im: [0.0; 128],
        }
    }

    pub(crate) fn apply(&mut self, previous: &[f32], next: &[f32], out: &mut [f32; 256]) {
        let window = &self.window;
        let (cos, sin) = (&self.cos, &self.sin);
        // Transpose the synthesis window and pair the aliased samples. Conjugating
        // the input and output uses the same inverse FFT for the forward transform.
        let input = |k: usize| {
            let (re, im) = if k < 64 {
                let (a, b) = (127 - 2 * k, 128 + 2 * k);
                (
                    previous[a] * window[a] - previous[b] * window[b],
                    -(next[a] * window[b] + next[b] * window[a]),
                )
            } else {
                let (a, b) = (2 * (k - 64), 255 - 2 * (k - 64));
                (
                    -(next[a] * window[b] + next[b] * window[a]),
                    previous[a] * window[a] - previous[b] * window[b],
                )
            };
            cmul((re, im), (cos[k], sin[k]))
        };
        self.fft.run(
            input,
            (cos, sin),
            |k, (re, im)| {
                out[255 - 2 * k] = re / 256.0;
                out[2 * k] = -im / 256.0;
            },
            &mut self.re,
            &mut self.im,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_analysis_matches_a52_formula() {
        let samples: [f32; 512] =
            core::array::from_fn(|n| ((n as f32) * 0.31).sin() * 0.6 + (n % 11) as f32 / 100.0);
        let mut analysis = Analysis::new();
        let mut actual = [0.0; 256];
        analysis.apply(&samples[..256], &samples[256..], &mut actual);
        for (k, value) in actual.iter().enumerate() {
            let expected = samples
                .iter()
                .enumerate()
                .map(|(n, &sample)| {
                    let window = analysis.window[n.min(511 - n)];
                    let angle = PI / 512.0 * (n as f64 + 128.5) * (2 * k + 1) as f64;
                    f64::from(sample * window) * angle.cos()
                })
                .sum::<f64>()
                / -256.0;
            assert!(
                (f64::from(*value) - expected).abs() < 2.0e-6,
                "{k}: {value} vs {expected}"
            );
        }
    }
}
