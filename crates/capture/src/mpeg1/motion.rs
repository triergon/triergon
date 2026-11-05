//! Forward motion estimation and half-sample motion compensation.
//!
//! Vectors are in half-sample luma units. Every candidate keeps the 16×16
//! luma block (plus the extra half-sample column/row) and both derived chroma
//! blocks inside the coded reference picture, so no decoder edge extension is
//! ever relied upon.
//!
//! Search: spatial/temporal predictors first; a near-perfect match ends the
//! search. Otherwise an exhaustive search of a 4×-decimated picture (±32
//! samples) adds coarse candidates, followed by a full-sample diamond and a
//! half-sample refinement, all on SAD plus a rate term.

use crate::Geometry;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mv {
    pub x: i32,
    pub y: i32,
}

impl Mv {
    pub const ZERO: Mv = Mv { x: 0, y: 0 };
}

/// MPEG-1 chroma vector: luma vector halved, truncating toward zero.
#[inline]
pub fn chroma(v: i32) -> i32 {
    v / 2
}

/// Reference-picture bounds for one macroblock position, in half-samples.
#[derive(Clone, Copy)]
pub struct Bounds {
    lo_x: i32,
    hi_x: i32,
    lo_y: i32,
    hi_y: i32,
}

impl Bounds {
    /// `range` caps |component| (half-samples) to what the f_code can code.
    pub fn new(g: &Geometry, mb_x: usize, mb_y: usize, range: i32) -> Bounds {
        let (w, h) = ((g.mb_width * 16) as i32, (g.mb_height * 16) as i32);
        let (x, y) = ((mb_x * 16) as i32, (mb_y * 16) as i32);
        Bounds {
            lo_x: (-2 * x).max(-range),
            hi_x: (2 * (w - 16 - x)).min(range - 1),
            lo_y: (-2 * y).max(-range),
            hi_y: (2 * (h - 16 - y)).min(range - 1),
        }
    }

    /// Whether `mv` stays inside the coded picture for luma and chroma.
    #[inline]
    pub fn contains(&self, mv: Mv) -> bool {
        if mv.x < self.lo_x || mv.x > self.hi_x || mv.y < self.lo_y || mv.y > self.hi_y {
            return false;
        }
        // Chroma: the halved vector may round outward by one half-sample.
        let (cx, cy) = (chroma(mv.x), chroma(mv.y));
        cx >= self.lo_x / 2 && cx <= self.hi_x / 2 && cy >= self.lo_y / 2 && cy <= self.hi_y / 2
    }
}

/// 4×4-averaged copy of a coded luma plane (one sample per 4×4 block).
pub fn decimate4(plane: &[u8], stride: usize, rows: usize, out: &mut Vec<u8>) {
    let (w, h) = (stride / 4, rows / 4);
    out.resize(w * h, 0);
    for (src, dst) in plane[..h * 4 * stride]
        .chunks_exact(4 * stride)
        .zip(out.chunks_exact_mut(w))
    {
        let [a, b, c, d] =
            std::array::from_fn(|r| src[r * stride..(r + 1) * stride].chunks_exact(4));
        for (o, (((a, b), c), d)) in dst.iter_mut().zip(a.zip(b).zip(c).zip(d)) {
            // Two 16-bit lanes, each at most 2,040 after the sum. Keeping
            // the 4×4 reduction in registers avoids the per-plane scratch row.
            let lanes = byte_pairs(a) + byte_pairs(b) + byte_pairs(c) + byte_pairs(d);
            *o = (((lanes & 0xffff) + (lanes >> 16) + 8) >> 4) as u8;
        }
    }
}

#[inline]
fn byte_pairs(bytes: &[u8]) -> u32 {
    let word = u32::from_ne_bytes(bytes.try_into().expect("four luma samples"));
    const LOW_BYTES: u32 = 0x00ff_00ff;
    (word & LOW_BYTES) + ((word >> 8) & LOW_BYTES)
}

/// SAD of two 16×16 blocks; per-lane accumulation keeps it vectorisable.
#[inline(never)]
pub fn sad16(cur: &[u8], cur_stride: usize, reference: &[u8], ref_stride: usize) -> u32 {
    let mut acc = [0u16; 16];
    for row in 0..16 {
        let a: &[u8; 16] = cur[row * cur_stride..row * cur_stride + 16]
            .try_into()
            .unwrap();
        let b: &[u8; 16] = reference[row * ref_stride..row * ref_stride + 16]
            .try_into()
            .unwrap();
        for i in 0..16 {
            acc[i] += a[i].abs_diff(b[i]) as u16;
        }
    }
    acc.iter().map(|&v| v as u32).sum()
}

/// SAD of two 16×16 blocks, abandoned once it reaches `limit`: the result is
/// exact when below `limit` and otherwise only guaranteed to be >= `limit`.
#[inline]
pub fn sad16_within(
    cur: &[u8],
    cur_stride: usize,
    reference: &[u8],
    ref_stride: usize,
    limit: u32,
) -> u32 {
    let mut total = 0u32;
    for quarter in 0..4 {
        let mut acc = [0u16; 16];
        for row in quarter * 4..quarter * 4 + 4 {
            let a: &[u8; 16] = cur[row * cur_stride..row * cur_stride + 16]
                .try_into()
                .unwrap();
            let b: &[u8; 16] = reference[row * ref_stride..row * ref_stride + 16]
                .try_into()
                .unwrap();
            for i in 0..16 {
                acc[i] += a[i].abs_diff(b[i]) as u16;
            }
        }
        total += acc.iter().map(|&v| v as u32).sum::<u32>();
        if total >= limit {
            return total;
        }
    }
    total
}

/// SAD of a 16×16 block against the half-sample interpolation of
/// `reference` (top-left full sample given) without materialising it. The
/// sample-to-the-right rows are separate overlapping 16-byte loads, which keeps
/// every operand a plain vector.
#[inline]
pub fn sad16_half(cur: &[u8], reference: &[u8], stride: usize, half_x: bool, half_y: bool) -> u32 {
    let cur = &cur[..15 * stride + 16];
    let reference = &reference[..15 * stride + 16 + half_y as usize * stride + half_x as usize];
    let mut acc = [0u16; 16];
    for k in 0..16 {
        let a: &[u8; 16] = cur[k * stride..][..16].try_into().unwrap();
        let mut pred = [0u8; 16];
        half_row(reference, k * stride, stride, half_x, half_y, &mut pred);
        for i in 0..16 {
            acc[i] += a[i].abs_diff(pred[i]) as u16;
        }
    }
    acc.iter().map(|&v| v as u32).sum()
}

/// One 16-sample row of the half-sample prediction starting at `at`.
/// Averages round half up (MPEG-1).
#[inline(always)]
fn half_row(
    plane: &[u8],
    at: usize,
    stride: usize,
    half_x: bool,
    half_y: bool,
    out: &mut [u8; 16],
) {
    let p0: &[u8; 16] = plane[at..][..16].try_into().unwrap();
    match (half_x, half_y) {
        (false, false) => *out = *p0,
        (true, false) => {
            let p1: &[u8; 16] = plane[at + 1..][..16].try_into().unwrap();
            for i in 0..16 {
                out[i] = ((p0[i] as u16 + p1[i] as u16 + 1) >> 1) as u8;
            }
        }
        (false, true) => {
            let q0: &[u8; 16] = plane[at + stride..][..16].try_into().unwrap();
            for i in 0..16 {
                out[i] = ((p0[i] as u16 + q0[i] as u16 + 1) >> 1) as u8;
            }
        }
        (true, true) => {
            let p1: &[u8; 16] = plane[at + 1..][..16].try_into().unwrap();
            let q0: &[u8; 16] = plane[at + stride..][..16].try_into().unwrap();
            let q1: &[u8; 16] = plane[at + stride + 1..][..16].try_into().unwrap();
            for i in 0..16 {
                let s = p0[i] as u16 + p1[i] as u16 + q0[i] as u16 + q1[i] as u16;
                out[i] = ((s + 2) >> 2) as u8;
            }
        }
    }
}

/// Sum of squared differences of two 16×16 blocks.
#[inline]
pub fn sse16(cur: &[u8], cur_stride: usize, reference: &[u8], ref_stride: usize) -> u32 {
    let mut acc = [0u32; 16];
    for row in 0..16 {
        let a: &[u8; 16] = cur[row * cur_stride..row * cur_stride + 16]
            .try_into()
            .unwrap();
        let b: &[u8; 16] = reference[row * ref_stride..row * ref_stride + 16]
            .try_into()
            .unwrap();
        for i in 0..16 {
            let d = a[i].abs_diff(b[i]) as u32;
            acc[i] += d * d;
        }
    }
    acc.iter().sum()
}

/// Predict a `N`×`N` block at half-sample vector `(vx, vy)` relative to
/// sample position `(x, y)` of `plane`. Averages round half up (MPEG-1).
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn predict<const N: usize>(
    plane: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    vx: i32,
    vy: i32,
    out: &mut [u8],
) {
    let sx = (x as i32 + (vx >> 1)) as usize;
    let sy = (y as i32 + (vy >> 1)) as usize;
    let base = sy * stride + sx;
    let rows = out[..N * N].chunks_exact_mut(N).enumerate();
    match (vx & 1, vy & 1) {
        (0, 0) => {
            for (r, o) in rows {
                o.copy_from_slice(&plane[base + r * stride..base + r * stride + N]);
            }
        }
        (1, 0) => {
            for (r, o) in rows {
                let (o, a) = (
                    &mut o[..N],
                    &plane[base + r * stride..base + r * stride + N + 1],
                );
                for c in 0..N {
                    o[c] = ((a[c] as u16 + a[c + 1] as u16 + 1) >> 1) as u8;
                }
            }
        }
        (0, _) => {
            for (r, o) in rows {
                let a = &plane[base + r * stride..base + r * stride + N];
                let b = &plane[base + (r + 1) * stride..base + (r + 1) * stride + N];
                for ((o, &p), &q) in o.iter_mut().zip(a).zip(b) {
                    *o = ((p as u16 + q as u16 + 1) >> 1) as u8;
                }
            }
        }
        _ => {
            for (r, o) in rows {
                let o = &mut o[..N];
                let a = &plane[base + r * stride..base + r * stride + N + 1];
                let b = &plane[base + (r + 1) * stride..base + (r + 1) * stride + N + 1];
                for c in 0..N {
                    let s = a[c] as u16 + a[c + 1] as u16 + b[c] as u16 + b[c + 1] as u16;
                    o[c] = ((s + 2) >> 2) as u8;
                }
            }
        }
    }
}

/// Predict the 16×16 luma of a macroblock straight into its four 8×8
/// blocks (raster order Y0..Y3), with the same rounding as [`predict`].
#[inline]
pub fn predict_luma_blocks(
    plane: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    vx: i32,
    vy: i32,
    blocks: &mut [[u8; 64]; 6],
) {
    let sx = (x as i32 + (vx >> 1)) as usize;
    let sy = (y as i32 + (vy >> 1)) as usize;
    let base = sy * stride + sx;
    let (hx, hy) = (vx & 1 != 0, vy & 1 != 0);
    for r in 0..16 {
        let at = base + r * stride;
        let mut row = [0u8; 16];
        match (hx, hy) {
            (false, false) => row.copy_from_slice(&plane[at..at + 16]),
            (true, false) => {
                let a: &[u8; 17] = plane[at..at + 17].try_into().unwrap();
                for c in 0..16 {
                    row[c] = ((a[c] as u16 + a[c + 1] as u16 + 1) >> 1) as u8;
                }
            }
            (false, true) => {
                let a: &[u8; 16] = plane[at..at + 16].try_into().unwrap();
                let b: &[u8; 16] = plane[at + stride..at + stride + 16].try_into().unwrap();
                for c in 0..16 {
                    row[c] = ((a[c] as u16 + b[c] as u16 + 1) >> 1) as u8;
                }
            }
            (true, true) => {
                let a: &[u8; 17] = plane[at..at + 17].try_into().unwrap();
                let b: &[u8; 17] = plane[at + stride..at + stride + 17].try_into().unwrap();
                for c in 0..16 {
                    let s = a[c] as u16 + a[c + 1] as u16 + b[c] as u16 + b[c + 1] as u16;
                    row[c] = ((s + 2) >> 2) as u8;
                }
            }
        }
        let (top, rr) = ((r >> 3) * 2, (r & 7) * 8);
        blocks[top][rr..rr + 8].copy_from_slice(&row[..8]);
        blocks[top + 1][rr..rr + 8].copy_from_slice(&row[8..]);
    }
}

/// Bits to code one vector component difference with forward f_code `f_code`.
#[inline]
pub fn component_bits(delta: i32, f_code: u32) -> u32 {
    let r_size = f_code - 1;
    if delta == 0 {
        return 1;
    }
    let code = (((delta.abs() - 1) >> r_size) + 1).min(16) as usize;
    super::tables::MOTION_CODE[code].1 + 1 + r_size
}

/// Coarse-level inputs: decimated current and reference luma.
pub struct Coarse<'a> {
    pub cur: &'a [u8],
    pub reference: &'a [u8],
    pub width: usize,
}

/// Searches one 16×16 luma block of `cur` against `reference`.
pub struct Search<'a> {
    pub cur: &'a [u8],
    pub reference: &'a [u8],
    pub stride: usize,
    pub x: usize,
    pub y: usize,
    pub bounds: Bounds,
    pub pred: Mv,
    pub lambda: u32,
    pub f_code: u32,
}

/// A predictor this good (mean absolute error ≤ 1) ends the search.
const GOOD_ENOUGH: u32 = 256;
/// Coarse (4×-decimated) exhaustive search radius: ±32 luma samples.
const COARSE_RADIUS: i32 = 8;
/// Coarse survivors re-evaluated at full resolution.
const COARSE_KEEP: usize = 4;
const DIAGONALS: [(i32, i32); 4] = [(-1, -1), (1, -1), (-1, 1), (1, 1)];
const DIAMOND: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

impl Search<'_> {
    #[inline]
    fn cost_bits(&self, mv: Mv) -> u32 {
        if mv == Mv::ZERO {
            // Coded without a vector (skipped / no-MC macroblock).
            return 0;
        }
        self.lambda
            * (component_bits(mv.x - self.pred.x, self.f_code)
                + component_bits(mv.y - self.pred.y, self.f_code))
    }

    /// SAD of a full-sample vector (both components even).
    #[inline]
    fn full_sad(&self, mv: Mv) -> u32 {
        let base =
            (self.y as i32 + mv.y / 2) as usize * self.stride + (self.x as i32 + mv.x / 2) as usize;
        let cur = &self.cur[self.y * self.stride + self.x..];
        sad16(cur, self.stride, &self.reference[base..], self.stride)
    }

    #[inline]
    fn half_sad(&self, mv: Mv) -> u32 {
        let sx = (self.x as i32 + (mv.x >> 1)) as usize;
        let sy = (self.y as i32 + (mv.y >> 1)) as usize;
        sad16_half(
            &self.cur[self.y * self.stride + self.x..],
            &self.reference[sy * self.stride + sx..],
            self.stride,
            mv.x & 1 != 0,
            mv.y & 1 != 0,
        )
    }

    /// Returns (vector, SAD at that vector). `neighbour_sad` is the final
    /// SAD of an already searched neighbouring macroblock (u32::MAX if none):
    /// predictors that do about as well as the neighbour make the coarse
    /// search unnecessary.
    pub fn run(
        &self,
        predictors: &[Mv],
        coarse: &Coarse,
        mb_x: usize,
        mb_y: usize,
        neighbour_sad: u32,
    ) -> (Mv, u32) {
        let mut best = Mv::ZERO;
        let mut best_sad = self.full_sad(best);
        let mut best_cost = best_sad;
        if best_sad == 0 {
            return (best, 0);
        }
        // Evaluates a full-sample candidate and returns its cost (exact when
        // it beats the best, otherwise only a lower bound; u32::MAX when not
        // evaluated).
        let consider = |c: Mv, best: &mut Mv, best_sad: &mut u32, best_cost: &mut u32| -> u32 {
            let c = Mv {
                x: c.x & !1,
                y: c.y & !1,
            };
            if c == *best {
                return *best_cost;
            }
            if !self.bounds.contains(c) {
                return u32::MAX;
            }
            // Only a SAD below best_cost − bits can win, so the sum may stop
            // as soon as it reaches that bound: decisions are unchanged.
            let bits = self.cost_bits(c);
            if bits >= *best_cost {
                return bits;
            }
            let base = (self.y as i32 + c.y / 2) as usize * self.stride
                + (self.x as i32 + c.x / 2) as usize;
            let cur = &self.cur[self.y * self.stride + self.x..];
            let sad = sad16_within(
                cur,
                self.stride,
                &self.reference[base..],
                self.stride,
                *best_cost - bits,
            );
            if sad + bits < *best_cost {
                (*best, *best_sad, *best_cost) = (c, sad, sad + bits);
            }
            sad + bits
        };
        // Neighbouring predictors coincide often; evaluate each vector once.
        for (k, &c) in predictors.iter().enumerate() {
            let c = Mv {
                x: c.x & !1,
                y: c.y & !1,
            };
            if c != Mv::ZERO
                && !predictors[..k].iter().any(|p| {
                    Mv {
                        x: p.x & !1,
                        y: p.y & !1,
                    } == c
                })
            {
                consider(c, &mut best, &mut best_sad, &mut best_cost);
            }
        }
        if best_sad > GOOD_ENOUGH {
            // Predictors clearly worse than the neighbour's final match mean
            // prediction failed here: search wide.
            let wide = neighbour_sad == u32::MAX || best_sad > neighbour_sad + neighbour_sad / 4;
            if wide {
                for c in self.coarse(coarse, mb_x, mb_y) {
                    consider(c, &mut best, &mut best_sad, &mut best_cost);
                }
            }
            // Two-sample then one-sample diamonds. Where the one-sample
            // diamond settles, diagonals are tried too (error surfaces of real
            // content are not separable): all four after a wide search, else
            // only the one between the better horizontal and vertical
            // neighbour. A win restarts the diamond.
            for _ in 0..8 {
                let center = best;
                for (dx, dy) in DIAMOND {
                    consider(
                        Mv {
                            x: center.x + 4 * dx,
                            y: center.y + 4 * dy,
                        },
                        &mut best,
                        &mut best_sad,
                        &mut best_cost,
                    );
                }
                if best == center {
                    break;
                }
            }
            for _ in 0..4 {
                let mut axis = [u32::MAX; 4];
                for _ in 0..8 {
                    let center = best;
                    for (k, (dx, dy)) in DIAMOND.into_iter().enumerate() {
                        axis[k] = consider(
                            Mv {
                                x: center.x + 2 * dx,
                                y: center.y + 2 * dy,
                            },
                            &mut best,
                            &mut best_sad,
                            &mut best_cost,
                        );
                    }
                    if best == center {
                        break;
                    }
                }
                let center = best;
                let directed = [(
                    if axis[0] <= axis[1] { 1 } else { -1 },
                    if axis[2] <= axis[3] { 1 } else { -1 },
                )];
                let diagonals: &[(i32, i32)] = if wide { &DIAGONALS } else { &directed };
                for &(dx, dy) in diagonals {
                    consider(
                        Mv {
                            x: center.x + 2 * dx,
                            y: center.y + 2 * dy,
                        },
                        &mut best,
                        &mut best_sad,
                        &mut best_cost,
                    );
                }
                if best == center {
                    break;
                }
            }
        }
        if best_sad == 0 {
            // Exact match. Any residual left, even tiny, is worth a
            // half-sample check: sharp text edges often land between samples.
            return (best, 0);
        }
        // Half-sample refinement: the four axis neighbours, then the one
        // diagonal between the better horizontal and vertical neighbour.
        let center = best;
        let mut axis = [u32::MAX; 4];
        for (k, (dx, dy)) in DIAMOND.into_iter().enumerate() {
            let c = Mv {
                x: center.x + dx,
                y: center.y + dy,
            };
            if self.bounds.contains(c) {
                let sad = self.half_sad(c);
                let cost = sad + self.cost_bits(c);
                axis[k] = cost;
                if cost < best_cost {
                    (best, best_sad, best_cost) = (c, sad, cost);
                }
            }
        }
        let dx = if axis[0] <= axis[1] { 1 } else { -1 };
        let dy = if axis[2] <= axis[3] { 1 } else { -1 };
        let c = Mv {
            x: center.x + dx,
            y: center.y + dy,
        };
        if self.bounds.contains(c) {
            let sad = self.half_sad(c);
            if sad + self.cost_bits(c) < best_cost {
                (best, best_sad) = (c, sad);
            }
        }
        (best, best_sad)
    }

    /// The best `COARSE_KEEP` vectors of an exhaustive decimated search.
    fn coarse(&self, coarse: &Coarse, mb_x: usize, mb_y: usize) -> [Mv; COARSE_KEEP] {
        let width = coarse.width;
        let (x0, y0) = (mb_x * 4, mb_y * 4);
        let mut block = [0u8; 16];
        for r in 0..4 {
            block[r * 4..r * 4 + 4]
                .copy_from_slice(&coarse.cur[(y0 + r) * width + x0..(y0 + r) * width + x0 + 4]);
        }
        // Coarse steps are 4 luma samples = 8 half-samples.
        let b = &self.bounds;
        let lo_x = (b.lo_x + 7).div_euclid(8).max(-COARSE_RADIUS);
        let hi_x = b.hi_x.div_euclid(8).min(COARSE_RADIUS);
        let lo_y = (b.lo_y + 7).div_euclid(8).max(-COARSE_RADIUS);
        let hi_y = b.hi_y.div_euclid(8).min(COARSE_RADIUS);
        let mut best = [(u32::MAX, 0i32, 0i32); COARSE_KEEP];
        let span = (hi_x - lo_x + 1).max(0) as usize;
        let mut sads = [0u16; 2 * COARSE_RADIUS as usize + 1];
        for dy in lo_y..=hi_y {
            let ry = (y0 as i32 + dy) as usize;
            // Sliding correlation: for every block sample, one contiguous pass
            // over all horizontal displacements of this row (vectorises over dx).
            let sads = &mut sads[..span];
            sads.fill(0);
            for r in 0..4 {
                let start = (ry + r) * width + (x0 as i32 + lo_x) as usize;
                let line = &coarse.reference[start..start + span + 3];
                for c in 0..4 {
                    let b = block[r * 4 + c];
                    for (acc, &v) in sads.iter_mut().zip(&line[c..c + span]) {
                        *acc += b.abs_diff(v) as u16;
                    }
                }
            }
            for (k, &sad) in sads.iter().enumerate() {
                let dx = lo_x + k as i32;
                // Prefer shorter vectors on ties.
                let score = sad as u32 * 4 + (dx.unsigned_abs() + dy.unsigned_abs());
                if score < best[COARSE_KEEP - 1].0 {
                    let mut i = COARSE_KEEP - 1;
                    while i > 0 && best[i - 1].0 > score {
                        best[i] = best[i - 1];
                        i -= 1;
                    }
                    best[i] = (score, dx, dy);
                }
            }
        }
        best.map(|(_, dx, dy)| Mv {
            x: dx * 8,
            y: dy * 8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coarse_luma_matches_scalar_average_across_rows_and_edges() {
        let mut coarse = Vec::new();
        for (stride, rows) in [(496, 272), (32, 48), (16, 16)] {
            for plane in [
                vec![255; stride * rows],
                (0..stride * rows)
                    .map(|i| (i.wrapping_mul(199) ^ (i >> 4) ^ (i / stride * 17)) as u8)
                    .collect(),
            ] {
                let mut expected = Vec::new();
                for y in (0..rows).step_by(4) {
                    for x in (0..stride).step_by(4) {
                        let mut sum = 0u32;
                        for dy in 0..4 {
                            for dx in 0..4 {
                                sum += u32::from(plane[(y + dy) * stride + x + dx]);
                            }
                        }
                        expected.push(((sum + 8) / 16) as u8);
                    }
                }
                decimate4(&plane, stride, rows, &mut coarse);
                assert_eq!(coarse, expected, "coded stride {stride}, height {rows}");
            }
        }
    }

    #[test]
    fn half_sample_prediction_rounds_up() {
        let plane = [10u8, 11, 20, 21, 30, 31, 40, 41, 50];
        let mut out = [0u8; 1];
        predict::<1>(&plane, 3, 0, 0, 1, 0, &mut out);
        assert_eq!(out[0], 11); // (10+11+1)/2
        predict::<1>(&plane, 3, 0, 0, 0, 1, &mut out);
        assert_eq!(out[0], 16); // (10+21+1)/2
        predict::<1>(&plane, 3, 0, 0, 1, 1, &mut out);
        assert_eq!(out[0], 18); // (10+11+21+30+2)/4, stride 3
    }

    #[test]
    fn bounds_keep_blocks_inside() {
        let g = Geometry::new(64, 48).unwrap();
        let b = Bounds::new(&g, 0, 0, 256);
        assert!(b.contains(Mv { x: 0, y: 0 }));
        assert!(!b.contains(Mv { x: -1, y: 0 }));
        assert!(b.contains(Mv { x: 96, y: 64 }));
        assert!(!b.contains(Mv { x: 97, y: 0 }));
        let b = Bounds::new(&g, 3, 2, 256);
        assert!(b.contains(Mv { x: -96, y: -64 }));
        assert!(!b.contains(Mv { x: 1, y: 0 }));
    }

    #[test]
    fn chroma_vector_truncates_toward_zero() {
        assert_eq!(chroma(3), 1);
        assert_eq!(chroma(-3), -1);
        assert_eq!(chroma(-1), 0);
    }

    #[test]
    fn search_finds_a_translation() {
        let g = Geometry::new(128, 96).unwrap();
        let stride = g.luma_stride();
        let texture = |x: usize, y: usize| {
            let (x, y) = (x as f64, y as f64);
            (128.0 + 60.0 * (x / 7.0).sin() + 50.0 * (y / 5.0).cos() + 12.0 * ((x + y) / 3.0).sin())
                as u8
        };
        // Current = reference shifted by (+9, -5) samples.
        let reference: Vec<u8> = (0..stride * 96)
            .map(|i| texture(i % stride, i / stride))
            .collect();
        let cur: Vec<u8> = (0..stride * 96)
            .map(|i| {
                let (x, y) = ((i % stride) as i32, (i / stride) as i32);
                texture(
                    (x + 9).clamp(0, 127) as usize,
                    (y - 5).clamp(0, 95) as usize,
                )
            })
            .collect();
        let (mut cc, mut cr) = (Vec::new(), Vec::new());
        decimate4(&cur, stride, 96, &mut cc);
        decimate4(&reference, stride, 96, &mut cr);
        let coarse = Coarse {
            cur: &cc,
            reference: &cr,
            width: stride / 4,
        };
        let s = Search {
            cur: &cur,
            reference: &reference,
            stride,
            x: 48,
            y: 32,
            bounds: Bounds::new(&g, 3, 2, 128),
            pred: Mv::ZERO,
            lambda: 2,
            f_code: 3,
        };
        let (mv, sad) = s.run(&[], &coarse, 3, 2, u32::MAX);
        assert_eq!((mv, sad), (Mv { x: 18, y: -10 }, 0));
    }
}
