//! Requantization of the Huffman-decoded spectrum (ISO/IEC 11172-3 2.4.3.4.7.1):
//! `xr = sign(is) |is|^(4/3) 2^(gain / 4)` with the gain of each scalefactor band.

use std::sync::LazyLock;

use super::bands::Bands;
use super::scalefactors::Scalefactors;
use super::side_info::{BlockType, GranuleChannel};

/// The largest quantized magnitude: table value 15 plus 13 linbits.
pub(crate) const MAX_MAGNITUDE: usize = 15 + (1 << 13) - 1;

/// `|is|^(4/3)` for every magnitude a stream can code.
static POW_4_3: LazyLock<Box<[f32; MAX_MAGNITUDE + 1]>> =
    LazyLock::new(|| Box::new(core::array::from_fn(|i| (i as f64).powf(4.0 / 3.0) as f32)));

/// `|is|^(4/3)` for every magnitude a stream can code.
pub(crate) fn pow_4_3() -> &'static [f32; MAX_MAGNITUDE + 1] {
    &POW_4_3
}

/// Pre-emphasis added to long band `b` when `preflag` is set (Table B.6).
const PRETAB: [u8; 22] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 3, 2, 0,
];

/// The lines a mixed block keeps long: its two lowest subbands.
const MIXED_LONG_LINES: usize = 36;

/// `2^(quarters / 4)`, for the exponents a stream can code (within the normal range).
pub(crate) fn pow2_quarters(quarters: i32) -> f32 {
    const FRACTIONS: [f32; 4] = [1.0, 1.189_207_1, std::f32::consts::SQRT_2, 1.681_792_8];
    let exponent = quarters.div_euclid(4).clamp(-126, 127);
    f32::from_bits(((exponent + 127) as u32) << 23) * FRACTIONS[quarters.rem_euclid(4) as usize]
}

/// Scale the first `lines` lines of `xr`, `sign(is) |is|^(4/3)` from the Huffman
/// decoder, by the gain of their band; the rest are 0 already.
pub(crate) fn requantize(
    xr: &mut [f32; 576],
    lines: usize,
    g: &GranuleChannel,
    sf: &Scalefactors,
    bands: &Bands,
) {
    let shift = if g.scalefac_scale { 4 } else { 2 };
    let base = i32::from(g.global_gain) - 210;
    let mut scale = |range: core::ops::Range<usize>, quarters: i32| {
        let range = range.start.min(lines)..range.end.min(lines);
        let gain = pow2_quarters(quarters);
        for x in &mut xr[range] {
            *x *= gain;
        }
    };
    let short_from = match (g.block_type, g.mixed) {
        (BlockType::Short, false) => 0,
        (BlockType::Short, true) => {
            for b in (0..22).take_while(|&b| usize::from(bands.long[b + 1]) <= MIXED_LONG_LINES) {
                scale(bands.long_band(b), base - shift * i32::from(sf.long[b]));
            }
            (0..13)
                .find(|&b| 3 * usize::from(bands.short[b]) >= MIXED_LONG_LINES)
                .unwrap_or(13)
        }
        _ => {
            for b in (0..22).take_while(|&b| usize::from(bands.long[b]) < lines) {
                let pre = if g.preflag { PRETAB[b] } else { 0 };
                scale(
                    bands.long_band(b),
                    base - shift * i32::from(sf.long[b] + pre),
                );
            }
            13
        }
    };
    for b in (short_from..13).take_while(|&b| 3 * usize::from(bands.short[b]) < lines) {
        let band = bands.short_band(b);
        let start = 3 * band.start;
        let width = band.len();
        for w in 0..3 {
            let quarters =
                base - 8 * i32::from(g.subblock_gain[w]) - shift * i32::from(sf.short[b][w]);
            let first = start + w * width;
            scale(first..first + width, quarters);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_powers_of_two() {
        for q in -400..60 {
            let exact = 2f64.powf(f64::from(q) / 4.0);
            assert!(
                (f64::from(pow2_quarters(q)) / exact - 1.0).abs() < 1e-7,
                "{q}"
            );
        }
    }
}
