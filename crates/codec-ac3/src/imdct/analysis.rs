//! Analysis transforms from A/52:2018 8.2.3.

use super::{Fft, cmul, kbd_window};
use core::f64::consts::PI;

#[derive(Debug)]
struct Forward<const N: usize> {
    fft: Fft<N>,
    cos: [f32; N],
    sin: [f32; N],
    re: [f32; N],
    im: [f32; N],
}

impl<const N: usize> Forward<N> {
    fn new() -> Self {
        let mut cos = [0.0; N];
        let mut sin = [0.0; N];
        for (k, (c, s)) in cos.iter_mut().zip(&mut sin).enumerate() {
            let angle = 2.0 * PI * (8 * k + 1) as f64 / (32 * N) as f64;
            *c = -angle.cos() as f32;
            *s = -angle.sin() as f32;
        }
        Self {
            fft: Fft::new(),
            cos,
            sin,
            re: [0.0; N],
            im: [0.0; N],
        }
    }

    fn run(&mut self, input: impl Fn(usize) -> (f32, f32), emit: impl FnMut(usize, (f32, f32))) {
        let (cos, sin) = (&self.cos, &self.sin);
        // Conjugating the input and output uses the inverse FFT for the forward transform.
        self.fft.run(
            |k| cmul(input(k), (cos[k], sin[k])),
            (cos, sin),
            emit,
            &mut self.re,
            &mut self.im,
        );
    }
}

#[derive(Debug)]
pub(crate) struct Analysis {
    long: Forward<128>,
    short: Forward<64>,
    window: [f32; 256],
}

impl Analysis {
    pub(crate) fn new() -> Self {
        Self {
            long: Forward::new(),
            short: Forward::new(),
            window: kbd_window(),
        }
    }

    pub(crate) fn long(&mut self, previous: &[f32], next: &[f32], out: &mut [f32; 256]) {
        let window = &self.window;
        // Transpose the synthesis window and pair the aliased samples.
        self.long.run(
            |k| {
                if k < 64 {
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
                }
            },
            |k, (re, im)| {
                out[255 - 2 * k] = re / 256.0;
                out[2 * k] = -im / 256.0;
            },
        );
    }

    pub(crate) fn short(&mut self, previous: &[f32], next: &[f32], out: &mut [f32; 256]) {
        let window = &self.window;
        // The first half uses the forward window and feeds the even coefficient slots.
        self.short.run(
            |k| {
                let (a, b) = (2 * k, 255 - 2 * k);
                let (c, d) = (127 - 2 * k, 128 + 2 * k);
                (
                    previous[c] * window[c] - previous[d] * window[d],
                    previous[a] * window[a] - previous[b] * window[b],
                )
            },
            |k, (re, im)| {
                out[2 * (127 - 2 * k)] = re / 128.0;
                out[4 * k] = -im / 128.0;
            },
        );
        // The second half reverses the window and uses the other phase in A/52 7.9.4.2.
        self.short.run(
            |k| {
                let (a, b) = (2 * k, 255 - 2 * k);
                let (c, d) = (127 - 2 * k, 128 + 2 * k);
                (
                    -(next[a] * window[b] + next[b] * window[a]),
                    -(next[c] * window[d] + next[d] * window[c]),
                )
            },
            |k, (re, im)| {
                out[2 * (127 - 2 * k) + 1] = re / 128.0;
                out[4 * k + 1] = -im / 128.0;
            },
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
        analysis.long(&samples[..256], &samples[256..], &mut actual);
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

    #[test]
    fn short_analysis_matches_a52_formula() {
        let samples: [f32; 512] =
            core::array::from_fn(|n| ((n as f32) * 0.31).sin() * 0.6 + (n % 11) as f32 / 100.0);
        let mut analysis = Analysis::new();
        let mut actual = [0.0; 256];
        analysis.short(&samples[..256], &samples[256..], &mut actual);
        for (k, value) in actual.iter().enumerate() {
            let half = k % 2;
            let bin = (k - half) / 2;
            let expected = samples[half * 256..(half + 1) * 256]
                .iter()
                .enumerate()
                .map(|(n, &sample)| {
                    let window = analysis.window[if half == 0 { n } else { 255 - n }];
                    let angle =
                        PI / 256.0 * (n as f64 + 128.0 * half as f64 + 0.5) * (2 * bin + 1) as f64;
                    f64::from(sample * window) * angle.cos()
                })
                .sum::<f64>()
                / -128.0;
            assert!(
                (f64::from(*value) - expected).abs() < 2.0e-6,
                "{k}: {value} vs {expected}"
            );
        }
    }

    #[test]
    fn switching_transforms_preserves_overlap() {
        let mut analysis = Analysis::new();
        let mut synthesis = super::super::Imdct::new();
        let mut coefficients = [0.0; 256];
        let mut delay = [0.0; 256];
        let mut output = [0.0; 256];
        let mut previous = [0.0; 256];
        for (block, short) in [false, true, true, false, true, false]
            .into_iter()
            .enumerate()
        {
            let next: [f32; 256] =
                core::array::from_fn(|i| ((i + block * 256) as f32 * 0.31).sin());
            if short {
                analysis.short(&previous, &next, &mut coefficients);
                synthesis.short(&coefficients, &mut delay, &mut output);
            } else {
                analysis.long(&previous, &next, &mut coefficients);
                synthesis.long(&coefficients, &mut delay, &mut output);
            }
            for (sample, &expected) in output.iter().zip(&previous) {
                assert!(
                    (*sample - expected).abs() < 2.0e-6,
                    "{sample} vs {expected}"
                );
            }
            previous = next;
        }
    }
}
