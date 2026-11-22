//! Isolated AAC-LC 960-sample synthesis (ISO/IEC 14496-3, 4.6.11).
//!
//! Each call accepts exactly 960 coefficients: one long spectrum or eight consecutive
//! 120-coefficient short spectra. Output and caller-owned overlap are each 960 samples.
//! Transform allocations are charged to the supplied budget; the fixed windows
//! and block scratch occupy 17,280 bytes inline in the owning decoder. Window construction additionally uses
//! a temporary, fixed-size 961-element f64 kernel in the shared KBD routine. The
//! transform allocations fit within a 32 KiB budget plus pool control on the test host;
//! a refused charge returns an error and releases all earlier charges.

use triergon_codec::{Budget, StorageError, mdct::Imdct};

const LONG: usize = 960;
const SHORT: usize = 120;
const OFFSET: usize = (LONG - SHORT) / 2;
const PCM_SCALE: f64 = 1.0 / 32768.0;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Sequence {
    OnlyLong,
    LongStart,
    EightShort,
    LongStop,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Shape {
    Sine,
    Kbd,
}

impl Shape {
    fn index(self) -> usize {
        match self {
            Self::Sine => 0,
            Self::Kbd => 1,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Filterbank960 {
    long: Imdct,
    short: Imdct,
    long_windows: [[f32; LONG]; 2],
    short_windows: [[f32; SHORT]; 2],
    block: [f32; 2 * LONG],
    short_block: [f32; 2 * SHORT],
}

impl Filterbank960 {
    pub(crate) fn new(budget: &Budget) -> Result<Self, StorageError> {
        let mut long_windows = [[0.0; LONG]; 2];
        let mut short_windows = [[0.0; SHORT]; 2];
        for (n, value) in long_windows[0].iter_mut().enumerate() {
            *value = (std::f64::consts::PI * (n as f64 + 0.5) / (2 * LONG) as f64).sin() as f32;
        }
        for (n, value) in short_windows[0].iter_mut().enumerate() {
            *value = (std::f64::consts::PI * (n as f64 + 0.5) / (2 * SHORT) as f64).sin() as f32;
        }
        triergon_codec::kbd_window(4.0, &mut long_windows[1]);
        triergon_codec::kbd_window(6.0, &mut short_windows[1]);
        Ok(Self {
            long: Imdct::new(2 * LONG, PCM_SCALE / LONG as f64, budget)?,
            short: Imdct::new(2 * SHORT, PCM_SCALE / SHORT as f64, budget)?,
            long_windows,
            short_windows,
            block: [0.0; 2 * LONG],
            short_block: [0.0; 2 * SHORT],
        })
    }

    pub(crate) fn synth(
        &mut self,
        coefficients: &[f32; LONG],
        sequence: Sequence,
        shape: Shape,
        previous: Shape,
        overlap: &mut [f32; LONG],
        output: &mut [f32; LONG],
    ) {
        let long_prev = &self.long_windows[previous.index()];
        let long_cur = &self.long_windows[shape.index()];
        let short_prev = &self.short_windows[previous.index()];
        let short_cur = &self.short_windows[shape.index()];
        if matches!(sequence, Sequence::EightShort) {
            self.block.fill(0.0);
            for window in 0..8 {
                self.short.imdct(
                    &coefficients[window * SHORT..(window + 1) * SHORT],
                    &mut self.short_block,
                );
                let rise = if window == 0 { short_prev } else { short_cur };
                let base = OFFSET + window * SHORT;
                for i in 0..SHORT {
                    self.block[base + i] += self.short_block[i] * rise[i];
                    self.block[base + SHORT + i] +=
                        self.short_block[SHORT + i] * short_cur[SHORT - 1 - i];
                }
            }
        } else {
            self.long.imdct(coefficients, &mut self.block);
            for i in 0..LONG {
                let rise = if matches!(sequence, Sequence::LongStop) {
                    if i < OFFSET {
                        0.0
                    } else if i < OFFSET + SHORT {
                        short_prev[i - OFFSET]
                    } else {
                        1.0
                    }
                } else {
                    long_prev[i]
                };
                let fall = if matches!(sequence, Sequence::LongStart) {
                    if i < OFFSET {
                        1.0
                    } else if i < OFFSET + SHORT {
                        short_cur[OFFSET + SHORT - 1 - i]
                    } else {
                        0.0
                    }
                } else {
                    long_cur[LONG - 1 - i]
                };
                self.block[i] *= rise;
                self.block[LONG + i] *= fall;
            }
        }
        for i in 0..LONG {
            output[i] = overlap[i] + self.block[i];
            overlap[i] = self.block[LONG + i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    // Independent direct cosine sum, not a DCT or FFT factorization.
    fn inverse(x: &[f32]) -> Vec<f64> {
        let k = x.len();
        (0..2 * k)
            .map(|n| {
                x.iter()
                    .enumerate()
                    .map(|(j, &c)| {
                        f64::from(c)
                            * (PI / k as f64 * (n as f64 + 0.5 + k as f64 / 2.0) * (j as f64 + 0.5))
                                .cos()
                    })
                    .sum::<f64>()
                    * PCM_SCALE
                    / k as f64
            })
            .collect()
    }

    // Direct KBD definition; recomputes the normalization independently of production.
    fn window(shape: Shape, size: usize) -> Vec<f64> {
        if matches!(shape, Shape::Sine) {
            return (0..size)
                .map(|i| (PI * (i as f64 + 0.5) / (2 * size) as f64).sin())
                .collect();
        }
        let alpha = if size == LONG { 4.0 } else { 6.0 };
        let kernel: Vec<f64> = (0..=size)
            .map(|i| {
                let x = PI
                    * alpha
                    * (1.0 - (2.0 * i as f64 / size as f64 - 1.0).powi(2))
                        .max(0.0)
                        .sqrt();
                let mut term = 1.0;
                let mut sum = 1.0;
                for j in 1..80 {
                    term *= x * x / (4.0 * (j * j) as f64);
                    sum += term;
                }
                sum
            })
            .collect();
        let total: f64 = kernel.iter().sum();
        (0..size)
            .map(|i| (kernel[..=i].iter().sum::<f64>() / total).sqrt())
            .collect()
    }

    fn reference(x: &[f32; LONG], seq: Sequence, shape: Shape, prev: Shape) -> Vec<f64> {
        let lp = window(prev, LONG);
        let lc = window(shape, LONG);
        let sp = window(prev, SHORT);
        let sc = window(shape, SHORT);
        let mut block = vec![0.0; 2 * LONG];
        if matches!(seq, Sequence::EightShort) {
            for w in 0..8 {
                let y = inverse(&x[w * SHORT..(w + 1) * SHORT]);
                for i in 0..2 * SHORT {
                    let weight = if i < SHORT {
                        if w == 0 { sp[i] } else { sc[i] }
                    } else {
                        sc[2 * SHORT - 1 - i]
                    };
                    block[OFFSET + w * SHORT + i] += y[i] * weight;
                }
            }
        } else {
            let y = inverse(x);
            for i in 0..2 * LONG {
                let weight = if i < LONG {
                    if matches!(seq, Sequence::LongStop) {
                        if i < OFFSET {
                            0.0
                        } else if i < OFFSET + SHORT {
                            sp[i - OFFSET]
                        } else {
                            1.0
                        }
                    } else {
                        lp[i]
                    }
                } else {
                    let j = i - LONG;
                    if matches!(seq, Sequence::LongStart) {
                        if j < OFFSET {
                            1.0
                        } else if j < OFFSET + SHORT {
                            sc[SHORT - 1 - (j - OFFSET)]
                        } else {
                            0.0
                        }
                    } else {
                        lc[LONG - 1 - j]
                    }
                };
                block[i] = y[i] * weight;
            }
        }
        block
    }

    #[test]
    fn reference_all_sequences_shapes_and_overlap_transitions() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut fb = Filterbank960::new(&budget).unwrap();
        let mut overlap = [0.0; LONG];
        let mut expected_overlap = vec![0.0; LONG];
        let mut out = [0.0; LONG];
        let sequences = [
            Sequence::OnlyLong,
            Sequence::LongStart,
            Sequence::EightShort,
            Sequence::EightShort,
            Sequence::LongStop,
            Sequence::OnlyLong,
        ];
        for initial in [Shape::Sine, Shape::Kbd] {
            for next in [Shape::Sine, Shape::Kbd] {
                let mut prev = initial;
                for (frame, &seq) in sequences.iter().enumerate() {
                    let shape = if matches!(initial, Shape::Sine) || frame % 2 == 0 {
                        next
                    } else {
                        match next {
                            Shape::Sine => Shape::Kbd,
                            Shape::Kbd => Shape::Sine,
                        }
                    };
                    let x = std::array::from_fn(|i| {
                        (((i * 73 + frame * 17) % 211) as f32 - 105.0) * 32.0
                    });
                    let block = reference(&x, seq, shape, prev);
                    fb.synth(&x, seq, shape, prev, &mut overlap, &mut out);
                    for i in 0..LONG {
                        let expected = block[i] + expected_overlap[i];
                        assert!(
                            (f64::from(out[i]) - expected).abs() < 2e-8,
                            "{seq:?} {initial:?} {next:?} sample {i}: {} != {expected}",
                            out[i]
                        );
                        assert!((f64::from(overlap[i]) - block[LONG + i]).abs() < 2e-8);
                    }
                    assert!(out.iter().any(|v| v.abs() > 1e-5));
                    expected_overlap.copy_from_slice(&block[LONG..]);
                    prev = shape;
                }
            }
        }
    }

    #[test]
    fn transform_budget_is_enforced_and_released() {
        let small = Budget::new(Budget::control_storage_bytes()).unwrap();
        assert!(Filterbank960::new(&small).is_err());
        assert_eq!(small.used(), Budget::control_storage_bytes());
        let budget = Budget::new(Budget::control_storage_bytes() + 32 * 1024).unwrap();
        let fb = Filterbank960::new(&budget).unwrap();
        assert!(budget.used() > Budget::control_storage_bytes());
        drop(fb);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}
