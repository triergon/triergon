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

use triergon_pixel::Geometry;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mv {
    pub x: i32,
    pub y: i32,
}

impl Mv {
    pub(crate) const ZERO: Mv = Mv { x: 0, y: 0 };
}

/// MPEG-1 chroma vector: luma vector halved, truncating toward zero.
#[inline]
pub(crate) fn chroma(v: i32) -> i32 {
    v / 2
}

/// Reference-picture bounds for one macroblock position, in half-samples.
#[derive(Clone, Copy)]
pub(crate) struct Bounds {
    lo_x: i32,
    hi_x: i32,
    lo_y: i32,
    hi_y: i32,
}

impl Bounds {
    /// `range` caps |component| (half-samples) to what the f_code can code.
    pub(crate) fn new(g: &Geometry, mb_x: usize, mb_y: usize, range: i32) -> Bounds {
        let (w, h) = (
            (g.macroblock_width() * 16) as i32,
            (g.macroblock_height() * 16) as i32,
        );
        let (x, y) = ((mb_x * 16) as i32, (mb_y * 16) as i32);
        Bounds {
            lo_x: (-2 * x).max(-range),
            hi_x: (2 * (w - 16 - x)).min(range - 1),
            lo_y: (-2 * y).max(-range),
            hi_y: (2 * (h - 16 - y)).min(range - 1),
        }
    }

    /// Whether `mv` stays inside the coded picture for luma and chroma.
    /// Halving the luma limits and the vector preserves their order.
    #[inline]
    pub(crate) fn contains(&self, mv: Mv) -> bool {
        (self.lo_x..=self.hi_x).contains(&mv.x) && (self.lo_y..=self.hi_y).contains(&mv.y)
    }
}

/// 4×4-averaged copy of a coded luma plane (one sample per 4×4 block) into `out`
/// of `(stride / 4) * (rows / 4)` samples.
pub(crate) fn decimate4(plane: &[u8], stride: usize, rows: usize, out: &mut [u8]) {
    let (w, h) = (stride / 4, rows / 4);
    debug_assert_eq!(out.len(), w * h);
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

/// Sum of the 16 per-lane SAD accumulators. Each lane holds at most 16 rows of
/// 255, so the total, at most 65,280, is summed in 16-bit lanes.
#[inline(always)]
fn lane_sum(acc: &[u16; 16]) -> u32 {
    u32::from(acc.iter().sum::<u16>())
}

/// Adds the absolute differences of one 16-sample row to the per-lane sums.
#[inline(always)]
fn accumulate(acc: &mut [u16; 16], a: &[u8; 16], b: &[u8; 16]) {
    for i in 0..16 {
        acc[i] += u16::from(a[i].abs_diff(b[i]));
    }
}

/// The 16 samples of `plane` at `at`.
#[inline(always)]
fn row16(plane: &[u8], at: usize) -> &[u8; 16] {
    plane[at..][..16].try_into().expect("16 samples")
}

/// SAD of two 16×16 blocks.
///
/// The SADs are kept out of line: inlined into the search, the sums of their
/// lanes compile on 32-bit Arm to a lane move per lane and spills, and as
/// functions of their own to vector additions.
#[inline(never)]
pub(crate) fn sad16(cur: &[u8], cur_stride: usize, reference: &[u8], ref_stride: usize) -> u32 {
    let mut acc = [0u16; 16];
    for row in 0..16 {
        accumulate(
            &mut acc,
            row16(cur, row * cur_stride),
            row16(reference, row * ref_stride),
        );
    }
    lane_sum(&acc)
}

/// SAD of a group of four rows. Accumulate across each column before reducing
/// the 16 columns, with at most 16,320 in the 16-bit total.
#[inline(always)]
fn sad4([a0, a1, a2, a3]: &[[u8; 16]; 4], reference: &[u8], stride: usize) -> u32 {
    let [b0, b1, b2, b3] = std::array::from_fn(|r| row16(reference, r * stride));
    let total = (0..16)
        .map(|i| {
            u16::from(a0[i].abs_diff(b0[i]))
                + u16::from(a1[i].abs_diff(b1[i]))
                + u16::from(a2[i].abs_diff(b2[i]))
                + u16::from(a3[i].abs_diff(b3[i]))
        })
        .sum::<u16>();
    u32::from(total)
}

/// SAD of two 16×16 blocks, abandoned once it reaches `limit`: the result is
/// exact when below `limit` and otherwise the sum of the rows up to the first
/// group of four at which it reached `limit`.
#[inline(never)]
fn sad16_within(cur: &[[u8; 16]; 16], reference: &[u8], stride: usize, limit: u32) -> u32 {
    let mut total = 0u32;
    for (group, row) in cur.as_chunks::<4>().0.iter().zip([0, 4, 8, 12]) {
        total += sad4(group, &reference[row * stride..], stride);
        if total >= limit {
            return total;
        }
    }
    total
}

/// SAD of a 16×16 block against the half-sample interpolation of
/// `reference` (top-left full sample given) without materialising it.
#[inline]
fn sad16_half(
    cur: &[[u8; 16]; 16],
    reference: &[u8],
    stride: usize,
    half_x: bool,
    half_y: bool,
) -> u32 {
    /// One interpolation, decided once for the whole block.
    #[inline(never)]
    fn sad<const HX: bool, const HY: bool>(
        cur: &[[u8; 16]; 16],
        reference: &[u8],
        stride: usize,
    ) -> u32 {
        let mut acc = [0u16; 16];
        for (k, row) in cur.iter().enumerate() {
            let prediction = half_row::<HX, HY>(reference, k * stride, stride);
            accumulate(&mut acc, row, &prediction);
        }
        lane_sum(&acc)
    }
    match (half_x, half_y) {
        (false, false) => sad::<false, false>(cur, reference, stride),
        (true, false) => sad::<true, false>(cur, reference, stride),
        (false, true) => sad::<false, true>(cur, reference, stride),
        (true, true) => sad::<true, true>(cur, reference, stride),
    }
}

/// One 16-sample row of the half-sample prediction starting at `at`, with a
/// half-sample offset to the right (`HX`) and down (`HY`). The samples to the
/// right and below are separate overlapping 16-byte loads, which keeps every
/// operand a plain vector. Averages round half up (MPEG-1).
#[inline(always)]
fn half_row<const HX: bool, const HY: bool>(plane: &[u8], at: usize, stride: usize) -> [u8; 16] {
    let p0 = row16(plane, at);
    match (HX, HY) {
        (false, false) => *p0,
        (true, false) => {
            let p1 = row16(plane, at + 1);
            std::array::from_fn(|i| ((u16::from(p0[i]) + u16::from(p1[i]) + 1) >> 1) as u8)
        }
        (false, true) => {
            let q0 = row16(plane, at + stride);
            std::array::from_fn(|i| ((u16::from(p0[i]) + u16::from(q0[i]) + 1) >> 1) as u8)
        }
        (true, true) => {
            let p1 = row16(plane, at + 1);
            let q0 = row16(plane, at + stride);
            let q1 = row16(plane, at + stride + 1);
            std::array::from_fn(|i| {
                let s = u16::from(p0[i]) + u16::from(p1[i]) + u16::from(q0[i]) + u16::from(q1[i]);
                ((s + 2) >> 2) as u8
            })
        }
    }
}

/// Sum of squared differences of two 16×16 blocks.
#[inline]
pub(crate) fn sse16(cur: &[u8], cur_stride: usize, reference: &[u8], ref_stride: usize) -> u32 {
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
pub(crate) fn predict<const N: usize>(
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
pub(crate) fn predict_luma_blocks(
    plane: &[u8],
    stride: usize,
    x: usize,
    y: usize,
    vx: i32,
    vy: i32,
    blocks: &mut [[u8; 64]; 6],
) {
    #[inline(always)]
    fn rows<const HX: bool, const HY: bool>(
        plane: &[u8],
        base: usize,
        stride: usize,
        blocks: &mut [[u8; 64]; 6],
    ) {
        for r in 0..16 {
            let row = half_row::<HX, HY>(plane, base + r * stride, stride);
            let (top, at) = ((r >> 3) * 2, (r & 7) * 8);
            blocks[top][at..at + 8].copy_from_slice(&row[..8]);
            blocks[top + 1][at..at + 8].copy_from_slice(&row[8..]);
        }
    }
    let sx = (x as i32 + (vx >> 1)) as usize;
    let sy = (y as i32 + (vy >> 1)) as usize;
    let base = sy * stride + sx;
    match (vx & 1 != 0, vy & 1 != 0) {
        (false, false) => rows::<false, false>(plane, base, stride, blocks),
        (true, false) => rows::<true, false>(plane, base, stride, blocks),
        (false, true) => rows::<false, true>(plane, base, stride, blocks),
        (true, true) => rows::<true, true>(plane, base, stride, blocks),
    }
}

/// Bits to code one vector component difference with forward f_code `f_code`.
#[inline]
pub(crate) fn component_bits(delta: i32, f_code: u32) -> u32 {
    let r_size = f_code - 1;
    if delta == 0 {
        return 1;
    }
    let code = (((delta.abs() - 1) >> r_size) + 1).min(16) as usize;
    super::tables::MOTION_CODE[code].1 + 1 + r_size
}

/// Coarse-level inputs: decimated current and reference luma.
pub(crate) struct Coarse<'a> {
    pub cur: &'a [u8],
    pub reference: &'a [u8],
    pub width: usize,
}

/// Searches one 16×16 luma block of `cur` against `reference`.
pub(crate) struct Search<'a> {
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
    fn half_sad(&self, cur: &[[u8; 16]; 16], mv: Mv) -> u32 {
        let sx = (self.x as i32 + (mv.x >> 1)) as usize;
        let sy = (self.y as i32 + (mv.y >> 1)) as usize;
        sad16_half(
            cur,
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
    pub(crate) fn run(
        &self,
        predictors: &[Mv],
        coarse: &Coarse<'_>,
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
        // Every remaining candidate compares against these same source rows.
        let start = self.y * self.stride + self.x;
        let cur = std::array::from_fn(|row| *row16(self.cur, start + row * self.stride));
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
            let sad = sad16_within(
                &cur,
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
                let sad = self.half_sad(&cur, c);
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
            let sad = self.half_sad(&cur, c);
            if sad + self.cost_bits(c) < best_cost {
                (best, best_sad) = (c, sad);
            }
        }
        (best, best_sad)
    }

    /// The best `COARSE_KEEP` vectors of an exhaustive decimated search.
    fn coarse(&self, coarse: &Coarse<'_>, mb_x: usize, mb_y: usize) -> [Mv; COARSE_KEEP] {
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
        for dy in lo_y..=hi_y {
            let ry = (y0 as i32 + dy) as usize;
            // Sliding correlation: for every block sample, one contiguous pass
            // over all horizontal displacements of this row (vectorises over dx).
            // Clearing the whole array, of a size known when compiling, takes a
            // few vector stores rather than a call.
            let mut sads = [0u16; 2 * COARSE_RADIUS as usize + 1];
            let sads = &mut sads[..span];
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
        for (stride, rows) in [(496, 272), (32, 48), (16, 16)] {
            let mut coarse = vec![0; stride / 4 * (rows / 4)];
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
    fn packed_sad_returns_the_same_four_row_cutoffs() {
        let stride = 32;
        let plane: Vec<u8> = (0..17 * stride).map(|i| (i * 73 + i / 9) as u8).collect();
        let reference = &plane[3..];
        let cur = std::array::from_fn(|r| *row16(&plane, r * stride + 11));
        let mut prefixes = [0u32; 4];
        let mut total = 0;
        for (r, row) in cur.iter().enumerate() {
            for (a, b) in row.iter().zip(row16(reference, r * stride)) {
                total += u32::from(a.abs_diff(*b));
            }
            if r % 4 == 3 {
                prefixes[r / 4] = total;
            }
        }
        for limit in [0, prefixes[0], prefixes[0] + 1, prefixes[2] + 1, total + 1] {
            let expected = prefixes
                .into_iter()
                .find(|sum| *sum >= limit)
                .unwrap_or(total);
            assert_eq!(sad16_within(&cur, reference, stride, limit), expected);
        }
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
        let b = Bounds::new(&g, 1, 1, 16);
        assert!(b.contains(Mv { x: 15, y: -15 }));
        assert!(b.contains(Mv { x: -16, y: 15 }));
        assert!(!b.contains(Mv { x: 16, y: 15 }));
        assert!(!b.contains(Mv { x: -17, y: 15 }));
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
        let (mut cc, mut cr) = (vec![0; stride / 4 * 24], vec![0; stride / 4 * 24]);
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
