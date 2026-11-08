//! Pyramid vector quantisation (RFC 6716 section 4.3.4): codeword indexing, the spreading
//! rotation and normalisation.

use crate::range::RangeDecoder;

/// Most pulses a band codes (`pulse_count` of the largest cached index).
pub(super) const MAX_PULSES: usize = 128;
/// Widest band: 22 bins of the last band times eight short blocks.
pub(super) const MAX_BAND: usize = 176;

/// Spreading strength of the pulses (RFC 6716 section 4.3.4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Spread {
    None,
    Light,
    Normal,
    Aggressive,
}

impl Spread {
    pub(super) fn from_symbol(symbol: usize) -> Spread {
        match symbol {
            0 => Spread::None,
            1 => Spread::Light,
            2 => Spread::Normal,
            _ => Spread::Aggressive,
        }
    }

    /// The rotation's spreading factor, `None` without rotation.
    fn factor(self) -> Option<i32> {
        match self {
            Spread::None => None,
            Spread::Light => Some(15),
            Spread::Normal => Some(10),
            Spread::Aggressive => Some(5),
        }
    }
}

/// The codebook rows of RFC 6716 section 4.3.4.2: U(n, k) counts the vectors of `n`
/// dimensions with at most `k - 1` pulses whose first entry is positive, so that
/// V(n, k) = U(n, k) + U(n, k + 1) codewords exist; U obeys
/// U(n, k) = U(n - 1, k) + U(n, k - 1) + U(n - 1, k - 1).
///
/// Fill `u[j] = U(n, j)` for `j <= k + 1` (`n >= 2`) and return V(n, k).
fn codebook_row(n: usize, k: usize, u: &mut [u32]) -> u32 {
    // Row 2: U(2, 0) = 0, U(2, j) = 2j - 1.
    u[0] = 0;
    for (j, value) in u.iter_mut().enumerate().skip(1) {
        *value = 2 * j as u32 - 1;
    }
    // U(r + 1, 1) = 1 and U(r + 1, j) = U(r, j) + U(r, j - 1) + U(r + 1, j - 1).
    for _ in 2..n {
        let mut previous_old = u[1];
        u[1] = 1;
        for j in 2..u.len() {
            let old = u[j];
            u[j] = old.wrapping_add(previous_old).wrapping_add(u[j - 1]);
            previous_old = old;
        }
    }
    u[k].wrapping_add(u[k + 1])
}

/// The vector of codeword `index` given row `u` of [`codebook_row`] (destroyed).
fn codeword(mut index: u32, mut k: usize, u: &mut [u32], y: &mut [i32]) {
    for value in y.iter_mut() {
        // Indices from U(n, k + 1) on have a negative entry here.
        let negative = index >= u[k + 1];
        if negative {
            index -= u[k + 1];
        }
        let total = k;
        while u[k] > index {
            k -= 1;
        }
        index -= u[k];
        let magnitude = (total - k) as i32;
        *value = if negative { -magnitude } else { magnitude };
        // Back to row n - 1: U(n - 1, j) = U(n, j) - U(n, j - 1) - U(n - 1, j - 1).
        let mut previous_new = 0u32;
        let mut previous_old = u[0];
        u[0] = 0;
        for j in 1..k + 2 {
            let old = u[j];
            previous_new = old.wrapping_sub(previous_old).wrapping_sub(previous_new);
            u[j] = previous_new;
            previous_old = old;
        }
    }
}

/// Decode the codeword of `k` pulses over `y.len() >= 2` dimensions.
fn decode_pulses(rc: &mut RangeDecoder<'_>, y: &mut [i32], k: usize) {
    let mut row = [0u32; MAX_PULSES + 2];
    let u = &mut row[..k + 2];
    let codewords = codebook_row(y.len(), k, u);
    let index = rc.uint(codewords);
    codeword(index, k, u, y);
}

/// One pass of the spreading rotation over pairs `stride` apart, forwards then backwards.
fn rotate_pairs(x: &mut [f32], stride: usize, c: f32, s: f32) {
    let len = x.len();
    for i in 0..len - stride {
        let (x1, x2) = (x[i], x[i + stride]);
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 - s * x2;
    }
    if len > 2 * stride {
        for i in (0..len - 2 * stride).rev() {
            let (x1, x2) = (x[i], x[i + stride]);
            x[i + stride] = c * x2 + s * x1;
            x[i] = c * x1 - s * x2;
        }
    }
}

/// Undo the encoder's spreading rotation of a band of `k` pulses split into `blocks`
/// interleaved blocks (RFC 6716 section 4.3.4.3).
fn unspread(x: &mut [f32], blocks: usize, k: usize, spread: Spread) {
    let len = x.len();
    let Some(factor) = spread.factor() else {
        return;
    };
    if 2 * k >= len {
        return;
    }
    let gain = len as f32 / (len as i32 + factor * k as i32) as f32;
    let theta = 0.5 * (gain * gain);
    let quarter = 0.5 * core::f32::consts::PI;
    let c = f64::from(quarter * theta).cos() as f32;
    let s = f64::from(quarter * (1.0 - theta)).cos() as f32;
    // A second rotation with stride about sqrt(len / blocks) for long blocks.
    let mut stride2 = 0;
    if len >= 8 * blocks {
        stride2 = 1;
        while (stride2 * stride2 + stride2) * blocks + (blocks >> 2) < len {
            stride2 += 1;
        }
    }
    for block in x.chunks_exact_mut(len / blocks) {
        if stride2 > 0 {
            rotate_pairs(block, stride2, s, c);
        }
        rotate_pairs(block, 1, c, s);
    }
}

/// Which of `blocks` interleaved blocks received pulses.
fn collapse_mask(y: &[i32], blocks: usize) -> u32 {
    if blocks <= 1 {
        return 1;
    }
    y.chunks_exact(y.len() / blocks)
        .enumerate()
        .fold(0, |mask, (i, block)| {
            mask | (u32::from(block.iter().any(|&v| v != 0)) << i)
        })
}

/// Decode a band's pulses into the unit-norm vector `x` scaled by `gain` (`alg_unquant`)
/// and return its collapse mask.
pub(super) fn decode_band(
    rc: &mut RangeDecoder<'_>,
    x: &mut [f32],
    k: usize,
    spread: Spread,
    blocks: usize,
    gain: f32,
) -> u32 {
    let mut pulses = [0i32; MAX_BAND];
    let y = &mut pulses[..x.len()];
    decode_pulses(rc, y, k);
    let energy = y.iter().fold(0f32, |e, &v| e + (v as f32) * (v as f32));
    let g = 1.0 / energy.sqrt() * gain;
    for (out, &v) in x.iter_mut().zip(y.iter()) {
        *out = g * v as f32;
    }
    unspread(x, blocks, k, spread);
    collapse_mask(y, blocks)
}

/// Scale `x` to norm `gain`.
pub(super) fn renormalise(x: &mut [f32], gain: f32) {
    let energy = x.iter().fold(1e-15f32, |e, &v| e + v * v);
    let g = 1.0 / energy.sqrt() * gain;
    for v in x {
        *v *= g;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// V(n, k) by its definition: sum over the number of nonzero entries.
    fn codewords(n: u64, k: u64) -> u64 {
        fn choose(n: u64, k: u64) -> u64 {
            if k > n {
                return 0;
            }
            (0..k).fold(1, |acc, i| acc * (n - i) / (i + 1))
        }
        if k == 0 {
            return 1;
        }
        (1..=k.min(n))
            .map(|j| (1 << j) * choose(n, j) * choose(k - 1, j - 1))
            .sum()
    }

    #[test]
    fn every_index_decodes_to_a_distinct_vector_of_k_pulses() {
        for n in 2..8usize {
            for k in 1..7usize {
                let mut u = vec![0u32; k + 2];
                let total = codebook_row(n, k, &mut u);
                assert_eq!(
                    u64::from(total),
                    codewords(n as u64, k as u64),
                    "V({n}, {k})"
                );
                let mut seen = std::collections::HashSet::new();
                for index in 0..total {
                    let mut row = u.clone();
                    let mut y = vec![0; n];
                    codeword(index, k, &mut row, &mut y);
                    assert_eq!(y.iter().map(|v| v.unsigned_abs()).sum::<u32>(), k as u32);
                    assert!(seen.insert(y), "n {n} k {k} index {index}");
                }
            }
        }
        // The largest codebooks still fit 32 bits.
        let mut u = vec![0u32; MAX_PULSES + 2];
        assert_eq!(codebook_row(2, MAX_PULSES, &mut u), 4 * MAX_PULSES as u32);
    }

    #[test]
    fn collapse_masks_mark_blocks_with_pulses() {
        assert_eq!(collapse_mask(&[0, 1, 0, 0, 0, 0, 0, -2], 4), 0b1001);
        assert_eq!(collapse_mask(&[0, 0], 1), 1);
    }
}
