//! Block quantisation, coefficient coding and the normative MPEG-1
//! reconstruction (ISO/IEC 11172-2 §2.4.4.1–2.4.4.2: dequantisation,
//! oddification mismatch control and saturation to [-2048, 2047]).

use super::bits::BitWriter;
use super::tables::{
    COEFFICIENTS, DC_SIZE_CHROMA, DC_SIZE_LUMA, DCT_ESCAPE, END_OF_BLOCK, INTRA_MATRIX, SCAN_INDEX,
    ZIGZAG,
};

const SHIFT: u32 = 20;

/// Per-quantiser reciprocal tables, built once per encoder.
pub struct Quantizer {
    /// Intra AC: round(8·2^20 / (q·W)) per raster position.
    intra: [[u32; 64]; 32],
    /// Non-intra: round(2^20 / (2·q)).
    inter: [u32; 32],
}

/// The original +3/8 scalar choice fixes the AC support and coded-bit budget.
/// A nonzero level may move to a neighbour only at the same actual intra
/// run/level code length and with smaller decoder-coefficient error. This is
/// not a picture-quality guarantee; integer reconstruction and the complete
/// encode session remain subject to independent decode and all R2 checks.
/// Non-intra truncation and its dead zone are unchanged.
const INTRA_BIAS: u32 = (3 << SHIFT) / 8;

impl Quantizer {
    pub fn new() -> Box<Quantizer> {
        let mut q = Box::new(Quantizer {
            intra: [[0; 64]; 32],
            inter: [0; 32],
        });
        for scale in 1..32u32 {
            for (i, &w) in INTRA_MATRIX.iter().enumerate() {
                let d = scale * w as u32;
                q.intra[scale as usize][i] = ((8u64 << SHIFT) + d as u64 / 2) as u32 / d;
            }
            q.inter[scale as usize] = ((1u32 << SHIFT) + scale) / (2 * scale);
        }
        q
    }

    /// Quantise intra AC coefficients (raster order) into `levels`; DC is
    /// handled separately. Returns the raster mask of non-zero AC levels.
    #[inline]
    pub fn intra(&self, coeffs: &[i32; 64], scale: u32, levels: &mut [i32; 64]) -> u64 {
        let table = &self.intra[scale as usize];
        for i in 0..64 {
            let c = coeffs[i];
            let m = ((c.unsigned_abs() * table[i] + INTRA_BIAS) >> SHIFT).min(255) as i32;
            levels[i] = if c < 0 { -m } else { m };
        }
        let mask = nonzero(levels) & !1;
        let mut scan = 0u64;
        for position in bits(mask) {
            scan |= 1 << SCAN_INDEX[position];
        }
        let mut next = 1;
        for index in bits(scan) {
            let run = index - next;
            next = index + 1;
            let position = ZIGZAG[index];
            levels[position] = refine_intra_same_bits(
                coeffs[position],
                levels[position],
                scale,
                INTRA_MATRIX[position],
                run,
            );
        }
        mask
    }

    /// Skip only when the actual fixed-DCT bound and this quantizer's rounded
    /// reciprocal prove that every one of its64 output levels is zero.
    #[inline]
    pub fn inter_is_zero(&self, sum_abs: u32, scale: u32) -> bool {
        u64::from(super::dct::magnitude_bound(sum_abs)) * u64::from(self.inter[scale as usize])
            < (1u64 << SHIFT)
    }

    /// Quantise non-intra coefficients (raster order). Returns the raster
    /// mask of non-zero levels.
    #[inline]
    pub fn inter(&self, coeffs: &[i32; 64], scale: u32, levels: &mut [i32; 64]) -> u64 {
        let r = self.inter[scale as usize];
        for i in 0..64 {
            let c = coeffs[i];
            let m = ((c.unsigned_abs() * r) >> SHIFT).min(255) as i32;
            levels[i] = if c < 0 { -m } else { m };
        }
        nonzero(levels)
    }
}

/// Length of the actual intra dct_coeff_next code, including sign or escape.
/// Callers pass a nonzero level, so no zero-run or EOB is introduced here.
#[inline]
fn intra_level_bits(run: usize, magnitude: u32) -> u32 {
    if let Some((_, length)) = COEFFICIENTS.lookup(run, magnitude) {
        length
    } else {
        DCT_ESCAPE.1 + 6 + if magnitude < 128 { 8 } else { 16 }
    }
}

#[inline]
fn refine_intra_same_bits(
    coefficient: i32,
    original: i32,
    scale: u32,
    weight: u16,
    run: usize,
) -> i32 {
    let reconstruct =
        |level: i32| oddify_saturate((2 * level * scale as i32 * i32::from(weight)) / 16);
    let magnitude = original.unsigned_abs();
    let length = intra_level_bits(run, magnitude);
    let mut best = original;
    let mut error = (i64::from(coefficient) - i64::from(reconstruct(best))).unsigned_abs();
    if error == 0 {
        return best;
    }
    for neighbour in [magnitude.saturating_sub(1), (magnitude + 1).min(255)] {
        if neighbour == 0 || neighbour == magnitude || intra_level_bits(run, neighbour) != length {
            continue;
        }
        let level = neighbour as i32 * original.signum();
        let candidate_error =
            (i64::from(coefficient) - i64::from(reconstruct(level))).unsigned_abs();
        if candidate_error < error {
            best = level;
            error = candidate_error;
        }
    }
    best
}

/// Raster bit mask of the non-zero entries.
#[inline]
fn nonzero(levels: &[i32; 64]) -> u64 {
    // Most quantised blocks are empty: an OR-reduction answers that cheaply.
    if levels.iter().fold(0, |a, &v| a | v) == 0 {
        return 0;
    }
    let mut mask = 0u64;
    for (i, chunk) in levels.chunks_exact(8).enumerate() {
        // Eight 0/1 bytes gathered into one byte by a single multiply.
        let mut flags = [0u8; 8];
        for (f, &v) in flags.iter_mut().zip(chunk) {
            *f = (v != 0) as u8;
        }
        let byte = u64::from_le_bytes(flags).wrapping_mul(0x0102_0408_1020_4080) >> 56;
        mask |= byte << (8 * i);
    }
    mask
}

/// Iterate the set bits of `mask`, lowest first.
#[inline]
fn bits(mut mask: u64) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        (mask != 0).then(|| {
            let i = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            i
        })
    })
}

#[inline]
fn oddify_saturate(v: i32) -> i32 {
    let v = if v & 1 == 0 && v != 0 {
        v - v.signum()
    } else {
        v
    };
    v.clamp(-2048, 2047)
}

/// Reconstruct intra coefficients: DC from its 8-bit quantised value, AC by
/// (2·QF·q·W)/16 truncated toward zero, oddified and saturated.
#[inline]
pub fn dequant_intra(levels: &[i32; 64], mask: u64, dc: i32, scale: u32, out: &mut [i32; 64]) {
    out.fill(0);
    out[0] = dc * 8;
    for i in bits(mask) {
        out[i] = oddify_saturate((2 * levels[i] * scale as i32 * INTRA_MATRIX[i] as i32) / 16);
    }
}

/// Reconstruct non-intra coefficients: ((2·QF + sign)·q·16)/16, oddified and
/// saturated.
#[inline]
pub fn dequant_inter(levels: &[i32; 64], mask: u64, scale: u32, out: &mut [i32; 64]) {
    out.fill(0);
    for i in bits(mask) {
        let l = levels[i];
        out[i] = oddify_saturate((2 * l + l.signum()) * scale as i32);
    }
}

/// dct_dc_size and differential bits for an intra DC difference.
#[inline]
pub fn write_dc(w: &mut BitWriter, diff: i32, luma: bool) {
    let size = 32 - diff.unsigned_abs().leading_zeros();
    let (code, len) = if luma {
        DC_SIZE_LUMA[size as usize]
    } else {
        DC_SIZE_CHROMA[size as usize]
    };
    w.put(code, len);
    if size > 0 {
        let bits = if diff > 0 {
            diff
        } else {
            diff + (1 << size) - 1
        };
        w.put(bits as u32, size);
    }
}

/// Write the run/level pairs of the levels selected by the raster `mask` in
/// zig-zag order, followed by end_of_block. Intra blocks start after DC;
/// non-intra blocks code a leading run-0 level-±1 with dct_coeff_first `1s`.
#[inline]
pub fn write_coefficients(w: &mut BitWriter, levels: &[i32; 64], mask: u64, intra: bool) {
    let mut scan = 0u64;
    for p in bits(mask) {
        scan |= 1 << SCAN_INDEX[p];
    }
    let mut next = usize::from(intra);
    let mut first = !intra;
    for k in bits(scan) {
        let run = k - next;
        next = k + 1;
        let level = levels[ZIGZAG[k]];
        let sign = (level < 0) as u32;
        let magnitude = level.unsigned_abs();
        if first && run == 0 && magnitude == 1 {
            w.put(0b10 | sign, 2);
        } else if let Some((code, len)) = COEFFICIENTS.lookup(run, magnitude) {
            w.put(code | sign, len);
        } else {
            w.put(DCT_ESCAPE.0, DCT_ESCAPE.1);
            w.put(run as u32, 6);
            if magnitude < 128 {
                w.put((level & 0xFF) as u32, 8);
            } else if level > 0 {
                w.put(level as u32, 16);
            } else {
                w.put(0x8000 | (level + 256) as u32, 16);
            }
        }
        first = false;
    }
    w.put(END_OF_BLOCK.0, END_OF_BLOCK.1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_differentials() {
        let mut w = BitWriter::default();
        write_dc(&mut w, 0, true); // 100
        write_dc(&mut w, -1, true); // 00 0
        write_dc(&mut w, 3, false); // 10 11
        write_dc(&mut w, -255, true); // 1111110 00000000
        let mut out = Vec::new();
        w.take_into(&mut out);
        let bits: String = out.iter().map(|b| format!("{b:08b}")).collect();
        assert!(bits.starts_with("100000101111111100000000"));
    }

    #[test]
    fn non_intra_first_coefficient_short_form() {
        let mut levels = [0; 64];
        levels[0] = -1;
        levels[1] = 1;
        let mut w = BitWriter::default();
        write_coefficients(&mut w, &levels, nonzero(&levels), false);
        let mut out = Vec::new();
        w.take_into(&mut out);
        // 11 (first, -1) 110 (next run0 +1 = 11s) 10 (EOB)
        assert_eq!(format!("{:08b}", out[0]), "11110100");
    }

    #[test]
    fn escape_levels() {
        let mut levels = [0; 64];
        levels[ZIGZAG[1]] = -200;
        levels[ZIGZAG[2]] = 130;
        let mut w = BitWriter::default();
        write_coefficients(&mut w, &levels, nonzero(&levels), true);
        let mut out = Vec::new();
        w.take_into(&mut out);
        let bits: String = out.iter().map(|b| format!("{b:08b}")).collect();
        let want = "000001".to_owned()
            + "000000"
            + &format!("{:016b}", 0x8000 | 0x38)
            + "000001"
            + "000000"
            + &format!("{:016b}", 130)
            + "10";
        assert!(bits.starts_with(&want));
    }

    #[test]
    fn reconstruction_is_odd_and_saturated() {
        let mut levels = [0; 64];
        levels[1] = 1;
        levels[2] = -3;
        levels[63] = 255;
        let mut out = [0; 64];
        dequant_inter(&levels, nonzero(&levels), 2, &mut out);
        assert_eq!(out[1], 5); // (2+1)*2 = 6 -> 5
        assert_eq!(out[2], -13); // (-6-1)*2 = -14 -> -13
        assert_eq!(out[63], 1021); // (510+1)*2 = 1022 -> 1021
    }
    #[test]
    fn same_bits_intra_refines_run_zero_plateau() {
        // W=16, q=2: levels5/6 reconstruct19/23, both have nine-bit VLCs.
        let quant = Quantizer::new();
        for sign in [-1, 1] {
            let mut coefficients = [0; 64];
            coefficients[1] = sign * 22;
            let mut levels = [0; 64];
            assert_eq!(quant.intra(&coefficients, 2, &mut levels), 2);
            assert_eq!(levels[1], sign * 6);
        }
    }

    #[test]
    fn same_bits_intra_refines_extended_escape_plateau() {
        // Levels129/130 both use the same extended escape width.
        let quant = Quantizer::new();
        for sign in [-1, 1] {
            let mut coefficients = [0; 64];
            coefficients[1] = sign * 518;
            let mut levels = [0; 64];
            quant.intra(&coefficients, 2, &mut levels);
            assert_eq!(levels[1], sign * 130);
        }
    }

    #[test]
    fn same_bits_intra_preserves_zero_and_expensive_neighbour() {
        let quant = Quantizer::new();
        for (coefficient, expected) in [(2, 0), (-2, 0), (6, 1), (-6, -1)] {
            let mut coefficients = [0; 64];
            coefficients[1] = coefficient;
            let mut levels = [0; 64];
            let mask = quant.intra(&coefficients, 2, &mut levels);
            assert_eq!(levels[1], expected);
            assert_eq!(mask, if expected == 0 { 0 } else { 2 });
        }
    }

    #[test]
    fn same_bits_intra_keeps_escape_width_boundary() {
        let quant = Quantizer::new();
        for sign in [-1, 1] {
            let mut coefficients = [0; 64];
            coefficients[1] = sign * 510;
            let mut levels = [0; 64];
            quant.intra(&coefficients, 2, &mut levels);
            // The closer128 level would grow the escape from20 to28 bits.
            assert_eq!(levels[1], sign * 127);
        }
    }

    fn old_scalar_levels(coefficients: &[i32; 64], q: u32) -> [i32; 64] {
        std::array::from_fn(|i| {
            if q == 0 {
                return 0;
            }
            let divisor = q * u32::from(INTRA_MATRIX[i]);
            let reciprocal = ((8u64 << 20) + u64::from(divisor) / 2) / u64::from(divisor);
            let magnitude =
                ((u64::from(coefficients[i].unsigned_abs()) * reciprocal + (3u64 << 17)) >> 20)
                    .min(255) as i32;
            if coefficients[i] < 0 {
                -magnitude
            } else {
                magnitude
            }
        })
    }

    fn literal_decoder_coefficient(level: i32, q: u32, position: usize) -> i32 {
        let value = level * q as i32 * i32::from(INTRA_MATRIX[position]) / 8;
        let odd = if value != 0 && value % 2 == 0 {
            value - value.signum()
        } else {
            value
        };
        odd.clamp(-2048, 2047)
    }

    #[test]
    fn same_bits_intra_matches_real_writer_cost_and_support() {
        let quant = Quantizer::new();
        for q in 1..=31 {
            for seed in 0..16 {
                let coefficients = std::array::from_fn(|i| {
                    if (i + seed) % 5 == 0 {
                        0
                    } else {
                        ((i * 137 + seed * 293) % 4096) as i32 - 2048
                    }
                });
                let old = old_scalar_levels(&coefficients, q);
                let mut selected = [0; 64];
                let mask = quant.intra(&coefficients, q, &mut selected);
                let expected_mask = nonzero(&old) & !1;
                assert_eq!(mask, expected_mask);
                let mut before = BitWriter::default();
                let mut after = BitWriter::default();
                write_coefficients(&mut before, &old, expected_mask, true);
                write_coefficients(&mut after, &selected, mask, true);
                assert_eq!(after.bit_len(), before.bit_len(), "q={q} seed={seed}");
            }
        }
        for &position in ZIGZAG.iter().skip(1) {
            for coefficient in [-2048, -518, -510, -22, 22, 510, 518, 2047] {
                let mut coefficients = [0; 64];
                coefficients[position] = coefficient;
                let old = old_scalar_levels(&coefficients, 2);
                let mut selected = [0; 64];
                let mask = quant.intra(&coefficients, 2, &mut selected);
                let expected_mask = nonzero(&old) & !1;
                assert_eq!(mask, expected_mask);
                let mut before = BitWriter::default();
                let mut after = BitWriter::default();
                write_coefficients(&mut before, &old, expected_mask, true);
                write_coefficients(&mut after, &selected, mask, true);
                assert_eq!(after.bit_len(), before.bit_len(), "position={position}");
            }
        }
    }

    #[test]
    fn same_bits_intra_never_increases_scalar_error() {
        let quant = Quantizer::new();
        for q in 1..=31 {
            for seed in 0..16 {
                let coefficients =
                    std::array::from_fn(|i| ((i * 131 + seed * 271) % 4096) as i32 - 2048);
                let old = old_scalar_levels(&coefficients, q);
                let mut selected = [0; 64];
                quant.intra(&coefficients, q, &mut selected);
                for (i, &coefficient) in coefficients.iter().enumerate().skip(1) {
                    let target = i64::from(coefficient);
                    let old_error =
                        (target - i64::from(literal_decoder_coefficient(old[i], q, i))).abs();
                    let new_error =
                        (target - i64::from(literal_decoder_coefficient(selected[i], q, i))).abs();
                    assert!(new_error <= old_error, "q={q} seed={seed} position={i}");
                }
            }
        }
    }

    #[test]
    fn same_bits_intra_keeps_original_on_equal_error() {
        let quant = Quantizer::new();
        for sign in [-1, 1] {
            let mut coefficients = [0; 64];
            coefficients[1] = sign * 21;
            let mut levels = [0; 64];
            quant.intra(&coefficients, 2, &mut levels);
            assert_eq!(levels[1], sign * 5);
        }
    }

    #[test]
    fn same_bits_intra_dc_zero_scale_and_inter_keep_original_rule() {
        let quant = Quantizer::new();
        let coefficients = std::array::from_fn(|i| i as i32 * 59 - 1871);
        for q in 0..=31 {
            let old = old_scalar_levels(&coefficients, q);
            let mut selected = [0; 64];
            quant.intra(&coefficients, q, &mut selected);
            assert_eq!(selected[0], old[0]);
            let mut inter = [0; 64];
            quant.inter(&coefficients, q, &mut inter);
            let reciprocal = if q == 0 {
                0
            } else {
                ((1u32 << 20) + q) / (2 * q)
            };
            let expected = std::array::from_fn(|i| {
                let magnitude =
                    ((coefficients[i].unsigned_abs() * reciprocal) >> 20).min(255) as i32;
                if coefficients[i] < 0 {
                    -magnitude
                } else {
                    magnitude
                }
            });
            assert_eq!(inter, expected);
            if q == 0 {
                assert_eq!(selected, [0; 64]);
            }
        }
    }
}
