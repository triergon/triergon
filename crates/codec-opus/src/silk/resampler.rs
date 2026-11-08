//! Resampling of the SILK output to 48 kHz.
//!
//! The resampler is not normative (RFC 6716 section 4.2.9). This one matches FFmpeg's Opus
//! decoder (see RESAMPLER.md): an integer-ratio polyphase interpolator with a 16-tap
//! Kaiser-windowed sinc (beta 9), primed at start with a few zero input samples, that only
//! produces an output once its whole filter span of input is present. With less priming
//! than half the filter span (8 kHz input) the first frame after a start comes out short,
//! and [`Resampler::flush`] returns the held-back samples.

use super::{Bandwidth, MAX_PACKET};

/// Filter taps per output phase.
const TAPS: usize = 16;
/// Taps before the output position's input sample (the others are at or after it).
const BEFORE: usize = 7;
/// Most zero input samples primed at start.
const MAX_PRIMING: usize = 11;
const BUFFER: usize = TAPS + MAX_PRIMING + MAX_PACKET;

/// A streaming resampler for one channel.
#[derive(Clone, Debug)]
pub(super) struct Resampler {
    filter: &'static [[f32; TAPS]],
    /// Zero input samples primed after a start, in input samples.
    priming: usize,
    /// Input history; `input[..len]` is valid, `input[0]` is input sample `base`.
    input: [f32; BUFFER],
    len: usize,
    /// Next output position: input sample `position` (an index into `input`) and phase.
    position: usize,
    phase: usize,
    /// Outputs owed since the start: one per phase for each real (unprimed) input sample.
    owed: usize,
}

impl Resampler {
    pub(super) fn new() -> Resampler {
        let mut resampler = Resampler {
            filter: &INTERPOLATION_WB,
            priming: 0,
            input: [0.0; BUFFER],
            len: 0,
            position: 0,
            phase: 0,
            owed: 0,
        };
        resampler.set_bandwidth(Bandwidth::Wide);
        resampler
    }

    /// Start over for input at the rate of `bandwidth`, with zero history.
    pub(super) fn set_bandwidth(&mut self, bandwidth: Bandwidth) {
        (self.filter, self.priming) = match bandwidth {
            Bandwidth::Narrow => (&INTERPOLATION_NB[..], 4),
            Bandwidth::Medium => (&INTERPOLATION_MB[..], 8),
            Bandwidth::Wide => (&INTERPOLATION_WB[..], 11),
        };
        self.reset();
    }

    /// Start over at the current rate, with zero history.
    pub(super) fn reset(&mut self) {
        // Zero history before the first output position, then the priming zeros.
        self.len = BEFORE + self.priming;
        self.input[..self.len].fill(0.0);
        self.position = BEFORE;
        self.phase = 0;
        self.owed = 0;
    }

    fn ratio(&self) -> usize {
        self.filter.len()
    }

    /// Resample `input` into the front of `output`; returns the samples written, which is
    /// `output.len()` once the filter has filled.
    pub(super) fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        self.compact();
        let take = input.len().min(BUFFER - self.len);
        self.input[self.len..self.len + take].copy_from_slice(&input[..take]);
        self.len += take;
        self.owed += take * self.ratio();
        let produced = self.emit(output, self.len);
        self.owed -= produced;
        produced
    }

    /// Emit the outputs still owed for the input so far, continuing the input by its mirror
    /// image (the last sample repeated first), as FFmpeg's resampler does when flushed;
    /// returns the samples written. The resampler then starts over.
    pub(super) fn flush(&mut self, output: &mut [f32]) -> usize {
        self.compact();
        let owed = self.owed.min(output.len());
        let pad = TAPS.min(BUFFER - self.len).min(self.len);
        for k in 0..pad {
            self.input[self.len + k] = self.input[self.len - 1 - k];
        }
        let produced = self.emit(&mut output[..owed], self.len + pad);
        self.reset();
        produced
    }

    /// Write outputs while their filter span lies within `input[..available]`.
    fn emit(&mut self, output: &mut [f32], available: usize) -> usize {
        let ratio = self.ratio();
        let mut produced = 0;
        for out in output.iter_mut() {
            if self.position + TAPS - BEFORE > available {
                break;
            }
            let span = &self.input[self.position - BEFORE..self.position - BEFORE + TAPS];
            let taps = &self.filter[self.phase];
            *out = span.iter().zip(taps).map(|(x, h)| x * h).sum();
            produced += 1;
            self.phase += 1;
            if self.phase == ratio {
                self.phase = 0;
                self.position += 1;
            }
        }
        produced
    }

    /// Drop input no longer needed by the next output.
    fn compact(&mut self) {
        let start = self.position - BEFORE;
        if start > 0 {
            self.input.copy_within(start..self.len, 0);
            self.len -= start;
            self.position = BEFORE;
        }
    }
}

/// Interpolation filter from 8 kHz: phase p, tap i weights input n - 7 + i for output (n + p / 6).
pub(super) const INTERPOLATION_NB: [[f32; 16]; 6] = [
    [
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    ],
    [
        -0.00022941311,
        0.0012447803,
        -0.0042160847,
        0.011106638,
        -0.0251817,
        0.053516205,
        -0.124593325,
        0.95317286,
        0.18237269,
        -0.06927932,
        0.03244909,
        -0.014777331,
        0.0059519038,
        -0.0019397057,
        0.00043643618,
        -3.7646263e-05,
    ],
    [
        -0.00027521176,
        0.0016971113,
        -0.006085474,
        0.016577573,
        -0.038321458,
        0.08173757,
        -0.18362376,
        0.8209227,
        0.40147382,
        -0.13732189,
        0.06369109,
        -0.02936703,
        0.01214038,
        -0.004130719,
        0.0010071687,
        -0.00011422389,
    ],
    [
        -0.00021090313,
        0.0015215348,
        -0.0058127423,
        0.016421385,
        -0.03878866,
        0.08330598,
        -0.18257746,
        0.6261479,
        0.6261479,
        -0.18257746,
        0.08330598,
        -0.03878866,
        0.016421385,
        -0.0058127423,
        0.0015215348,
        -0.00021090313,
    ],
    [
        -0.00011422389,
        0.0010071687,
        -0.004130719,
        0.01214038,
        -0.02936703,
        0.06369109,
        -0.13732189,
        0.40147382,
        0.8209227,
        -0.18362376,
        0.08173757,
        -0.038321458,
        0.016577573,
        -0.006085474,
        0.0016971113,
        -0.00027521176,
    ],
    [
        -3.7646263e-05,
        0.00043643618,
        -0.0019397057,
        0.0059519038,
        -0.014777331,
        0.03244909,
        -0.06927932,
        0.18237269,
        0.95317286,
        -0.124593325,
        0.053516205,
        -0.0251817,
        0.011106638,
        -0.0042160847,
        0.0012447803,
        -0.00022941311,
    ],
];

/// Interpolation filter from 12 kHz: phase p, tap i weights input n - 7 + i for output (n + p / 4).
pub(super) const INTERPOLATION_MB: [[f32; 16]; 4] = [
    [
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    ],
    [
        -0.00027125436,
        0.0015643511,
        -0.0054477905,
        0.014588848,
        -0.03338973,
        0.07105826,
        -0.16225058,
        0.89659345,
        0.2891,
        -0.10474362,
        0.04885559,
        -0.02239435,
        0.00914069,
        -0.0030452232,
        0.00071419304,
        -7.1491486e-05,
    ],
    [
        -0.00021090313,
        0.0015215348,
        -0.0058127423,
        0.016421385,
        -0.03878866,
        0.08330598,
        -0.18257746,
        0.6261479,
        0.6261479,
        -0.18257746,
        0.08330598,
        -0.03878866,
        0.016421385,
        -0.0058127423,
        0.0015215348,
        -0.00021090313,
    ],
    [
        -7.1491486e-05,
        0.00071419304,
        -0.0030452232,
        0.00914069,
        -0.02239435,
        0.04885559,
        -0.10474362,
        0.2891,
        0.89659345,
        -0.16225058,
        0.07105826,
        -0.03338973,
        0.014588848,
        -0.0054477905,
        0.0015643511,
        -0.00027125436,
    ],
];

/// Interpolation filter from 16 kHz: phase p, tap i weights input n - 7 + i for output (n + p / 3).
pub(super) const INTERPOLATION_WB: [[f32; 16]; 3] = [
    [
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    ],
    [
        -0.00027521176,
        0.0016971113,
        -0.006085474,
        0.016577573,
        -0.038321458,
        0.08173757,
        -0.18362376,
        0.8209227,
        0.40147382,
        -0.13732189,
        0.06369109,
        -0.02936703,
        0.01214038,
        -0.004130719,
        0.0010071687,
        -0.00011422389,
    ],
    [
        -0.00011422389,
        0.0010071687,
        -0.004130719,
        0.01214038,
        -0.02936703,
        0.06369109,
        -0.13732189,
        0.40147382,
        0.8209227,
        -0.18362376,
        0.08173757,
        -0.038321458,
        0.016577573,
        -0.006085474,
        0.0016971113,
        -0.00027521176,
    ],
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The filter the tables tabulate: sinc(t) under a Kaiser window of half-width 8.
    fn kaiser_sinc(t: f64) -> f64 {
        fn i0(x: f64) -> f64 {
            let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
            while term > 1e-20 * sum {
                term *= (x / 2.0 / k) * (x / 2.0 / k);
                sum += term;
                k += 1.0;
            }
            sum
        }
        let z = t / 8.0;
        if z.abs() >= 1.0 || (t != 0.0 && t.fract() == 0.0) {
            return 0.0;
        }
        let sinc = if t == 0.0 {
            1.0
        } else {
            (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t)
        };
        sinc * i0(9.0 * (1.0 - z * z).sqrt()) / i0(9.0)
    }

    #[test]
    fn taps_follow_the_kaiser_sinc() {
        for filter in [
            &INTERPOLATION_NB[..],
            &INTERPOLATION_MB[..],
            &INTERPOLATION_WB[..],
        ] {
            let ratio = filter.len() as f64;
            for (phase, taps) in filter.iter().enumerate() {
                for (i, &tap) in taps.iter().enumerate() {
                    let t = BEFORE as f64 - i as f64 + phase as f64 / ratio;
                    assert!(
                        (f64::from(tap) - kaiser_sinc(t)).abs() < 1e-7,
                        "{phase} {i}"
                    );
                }
            }
        }
    }

    #[test]
    fn narrowband_starts_short_and_flushes_the_rest() {
        let mut resampler = Resampler::new();
        resampler.set_bandwidth(Bandwidth::Narrow);
        let input = [0.5f32; 160];
        let mut output = [0.0f32; 960];
        assert_eq!(resampler.process(&input, &mut output), 936);
        assert_eq!(resampler.process(&input, &mut output), 960);
        // A constant comes through at unit gain once the filter has filled.
        assert!(output.iter().all(|&y| (y - 0.5).abs() < 1e-3));
        assert_eq!(resampler.flush(&mut output), 24);
    }

    #[test]
    fn wideband_has_no_start_latency() {
        let mut resampler = Resampler::new();
        let input = [0.25f32; 320];
        let mut output = [0.0f32; 960];
        assert_eq!(resampler.process(&input, &mut output), 960);
        assert_eq!(resampler.process(&input, &mut output), 960);
        assert_eq!(resampler.flush(&mut output), 0);
    }
}
