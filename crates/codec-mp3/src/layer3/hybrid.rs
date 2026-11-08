//! The hybrid filterbank's MDCT half (ISO/IEC 11172-3 2.4.3.4.8 to 2.4.3.4.10): short
//! block reordering, alias reduction, the 36- and 12-point IMDCT with the four window
//! shapes, overlap-add and frequency inversion. Its output feeds the polyphase synthesis.

use std::sync::LazyLock;

use super::bands::Bands;
use super::side_info::BlockType;

/// Lines of one subband.
const SUBBAND: usize = 18;

/// Alias-reduction coefficients `c_i` of Table B.9.
const ALIAS_C: [f64; 8] = [
    -0.6, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037,
];

struct Tables {
    /// `cos(π/72 (2m + 1)(2k + 1))`: the 18-point DCT-IV behind the 36-point IMDCT
    /// (symmetric in `m` and `k`).
    dct18: [[f32; 18]; 18],
    /// `cos(π/24 (2m + 1)(2k + 1))`: the 6-point DCT-IV behind the 12-point IMDCT
    /// (symmetric too).
    dct6: [[f32; 6]; 6],
    /// Long windows of block types 0 (normal), 1 (start) and 3 (stop).
    normal: [f32; 36],
    start: [f32; 36],
    stop: [f32; 36],
    /// The short window.
    short: [f32; 12],
    /// `(cs_i, ca_i)` of the alias-reduction butterflies.
    alias: [(f32, f32); 8],
}

static TABLES: LazyLock<Tables> = LazyLock::new(|| {
    use core::f64::consts::PI;
    let sine = |n: usize, i: usize| (PI / n as f64 * (i as f64 + 0.5)).sin() as f32;
    let normal = core::array::from_fn(|i| sine(36, i));
    let start = core::array::from_fn(|i| match i {
        0..18 => sine(36, i),
        18..24 => 1.0,
        24..30 => sine(12, i - 18),
        _ => 0.0,
    });
    let stop = core::array::from_fn(|i| match i {
        0..6 => 0.0,
        6..12 => sine(12, i - 6),
        12..18 => 1.0,
        _ => sine(36, i),
    });
    Tables {
        dct18: core::array::from_fn(|m| {
            core::array::from_fn(|k| (PI / 72.0 * ((2 * m + 1) * (2 * k + 1)) as f64).cos() as f32)
        }),
        dct6: core::array::from_fn(|m| {
            core::array::from_fn(|k| (PI / 24.0 * ((2 * m + 1) * (2 * k + 1)) as f64).cos() as f32)
        }),
        normal,
        start,
        stop,
        short: core::array::from_fn(|i| sine(12, i)),
        alias: ALIAS_C.map(|c| {
            let norm = (1.0 + c * c).sqrt();
            ((1.0 / norm) as f32, (c / norm) as f32)
        }),
    }
});

/// The overlap of one channel between granules.
#[derive(Clone, Debug)]
pub(crate) struct Hybrid {
    overlap: [[f32; SUBBAND]; 32],
}

impl Default for Hybrid {
    fn default() -> Self {
        Hybrid {
            overlap: [[0.0; SUBBAND]; 32],
        }
    }
}

impl Hybrid {
    pub(crate) fn reset(&mut self) {
        *self = Hybrid::default();
    }

    /// Transform the requantized, stereo-processed spectrum `xr` of one granule into 18
    /// time slots of 32 subband samples. `xr` is used as scratch.
    pub(crate) fn run(
        &mut self,
        xr: &mut [f32; 576],
        block: BlockType,
        mixed: bool,
        bands: &Bands,
        out: &mut [[f32; 32]; SUBBAND],
    ) {
        let tables = &*TABLES;
        let (long_subbands, aliased) = match (block, mixed) {
            (BlockType::Short, false) => (0, 0..0),
            (BlockType::Short, true) => (2, 1..2),
            _ => (32, 1..32),
        };
        if long_subbands < 32 {
            reorder(xr, bands, long_subbands * SUBBAND);
        }
        let (subbands, []) = xr.as_chunks_mut::<SUBBAND>() else {
            unreachable!("a granule is 32 subbands");
        };
        let mut active = subbands
            .iter()
            .rposition(|subband| subband.iter().any(|&v| v != 0.0))
            .map_or(0, |last| last + 1);
        // Alias reduction: butterflies across each boundary between long subbands, of
        // which only those next to a nonzero subband change anything.
        let boundaries = aliased.start..aliased.end.min(active + 1);
        for boundary in boundaries.clone() {
            let (low, high) = subbands.split_at_mut(boundary);
            let (low, high) = (&mut low[boundary - 1], &mut high[0]);
            for (i, &(cs, ca)) in tables.alias.iter().enumerate() {
                let (a, b) = (low[SUBBAND - 1 - i], high[i]);
                low[SUBBAND - 1 - i] = a * cs - b * ca;
                high[i] = b * cs + a * ca;
            }
        }
        if !boundaries.is_empty() {
            active = active.max(boundaries.end);
        }
        for (sb, (coefficients, overlap)) in subbands.iter().zip(&mut self.overlap).enumerate() {
            let mut samples = [0.0; SUBBAND];
            if sb >= active {
                samples = *overlap;
                *overlap = [0.0; SUBBAND];
            } else if sb < long_subbands {
                let window = match block {
                    _ if mixed && sb < 2 => &tables.normal,
                    BlockType::Start => &tables.start,
                    BlockType::Stop => &tables.stop,
                    _ => &tables.normal,
                };
                long_imdct(tables, coefficients, window, overlap, &mut samples);
            } else {
                short_imdct(tables, coefficients, overlap, &mut samples);
            }
            for (t, sample) in samples.into_iter().enumerate() {
                // Frequency inversion: odd time samples of odd subbands change sign.
                out[t][sb] = if sb & t & 1 == 1 { -sample } else { sample };
            }
        }
    }
}

/// Put the short-block lines of `xr` from line `from` in subband order: window `w` of
/// frequency `f` goes to `(f / 6) * 18 + w * 6 + f % 6`, where the bitstream orders them
/// by band, window and frequency.
fn reorder(xr: &mut [f32; 576], bands: &Bands, from: usize) {
    let mut sorted = [0.0; 576];
    for b in 0..13 {
        let band = bands.short_band(b);
        if 3 * band.start < from {
            continue;
        }
        let width = band.len();
        for w in 0..3 {
            let source = 3 * band.start + w * width;
            for (j, &value) in xr[source..source + width].iter().enumerate() {
                let f = band.start + j;
                sorted[f / 6 * SUBBAND + w * 6 + f % 6] = value;
            }
        }
    }
    xr[from..].copy_from_slice(&sorted[from..]);
}

/// 36-point IMDCT of one subband, windowed and overlapped.
fn long_imdct(
    tables: &Tables,
    x: &[f32; SUBBAND],
    window: &[f32; 36],
    overlap: &mut [f32; SUBBAND],
    out: &mut [f32; SUBBAND],
) {
    let w = dct_iv(&tables.dct18, x);
    // y[n] = w[n + 9] for n < 9, -w[26 - n] for n < 27, -w[n - 27] after.
    for n in 0..SUBBAND {
        let y = if n < 9 { w[n + 9] } else { -w[26 - n] };
        out[n] = overlap[n] + y * window[n];
    }
    for n in SUBBAND..36 {
        let y = if n < 27 { -w[26 - n] } else { -w[n - 27] };
        overlap[n - SUBBAND] = y * window[n];
    }
}

/// The three 12-point IMDCTs of a short-block subband, windowed, placed at offsets 6,
/// 12 and 18 of the 36-sample block and overlapped.
fn short_imdct(
    tables: &Tables,
    x: &[f32; SUBBAND],
    overlap: &mut [f32; SUBBAND],
    out: &mut [f32; SUBBAND],
) {
    let mut block = [0.0f32; 36];
    for (window, coefficients) in x.as_chunks::<6>().0.iter().enumerate() {
        let w = dct_iv(&tables.dct6, coefficients);
        for n in 0..12 {
            // y[n] = w[n + 3] for n < 3, -w[8 - n] for n < 9, -w[n - 9] after.
            let y = match n {
                0..3 => w[n + 3],
                3..9 => -w[8 - n],
                _ => -w[n - 9],
            };
            block[6 + 6 * window + n] += y * tables.short[n];
        }
    }
    for n in 0..SUBBAND {
        out[n] = overlap[n] + block[n];
        overlap[n] = block[SUBBAND + n];
    }
}

/// `w[m] = sum_k x[k] matrix[k][m]` for a symmetric `matrix`, accumulated a column at a
/// time so the sum over `m` vectorizes.
#[inline(always)]
fn dct_iv<const N: usize>(matrix: &[[f32; N]; N], x: &[f32; N]) -> [f32; N] {
    let mut w = [0.0; N];
    for (column, &value) in matrix.iter().zip(x) {
        for (out, &c) in w.iter_mut().zip(column) {
            *out += c * value;
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The IMDCT of ISO/IEC 11172-3 2.4.3.4.10.2 evaluated directly.
    fn direct(x: &[f32], n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| {
                x.iter()
                    .enumerate()
                    .map(|(k, &v)| {
                        f64::from(v)
                            * (core::f64::consts::PI / (2 * n) as f64
                                * (2 * i + 1 + n / 2) as f64
                                * (2 * k + 1) as f64)
                                .cos()
                    })
                    .sum()
            })
            .collect()
    }

    #[test]
    fn long_and_short_imdct_match_the_definition() {
        let tables = &*TABLES;
        let x: [f32; 18] = core::array::from_fn(|k| ((k * 7 % 11) as f32 - 5.0) / 3.0);
        let reference = direct(&x, 36);
        let ones = [1.0; 36];
        let mut overlap = [0.0; SUBBAND];
        let mut out = [0.0; SUBBAND];
        long_imdct(tables, &x, &ones, &mut overlap, &mut out);
        for n in 0..36 {
            let got = if n < 18 { out[n] } else { overlap[n - 18] };
            assert!((f64::from(got) - reference[n]).abs() < 1e-5, "{n}");
        }
        let short = direct(&x[..6], 12);
        let w = dct_iv(&tables.dct6, x.first_chunk::<6>().unwrap());
        let unfolded: Vec<f32> = (0..12)
            .map(|n| match n {
                0..3 => w[n + 3],
                3..9 => -w[8 - n],
                _ => -w[n - 9],
            })
            .collect();
        for n in 0..12 {
            assert!((f64::from(unfolded[n]) - short[n]).abs() < 1e-5, "{n}");
        }
    }
}
