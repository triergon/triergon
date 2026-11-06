//! Inverse transforms (clause 8.6.4) and residual reconstruction (8.6.2, 8.6.7).
//!
//! Both 1-D passes are even/odd butterflies applied to whole rows (lanes = columns), so the
//! inner loops run over contiguous lanes and auto-vectorize; the first pass output is
//! transposed so the second pass has the same shape. Integer arithmetic makes this
//! reassociation bit-exact with the matrix products of equations 8-315 / 8-317.

use crate::picture::{Pixel, Plane};
use crate::tables::{DST_MATRIX, TRANS_MATRIX};

const fn widen() -> [[i32; 32]; 32] {
    let mut m = [[0i32; 32]; 32];
    let mut k = 0;
    while k < 32 {
        let mut n = 0;
        while n < 32 {
            m[k][n] = TRANS_MATRIX[k][n] as i32;
            n += 1;
        }
        k += 1;
    }
    m
}
static M32: [[i32; 32]; 32] = widen();

#[inline(always)]
fn clip16(v: i32) -> i32 {
    v.clamp(-32768, 32767)
}

// ---------------------------------------------------------------- butterfly column passes
//
// A "column pass" computes out[y][x] = sum_k M_N[k][y] * inp[k][x] for x in 0..L, using the
// even/odd symmetry M_N[k][N-1-y] = (-1)^k M_N[k][y] and M_N[2k][y] = M_{N/2}[k][y].
// `inp` row k starts at k * stride; `out` holds N rows of L lanes. Inputs with k >= kmax
// are zero and skipped. The even half is computed in place in out[0..N/2] and then
// combined, so no temporary needs initialization.

#[inline(always)]
fn lane_row<const L: usize>(s: &[i32], off: usize) -> &[i32; L] {
    s[off..off + L].try_into().unwrap()
}

#[inline(always)]
fn pass4<const L: usize>(inp: &[i32], stride: usize, kmax: usize, out: &mut [i32]) {
    let z = [0i32; L];
    let i0: &[i32; L] = lane_row(inp, 0);
    let i1: &[i32; L] = if kmax > 1 { lane_row(inp, stride) } else { &z };
    let i2: &[i32; L] = if kmax > 2 {
        lane_row(inp, 2 * stride)
    } else {
        &z
    };
    let i3: &[i32; L] = if kmax > 3 {
        lane_row(inp, 3 * stride)
    } else {
        &z
    };
    let (o01, o23) = out[..4 * L].split_at_mut(2 * L);
    let (r0, r1) = o01.split_at_mut(L);
    let (r2, r3) = o23.split_at_mut(L);
    for x in 0..L {
        let e0 = 64 * (i0[x] + i2[x]);
        let e1 = 64 * (i0[x] - i2[x]);
        let p0 = 83 * i1[x] + 36 * i3[x];
        let p1 = 36 * i1[x] - 83 * i3[x];
        r0[x] = e0 + p0;
        r1[x] = e1 + p1;
        r2[x] = e1 - p1;
        r3[x] = e0 - p0;
    }
}

macro_rules! pass_n {
    ($name:ident, $n:expr, $half:ident, $mstep:expr) => {
        #[inline(always)]
        fn $name<const L: usize>(inp: &[i32], stride: usize, kmax: usize, out: &mut [i32]) {
            const N: usize = $n;
            const H: usize = N / 2;
            $half::<L>(inp, 2 * stride, kmax.div_ceil(2), out);
            let kend = kmax.min(N);
            let (lo, hi) = out[..N * L].split_at_mut(H * L);
            for i in 0..H {
                let mut o = [0i32; L];
                let mut k = 1;
                while k < kend {
                    let m = M32[$mstep * k][i];
                    let r: &[i32; L] = lane_row(inp, k * stride);
                    for x in 0..L {
                        o[x] += m * r[x];
                    }
                    k += 2;
                }
                let e: &mut [i32; L] = (&mut lo[i * L..i * L + L]).try_into().unwrap();
                let d: &mut [i32; L] = (&mut hi[(H - 1 - i) * L..(H - 1 - i) * L + L])
                    .try_into()
                    .unwrap();
                for x in 0..L {
                    let ev = e[x];
                    e[x] = ev + o[x];
                    d[x] = ev - o[x];
                }
            }
        }
    };
}
pass_n!(pass8, 8, pass4, 4);
pass_n!(pass16, 16, pass8, 2);
pass_n!(pass32, 32, pass16, 1);

/// Reusable transform temporaries (no per-block initialization).
#[derive(Debug)]
pub struct TransformScratch {
    gt: Vec<i32>,
    out: Vec<i32>,
}

impl TransformScratch {
    pub fn new() -> Self {
        TransformScratch {
            gt: vec![0; 32 * 32],
            out: vec![0; 32 * 32],
        }
    }
}

impl Default for TransformScratch {
    fn default() -> Self {
        Self::new()
    }
}

macro_rules! idct_n {
    ($name:ident, $n:expr, $pass:ident, [$($lim:expr),*]) => {
        #[inline(never)]
        fn $name(coef: &[i32], max_x: usize, max_y: usize, bd_shift: u32, res: &mut [i32], ts: &mut TransformScratch) {
            const N: usize = $n;
            let TransformScratch { gt, out } = ts;
            // Stage 1 on L lanes (columns 0..L hold all non-zero coefficients); writes the
            // clipped (8-314) result transposed: gt[x * N + y] = g[y][x].
            $(
                if max_x < $lim {
                    const L: usize = $lim;
                    $pass::<L>(coef, N, max_y + 1, out);
                    for y in 0..N {
                        let r: &[i32; L] = lane_row(out, y * L);
                        for x in 0..L {
                            gt[x * N + y] = clip16((r[x] + 64) >> 7);
                        }
                    }
                } else
            )*
            {
                unreachable!()
            }
            // Stage 2: rows of g are rows of gt; lanes run over y. out[x * N + y] = r[y][x].
            $pass::<N>(gt, N, max_x + 1, out);
            let rnd = 1 << (bd_shift - 1);
            for x in 0..N {
                let r: &[i32; N] = lane_row(out, x * N);
                for y in 0..N {
                    res[y * N + x] = (r[y] + rnd) >> bd_shift;
                }
            }
        }
    };
}
idct_n!(idct4, 4, pass4, [4]);
idct_n!(idct8, 8, pass8, [4, 8]);
idct_n!(idct16, 16, pass16, [4, 8, 16]);
idct_n!(idct32, 32, pass32, [4, 8, 16, 32]);

fn idst4(coef: &[i32], bd_shift: u32, res: &mut [i32]) {
    let mut g = [[0i32; 4]; 4];
    for y in 0..4 {
        for x in 0..4 {
            let mut acc = 0;
            for k in 0..4 {
                acc += DST_MATRIX[k][y] * coef[k * 4 + x];
            }
            g[y][x] = clip16((acc + 64) >> 7);
        }
    }
    let rnd = 1 << (bd_shift - 1);
    for y in 0..4 {
        for x in 0..4 {
            let mut acc = 0;
            for k in 0..4 {
                acc += g[y][k] * DST_MATRIX[k][x];
            }
            res[y * 4 + x] = (acc + rnd) >> bd_shift;
        }
    }
}

/// Compute the residual for a transform block into `res` (row-major, stride n).
/// `coef` holds scaled coefficients d (clause 8.6.3 output).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn inverse_transform(
    coef: &[i32],
    log2: u32,
    max_x: usize,
    max_y: usize,
    dst: bool,
    bit_depth: u32,
    res: &mut [i32],
    ts: &mut TransformScratch,
) {
    let bd_shift = 20 - bit_depth;
    if dst {
        idst4(coef, bd_shift, res);
        return;
    }
    match log2 {
        2 => idct4(coef, max_x, max_y, bd_shift, res, ts),
        3 => idct8(coef, max_x, max_y, bd_shift, res, ts),
        4 => idct16(coef, max_x, max_y, bd_shift, res, ts),
        _ => idct32(coef, max_x, max_y, bd_shift, res, ts),
    }
}

/// DC-only inverse DCT value (all residual samples equal).
#[inline]
pub fn dc_only_value(dc: i32, bit_depth: u32) -> i32 {
    let bd_shift = 20 - bit_depth;
    let g = clip16((64 * dc + 64) >> 7);
    (64 * g + (1 << (bd_shift - 1))) >> bd_shift
}

/// Transform skip residual (8-298, 8-299) for nTbS = 4: r = (d << 7 + rnd) >> bdShift.
#[inline(never)]
pub fn transform_skip(coef: &[i32], log2: u32, bit_depth: u32, res: &mut [i32]) {
    let n = 1usize << log2;
    let ts_shift = 5 + log2;
    let bd_shift = 20 - bit_depth;
    let rnd = 1 << (bd_shift - 1);
    for i in 0..n * n {
        res[i] = ((coef[i] << ts_shift) + rnd) >> bd_shift;
    }
}

/// Add a residual block to the prediction already stored in the plane (8.6.7), clipping.
#[inline]
pub fn add_residual<P: Pixel>(
    plane: &mut Plane<P>,
    x0: usize,
    y0: usize,
    n: usize,
    res: &[i32],
    max: i32,
) {
    let stride = plane.stride;
    for y in 0..n {
        let row = &mut plane.data[(y0 + y) * stride + x0..(y0 + y) * stride + x0 + n];
        let r = &res[y * n..y * n + n];
        for x in 0..n {
            let v = row[x].to_i32() + r[x];
            row[x] = P::from_i32(v.max(0).min(max));
        }
    }
}

#[inline]
pub fn add_constant<P: Pixel>(
    plane: &mut Plane<P>,
    x0: usize,
    y0: usize,
    n: usize,
    c: i32,
    max: i32,
) {
    let stride = plane.stride;
    for y in 0..n {
        let row = &mut plane.data[(y0 + y) * stride + x0..(y0 + y) * stride + x0 + n];
        for v in row.iter_mut() {
            *v = P::from_i32((v.to_i32() + c).max(0).min(max));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(coef: &[i32], n: usize, bd: u32) -> Vec<i32> {
        let s = 32 / n;
        let mut g = vec![0i32; n * n];
        for x in 0..n {
            for y in 0..n {
                let mut e: i64 = 0;
                for k in 0..n {
                    e += TRANS_MATRIX[k * s][y] as i64 * coef[k * n + x] as i64;
                }
                g[y * n + x] = ((e + 64) >> 7).clamp(-32768, 32767) as i32;
            }
        }
        let shift = 20 - bd;
        let mut r = vec![0i32; n * n];
        for y in 0..n {
            for x in 0..n {
                let mut e: i64 = 0;
                for k in 0..n {
                    e += TRANS_MATRIX[k * s][x] as i64 * g[y * n + k] as i64;
                }
                r[y * n + x] = ((e + (1 << (shift - 1))) >> shift) as i32;
            }
        }
        r
    }

    #[test]
    fn idct_matches_matrix_product() {
        let mut seed = 12345u32;
        for log2 in 2..=5u32 {
            let n = 1usize << log2;
            for _ in 0..20 {
                let mut coef = vec![0i32; n * n];
                for c in &mut coef {
                    seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                    *c = ((seed >> 16) as i32 % 2001) - 1000;
                }
                let mut res = vec![0i32; n * n];
                inverse_transform(
                    &coef,
                    log2,
                    n - 1,
                    n - 1,
                    false,
                    8,
                    &mut res,
                    &mut TransformScratch::new(),
                );
                assert_eq!(res, reference(&coef, n, 8));
            }
        }
    }

    #[test]
    fn idct_sparse_extents_match() {
        let mut seed = 777u32;
        for log2 in 2..=5u32 {
            let n = 1usize << log2;
            for t in 0..64 {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                let mx = (seed >> 8) as usize % n;
                let my = (seed >> 20) as usize % n;
                let mut coef = vec![0i32; n * n];
                for y in 0..=my {
                    for x in 0..=mx {
                        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                        coef[y * n + x] = ((seed >> 16) as i32 % 4001) - 2000;
                    }
                }
                let bd = if t % 2 == 0 { 8 } else { 10 };
                let mut res = vec![0i32; n * n];
                inverse_transform(
                    &coef,
                    log2,
                    mx,
                    my,
                    false,
                    bd,
                    &mut res,
                    &mut TransformScratch::new(),
                );
                assert_eq!(res, reference(&coef, n, bd), "log2 {log2} mx {mx} my {my}");
            }
        }
    }

    #[test]
    fn dc_only_matches() {
        for dc in [-500, -1, 0, 1, 64, 1000] {
            let mut coef = vec![0i32; 64];
            coef[0] = dc;
            let r = reference(&coef, 8, 8);
            assert!(r.iter().all(|&v| v == dc_only_value(dc, 8)));
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Development micro-benchmark: cargo test --release -- --ignored --nocapture bench_idct
    #[test]
    #[ignore = "timing probe; run on demand with --release"]
    fn bench_idct() {
        let mut seed = 1u32;
        for log2 in 2..=5u32 {
            let n = 1usize << log2;
            let mut coef = vec![0i32; n * n];
            for c in &mut coef {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                *c = ((seed >> 16) as i32 % 201) - 100;
            }
            let mut res = vec![0i32; n * n];
            let mut ts = TransformScratch::new();
            let iters = 2_000_000 / (n * n);
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = std::time::Instant::now();
                for i in 0..iters {
                    coef[0] = i as i32 & 63;
                    inverse_transform(&coef, log2, n - 1, n - 1, false, 8, &mut res, &mut ts);
                    std::hint::black_box(&res);
                }
                best = best.min(t.elapsed().as_secs_f64() / iters as f64);
            }
            eprintln!("idct{n}x{n} full: {:.1} ns", best * 1e9);
            // Sparse: only the top-left 4x4 non-zero.
            let mut sparse = vec![0i32; n * n];
            for y in 0..4.min(n) {
                for x in 0..4.min(n) {
                    sparse[y * n + x] = coef[y * n + x];
                }
            }
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = std::time::Instant::now();
                for i in 0..iters {
                    sparse[0] = i as i32 & 63;
                    inverse_transform(
                        &sparse,
                        log2,
                        3.min(n - 1),
                        3.min(n - 1),
                        false,
                        8,
                        &mut res,
                        &mut ts,
                    );
                    std::hint::black_box(&res);
                }
                best = best.min(t.elapsed().as_secs_f64() / iters as f64);
            }
            eprintln!("idct{n}x{n} 4x4-sparse: {:.1} ns", best * 1e9);
        }
    }
}
