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
    /// Column `k` of the 18-point DCT-IV behind the 36-point IMDCT,
    /// `cos(π/72 (2m + 1)(2k + 1))` for `m < 18`, padded with zeros to whole vectors.
    dct18: [[f32; 20]; 18],
    /// Column `k` of the 6-point DCT-IV behind the 12-point IMDCT,
    /// `cos(π/24 (2m + 1)(2k + 1))` for `m < 6`, padded likewise.
    dct6: [[f32; 8]; 6],
    /// Long windows of block types 0 (normal), 1 (start) and 3 (stop).
    normal: [f32; 36],
    start: [f32; 36],
    stop: [f32; 36],
    /// The short window.
    short: [f32; 12],
    /// `(cs_i, ca_i)` of the alias-reduction butterflies.
    alias: [(f32, f32); 8],
}

/// Columns of the `n`-point DCT-IV, `cos(π/4n (2m + 1)(2k + 1))`, padded to `L` lanes.
fn dct_iv_columns<const N: usize, const L: usize>(n: usize) -> [[f32; L]; N] {
    core::array::from_fn(|k| {
        core::array::from_fn(|m| {
            let angle = core::f64::consts::PI / (4 * n) as f64 * ((2 * m + 1) * (2 * k + 1)) as f64;
            if m < n { angle.cos() as f32 } else { 0.0 }
        })
    })
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
        dct18: dct_iv_columns(18),
        dct6: dct_iv_columns(6),
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

    /// Transform the requantized, stereo-processed spectrum `xr` of one granule, whose
    /// lines from `lines` on are +0, into the 18 time samples of each of the 32
    /// subbands. `xr` is used as scratch.
    pub(crate) fn run(
        &mut self,
        xr: &mut [f32; 576],
        lines: usize,
        block: BlockType,
        mixed: bool,
        bands: &Bands,
        out: &mut [[f32; SUBBAND]; 32],
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
        // Short-block reordering moves lines between subbands; long blocks keep them.
        let searched = if long_subbands < 32 {
            32
        } else {
            lines.div_ceil(SUBBAND).min(32)
        };
        let mut active = subbands[..searched]
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
        let pairs = subbands.iter().zip(&mut self.overlap).zip(out.iter_mut());
        for (sb, ((coefficients, overlap), samples)) in pairs.enumerate() {
            if sb >= active {
                *samples = *overlap;
                *overlap = [0.0; SUBBAND];
            } else if sb < long_subbands {
                let window = match block {
                    _ if mixed && sb < 2 => &tables.normal,
                    BlockType::Start => &tables.start,
                    BlockType::Stop => &tables.stop,
                    _ => &tables.normal,
                };
                long_imdct(tables, coefficients, window, overlap, samples);
            } else {
                short_imdct(tables, coefficients, overlap, samples);
            }
            if sb % 2 == 1 {
                // Frequency inversion: odd time samples of odd subbands change sign.
                for sample in samples.iter_mut().skip(1).step_by(2) {
                    *sample = -*sample;
                }
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
    for n in 0..9 {
        out[n] = overlap[n] + w[n + 9] * window[n];
    }
    for n in 9..18 {
        out[n] = overlap[n] + -w[26 - n] * window[n];
    }
    for n in 18..27 {
        overlap[n - 18] = -w[26 - n] * window[n];
    }
    for n in 27..36 {
        overlap[n - 18] = -w[n - 27] * window[n];
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

/// `w[m] = sum_k x[k] columns[k][m]`, accumulated a column at a time so the sum over
/// the `L` lanes (`m`, padded to whole vectors) vectorizes. Trailing zero coefficients
/// are skipped: adding a zero product to a sum that starts at +0 never changes it.
/// Kept out of line, so the use of its outputs does not reshape the vector loop.
#[inline(never)]
fn dct_iv<const N: usize, const L: usize>(columns: &[[f32; L]; N], x: &[f32; N]) -> [f32; L] {
    let used = x.iter().rposition(|&v| v != 0.0).map_or(0, |last| last + 1);
    let mut w = [0.0; L];
    for (column, &value) in columns[..used].iter().zip(x) {
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
