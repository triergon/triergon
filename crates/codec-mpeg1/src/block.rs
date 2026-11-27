//! Block quantisation, the raster masks of a block's nonzero coefficients, and the
//! normative MPEG-1 reconstruction (ISO/IEC 11172-2 §2.4.4.1–2.4.4.2:
//! dequantisation, oddification mismatch control and saturation to [-2048, 2047]).

use crate::tables::{COEFFICIENTS, DCT_ESCAPE, INTRA_MATRIX, SCAN_BIT, ZIGZAG};

const SHIFT: u32 = 20;

/// Per-quantiser reciprocal tables, built once per encoder.
#[derive(Debug)]
pub(crate) struct Quantizer {
    /// Intra AC: round(8·2^20 / (q·W)) per raster position.
    intra: [[u32; 64]; 32],
    /// Non-intra: round(2^20 / (2·q)).
    inter: [u32; 32],
}

/// Intra AC levels round at +3/8 of a step. That rounding decides which coefficients
/// are coded; a nonzero level then moves to a neighbouring level only when its
/// run/level code has the same length and it reconstructs closer to the coefficient.
/// Non-intra levels truncate, with their dead zone.
const INTRA_BIAS: u32 = (3 << SHIFT) / 8;

impl Quantizer {
    pub(crate) fn new() -> Box<Quantizer> {
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
    /// Refinement can be omitted when only the lengths of the codes are needed.
    #[inline]
    pub(crate) fn intra<const REFINE: bool>(
        &self,
        coeffs: &[i32; 64],
        scale: u32,
        levels: &mut [i32; 64],
    ) -> u64 {
        let table = &self.intra[scale as usize];
        for i in 0..64 {
            let c = coeffs[i];
            let m = ((c.unsigned_abs() * table[i] + INTRA_BIAS) >> SHIFT).min(255) as i32;
            levels[i] = if c < 0 { -m } else { m };
        }
        let mask = nonzero(levels) & !1;
        if REFINE {
            let mut scan = 0u64;
            for position in bits(mask) {
                scan |= SCAN_BIT[position];
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
        }
        mask
    }

    /// Whether every non-intra level of a residual whose absolute samples sum to
    /// `sum_abs` is zero at `scale`: the transform's bound on each coefficient
    /// ([`magnitude_bound`](crate::dct::magnitude_bound)) times this quantiser's
    /// reciprocal stays below one step.
    #[inline]
    pub(crate) fn inter_is_zero(&self, sum_abs: u32, scale: u32) -> bool {
        u64::from(crate::dct::magnitude_bound(sum_abs)) * u64::from(self.inter[scale as usize])
            < (1u64 << SHIFT)
    }

    /// Quantise non-intra coefficients (raster order). Returns the raster
    /// mask of non-zero levels.
    #[inline]
    pub(crate) fn inter(&self, coeffs: &[i32; 64], scale: u32, levels: &mut [i32; 64]) -> u64 {
        let r = self.inter[scale as usize];
        for i in 0..64 {
            let c = coeffs[i];
            let m = ((c.unsigned_abs() * r) >> SHIFT).min(255) as i32;
            levels[i] = if c < 0 { -m } else { m };
        }
        nonzero(levels)
    }
}

/// Length of the intra dct_coeff_next code of a nonzero level, including its sign or
/// escape.
#[inline]
fn intra_level_bits(run: usize, magnitude: u32) -> u32 {
    if let Some((_, length)) = COEFFICIENTS.lookup(run, magnitude) {
        length
    } else {
        DCT_ESCAPE.1 + 6 + if magnitude < 128 { 8 } else { 16 }
    }
}

/// The level nearest in reconstruction to `coefficient` among `rounded` and its two
/// neighbours whose codes are as long as its own; `rounded` on a tie.
#[inline]
fn refine_intra_same_bits(
    coefficient: i32,
    rounded: i32,
    scale: u32,
    weight: u16,
    run: usize,
) -> i32 {
    let reconstruct =
        |level: i32| oddify_saturate((2 * level * scale as i32 * i32::from(weight)) / 16);
    let magnitude = rounded.unsigned_abs();
    let length = intra_level_bits(run, magnitude);
    let mut best = rounded;
    let mut error = (i64::from(coefficient) - i64::from(reconstruct(best))).unsigned_abs();
    if error == 0 {
        return best;
    }
    for neighbour in [magnitude.saturating_sub(1), (magnitude + 1).min(255)] {
        if neighbour == 0 || neighbour == magnitude || intra_level_bits(run, neighbour) != length {
            continue;
        }
        let level = neighbour as i32 * rounded.signum();
        let candidate_error =
            (i64::from(coefficient) - i64::from(reconstruct(level))).unsigned_abs();
        if candidate_error < error {
            best = level;
            error = candidate_error;
        }
    }
    best
}

/// Raster bit mask of the non-zero entries, built as two 32-bit halves (one
/// compare, select and OR-reduction per half, all in vector lanes).
#[inline]
pub(crate) fn nonzero(levels: &[i32; 64]) -> u64 {
    let half = |levels: &[i32]| {
        levels
            .iter()
            .enumerate()
            .fold(0u32, |mask, (i, &v)| mask | (u32::from(v != 0) << i))
    };
    u64::from(half(&levels[..32])) | u64::from(half(&levels[32..])) << 32
}

/// Iterate the set bits of `mask`, lowest first.
#[inline]
pub(crate) fn bits(mut mask: u64) -> impl Iterator<Item = usize> {
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
/// (2·QF·q·W)/16 truncated toward zero, oddified and saturated. Only the DC
/// and the positions in `mask` are written; [`idct`](crate::dct::idct) reads no
/// others.
#[inline]
pub(crate) fn dequant_intra(
    levels: &[i32; 64],
    mask: u64,
    dc: i32,
    scale: u32,
    out: &mut [i32; 64],
) {
    out[0] = dc * 8;
    for i in bits(mask) {
        out[i] = oddify_saturate((2 * levels[i] * scale as i32 * INTRA_MATRIX[i] as i32) / 16);
    }
}

/// Reconstruct non-intra coefficients: ((2·QF + sign)·q·16)/16, oddified and
/// saturated. Only the positions in `mask` are written.
#[inline]
pub(crate) fn dequant_inter(levels: &[i32; 64], mask: u64, scale: u32, out: &mut [i32; 64]) {
    for i in bits(mask) {
        let l = levels[i];
        out[i] = oddify_saturate((2 * l + l.signum()) * scale as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;
    use crate::syntax::write_coefficients;

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
        // W = 16, q = 2: levels 5 and 6 reconstruct 19 and 23; both codes are nine bits.
        let quant = Quantizer::new();
        for sign in [-1, 1] {
            let mut coefficients = [0; 64];
            coefficients[1] = sign * 22;
            let mut levels = [0; 64];
            assert_eq!(quant.intra::<true>(&coefficients, 2, &mut levels), 2);
            assert_eq!(levels[1], sign * 6);
        }
    }

    #[test]
    fn same_bits_intra_refines_extended_escape_plateau() {
        // Levels 129 and 130 both take the 16-bit escape level.
        let quant = Quantizer::new();
        for sign in [-1, 1] {
            let mut coefficients = [0; 64];
            coefficients[1] = sign * 518;
            let mut levels = [0; 64];
            quant.intra::<true>(&coefficients, 2, &mut levels);
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
            let mask = quant.intra::<true>(&coefficients, 2, &mut levels);
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
            quant.intra::<true>(&coefficients, 2, &mut levels);
            // The closer level 128 would grow the escape from 20 to 28 bits.
            assert_eq!(levels[1], sign * 127);
        }
    }

    /// Intra levels at the +3/8 rounding, without the same-length refinement.
    fn rounded_levels(coefficients: &[i32; 64], q: u32) -> [i32; 64] {
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

    /// The intra coefficient a decoder reconstructs from `level`, as §2.4.4.1 writes it.
    fn decoded_coefficient(level: i32, q: u32, position: usize) -> i32 {
        let value = level * q as i32 * i32::from(INTRA_MATRIX[position]) / 8;
        let odd = if value != 0 && value % 2 == 0 {
            value - value.signum()
        } else {
            value
        };
        odd.clamp(-2048, 2047)
    }

    #[test]
    fn same_bits_intra_keeps_the_written_length_and_support() {
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
                let rounded = rounded_levels(&coefficients, q);
                let mut selected = [0; 64];
                let mask = quant.intra::<true>(&coefficients, q, &mut selected);
                let expected_mask = nonzero(&rounded) & !1;
                let mut trial = [0; 64];
                assert_eq!(
                    quant.intra::<false>(&coefficients, q, &mut trial),
                    expected_mask
                );
                assert_eq!(trial, rounded);
                assert_eq!(mask, expected_mask);
                let mut before = BitWriter::default();
                let mut after = BitWriter::default();
                write_coefficients(&mut before, &rounded, expected_mask, true);
                write_coefficients(&mut after, &selected, mask, true);
                assert_eq!(after.bit_len(), before.bit_len(), "q={q} seed={seed}");
            }
        }
        for &position in ZIGZAG.iter().skip(1) {
            for coefficient in [-2048, -518, -510, -22, 22, 510, 518, 2047] {
                let mut coefficients = [0; 64];
                coefficients[position] = coefficient;
                let rounded = rounded_levels(&coefficients, 2);
                let mut selected = [0; 64];
                let mask = quant.intra::<true>(&coefficients, 2, &mut selected);
                let expected_mask = nonzero(&rounded) & !1;
                assert_eq!(mask, expected_mask);
                let mut before = BitWriter::default();
                let mut after = BitWriter::default();
                write_coefficients(&mut before, &rounded, expected_mask, true);
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
                let rounded = rounded_levels(&coefficients, q);
                let mut selected = [0; 64];
                quant.intra::<true>(&coefficients, q, &mut selected);
                for (i, &coefficient) in coefficients.iter().enumerate().skip(1) {
                    let target = i64::from(coefficient);
                    let rounded_error =
                        (target - i64::from(decoded_coefficient(rounded[i], q, i))).abs();
                    let new_error =
                        (target - i64::from(decoded_coefficient(selected[i], q, i))).abs();
                    assert!(new_error <= rounded_error, "q={q} seed={seed} position={i}");
                }
            }
        }
    }

    #[test]
    fn same_bits_intra_keeps_the_rounded_level_on_equal_error() {
        let quant = Quantizer::new();
        for sign in [-1, 1] {
            let mut coefficients = [0; 64];
            coefficients[1] = sign * 21;
            let mut levels = [0; 64];
            quant.intra::<true>(&coefficients, 2, &mut levels);
            assert_eq!(levels[1], sign * 5);
        }
    }

    #[test]
    fn same_bits_intra_leaves_dc_zero_scale_and_inter_levels_rounded() {
        let quant = Quantizer::new();
        let coefficients = std::array::from_fn(|i| i as i32 * 59 - 1871);
        for q in 0..=31 {
            let rounded = rounded_levels(&coefficients, q);
            let mut selected = [0; 64];
            quant.intra::<true>(&coefficients, q, &mut selected);
            assert_eq!(selected[0], rounded[0]);
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
