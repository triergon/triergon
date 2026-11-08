//! Pyramid vector quantisation (RFC 6716 section 4.3.4): codeword indexing, the spreading
//! rotation and normalisation.

use crate::range::RangeDecoder;

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

/// The codebook sizes of RFC 6716 section 4.3.4.2: U(n, k) counts the vectors of `n`
/// dimensions with at most `k - 1` pulses whose first entry is positive, so that
/// V(n, k) = U(n, k) + U(n, k + 1) codewords exist. U is symmetric and obeys
/// U(n, k) = U(n - 1, k) + U(n, k - 1) + U(n - 1, k - 1), with U(0, 0) = 1 and
/// U(n, 0) = U(0, k) = 0 otherwise.
///
/// Every codebook the pulse cache allows has V(n, k) < 2^32 and min(n, k + 1) <= 13 for
/// n <= 176, so `U_TABLE[min(n, k)][max(n, k)]` holds every value decoding needs, without
/// overflow (larger entries wrap and are never read).
const U_ROWS: usize = 14;
const U_COLUMNS: usize = MAX_BAND + 1;
static U_TABLE: [[u32; U_COLUMNS]; U_ROWS] = {
    let mut table = [[0u32; U_COLUMNS]; U_ROWS];
    table[0][0] = 1;
    let mut a = 1;
    while a < U_ROWS {
        let mut b = 1;
        while b < U_COLUMNS {
            table[a][b] = table[a - 1][b]
                .wrapping_add(table[a][b - 1])
                .wrapping_add(table[a - 1][b - 1]);
            b += 1;
        }
        a += 1;
    }
    table
};

/// U(n, k).
#[inline(always)]
fn u(n: usize, k: usize) -> u32 {
    U_TABLE[n.min(k)][n.max(k)]
}

/// The vector of codeword `index` of `k` pulses over `y.len()` dimensions: at each
/// position with `n` dimensions left, indices from U(n, k + 1) on have a negative entry
/// there, and the entry's magnitude takes the pulses down to the largest `k'` with
/// U(n, k') <= the index. O(n + k) lookups.
fn codeword(mut index: u32, mut k: usize, y: &mut [i32]) {
    y.fill(0);
    let mut n = y.len();
    let mut j = 0;
    while k > 0 {
        if n > k + 1 {
            // With more dimensions than pulses, U(n, k) and U(n, k + 1) are column n of
            // rows k and k + 1; the entry is zero while the index lies between them.
            let (row, next) = (&U_TABLE[k], &U_TABLE[k + 1]);
            while n > k + 1 && (row[n]..next[n]).contains(&index) {
                index -= row[n];
                j += 1;
                n -= 1;
            }
        }
        let split = u(n, k + 1);
        let negative = index >= split;
        if negative {
            index -= split;
        }
        let total = k;
        let mut below = u(n, k);
        while below > index {
            k -= 1;
            // With fewer pulses than dimensions the walk stays in column n.
            below = if k < n { U_TABLE[k][n] } else { U_TABLE[n][k] };
        }
        index -= below;
        let magnitude = (total - k) as i32;
        y[j] = if negative { -magnitude } else { magnitude };
        j += 1;
        n -= 1;
    }
}

/// Decode the codeword of `k` pulses over `y.len() >= 2` dimensions.
fn decode_pulses(rc: &mut RangeDecoder<'_>, y: &mut [i32], k: usize) {
    let n = y.len();
    let index = rc.uint(u(n, k).wrapping_add(u(n, k + 1)));
    codeword(index, k, y);
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
    // Exact in integers: at most 128 pulses.
    let energy = y.iter().map(|&v| v * v).sum::<i32>() as f32;
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
                let total = u(n, k) + u(n, k + 1);
                assert_eq!(
                    u64::from(total),
                    codewords(n as u64, k as u64),
                    "V({n}, {k})"
                );
                let mut seen = std::collections::HashSet::new();
                for index in 0..total {
                    let mut y = vec![0; n];
                    codeword(index, k, &mut y);
                    assert_eq!(y.iter().map(|v| v.unsigned_abs()).sum::<u32>(), k as u32);
                    assert!(seen.insert(y), "n {n} k {k} index {index}");
                }
            }
        }
    }

    #[test]
    fn the_table_covers_every_cached_codebook_exactly() {
        use super::super::alloc::pulse_count;
        use super::super::tables::{BANDS, CACHE_BITS, CACHE_INDEX, EBANDS};
        for lm in -1i32..4 {
            for band in 0..BANDS {
                let index = CACHE_INDEX[(lm + 1) as usize * BANDS + band];
                if index < 0 {
                    continue;
                }
                let width = EBANDS[band + 1] - EBANDS[band];
                let n = if lm < 0 { width >> 1 } else { width << lm };
                let k = pulse_count(usize::from(CACHE_BITS[index as usize]));
                if n < 2 {
                    continue;
                }
                assert!(n.min(k + 1) < U_ROWS && n.max(k + 1) < U_COLUMNS, "{n} {k}");
                assert!(
                    u64::from(u(n, k)) + u64::from(u(n, k + 1)) == codewords(n as u64, k as u64),
                    "V({n}, {k})"
                );
            }
        }
    }

    #[test]
    fn collapse_masks_mark_blocks_with_pulses() {
        assert_eq!(collapse_mask(&[0, 1, 0, 0, 0, 0, 0, -2], 4), 0b1001);
        assert_eq!(collapse_mask(&[0, 0], 1), 1);
    }
}
