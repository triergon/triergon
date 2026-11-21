//! `std::simd` forms of the SBR QMF banks' windowing and shuffles, of the covariances HF
//! generation predicts from, and of the Parametric Stereo hybrid filters, used with the
//! `simd` feature (nightly).
//!
//! Every lane performs the scalar code's operations in the scalar code's order, so the
//! results equal the scalar code's wherever the vector unit follows IEEE 754 (AArch64).
//! ARMv7 NEON flushes subnormal operands and results to zero, so there the results can
//! differ from the scalar code's for subnormal values.

use super::{Covariance, XL_LEN, predictor};
use crate::fft::Cpx;
use std::simd::{f32x4, simd_swizzle};

type V = f32x4;

#[inline(always)]
fn ld(s: &[f32], i: usize) -> V {
    V::from_slice(&s[i..i + 4])
}

/// Two complex values as `[re0, im0, re1, im1]`.
#[inline(always)]
fn ld2(x: &[Cpx], i: usize) -> V {
    let (a, b) = (x[i], x[i + 1]);
    V::from_array([a.re, a.im, b.re, b.im])
}

#[inline(always)]
fn rev(v: V) -> V {
    simd_swizzle!(v, [3, 2, 1, 0])
}

/// The analysis input step and window sums: writes the 32 new samples (oldest first)
/// reversed and scaled to `x[..32]`, then returns u\[n\] = sum_{j<5} x\[64j + n\]
/// w\[64j + n\], n < 64, each summed in tap order from zero.
pub(super) fn analysis_window(
    x: &mut [f32; 320],
    input: &[f32; 32],
    scale: f32,
    w: &[f32; 320],
) -> [f32; 64] {
    let s = V::splat(scale);
    for b in 0..8 {
        (rev(ld(input, 4 * b)) * s).copy_to_slice(&mut x[28 - 4 * b..32 - 4 * b]);
    }
    let mut u = [0f32; 64];
    for b in 0..16 {
        let mut acc = V::splat(0.0);
        for j in 0..5 {
            acc += ld(x, 64 * j + 4 * b) * ld(w, 64 * j + 4 * b);
        }
        acc.copy_to_slice(&mut u[4 * b..4 * b + 4]);
    }
    u
}

/// The analysis folds: d\[n\] = u\[n\] - u\[63 - n\] and r\[n\] = u\[31 - n\] + u\[32 + n\],
/// n < 32.
pub(super) fn analysis_fold(u: &[f32; 64]) -> ([f32; 32], [f32; 32]) {
    let (mut d, mut r) = ([0f32; 32], [0f32; 32]);
    for b in 0..8 {
        (ld(u, 4 * b) - rev(ld(u, 60 - 4 * b))).copy_to_slice(&mut d[4 * b..4 * b + 4]);
        (rev(ld(u, 28 - 4 * b)) + ld(u, 32 + 4 * b)).copy_to_slice(&mut r[4 * b..4 * b + 4]);
    }
    (d, r)
}

/// The synthesis inputs: a\[k\] = (-1)^k Im x\[k\] and b\[k\] = -(-1)^k Re x\[63 - k\].
pub(super) fn synthesis_inputs(x: &[Cpx; 64], a: &mut [f32; 64], b: &mut [f32; 64]) {
    let sgn = V::from_array([1.0, -1.0, 1.0, -1.0]);
    for blk in 0..16 {
        let k = 4 * blk;
        let (lo, hi) = (ld2(x, k), ld2(x, k + 2));
        let im = simd_swizzle!(lo, hi, [1, 3, 5, 7]);
        (sgn * im).copy_to_slice(&mut a[k..k + 4]);
        // x[63 - k - 3 ..= 63 - k], read newest band first
        let (lo, hi) = (ld2(x, 60 - k), ld2(x, 62 - k));
        let re = simd_swizzle!(lo, hi, [6, 4, 2, 0]);
        (-sgn * re).copy_to_slice(&mut b[k..k + 4]);
    }
}

/// The synthesis butterflies into v (128): v\[64 + 2k\] = ce + se, v\[63 - 2k\] = ce - se,
/// v\[127 - 2k\] = co - so and v\[2k\] = co + so, k < 32.
pub(super) fn synthesis_butterflies(
    ce: &[f32; 32],
    co: &[f32; 32],
    se: &[f32; 32],
    so: &[f32; 32],
    v: &mut [f32; 128],
) {
    for b in 0..8 {
        let k = 4 * b;
        let r = 28 - k;
        // v[8b..8b + 8]: even 2j from co + so at j, odd 2j + 1 from ce - se at 31 - j
        let (lo, hi) = (ld(co, k) + ld(so, k)).interleave(rev(ld(ce, r) - ld(se, r)));
        lo.copy_to_slice(&mut v[2 * k..2 * k + 4]);
        hi.copy_to_slice(&mut v[2 * k + 4..2 * k + 8]);
        // v[64 + 8b..]: even from ce + se at j, odd from co - so at 31 - j
        let (lo, hi) = (ld(ce, k) + ld(se, k)).interleave(rev(ld(co, r) - ld(so, r)));
        lo.copy_to_slice(&mut v[64 + 2 * k..64 + 2 * k + 4]);
        hi.copy_to_slice(&mut v[64 + 2 * k + 4..64 + 2 * k + 8]);
    }
}

/// The synthesis window sums: out\[n\] = scale sum_{i<5} (v\[256i + n\] w\[128i + n\] +
/// v\[256i + 192 + n\] w\[128i + 64 + n\]), n < 64, summed in tap order from zero.
pub(super) fn synthesis_window(v: &[f32; 1280], w: &[f32; 640], scale: f32, out: &mut [f32; 64]) {
    for b in 0..16 {
        let n = 4 * b;
        let mut acc = V::splat(0.0);
        for i in 0..5 {
            acc += ld(v, 256 * i + n) * ld(w, 128 * i + n)
                + ld(v, 256 * i + 192 + n) * ld(w, 128 * i + 64 + n);
        }
        (acc * V::splat(scale)).copy_to_slice(&mut out[n..n + 4]);
    }
}

/// The prediction coefficients of each band of `xl`, into `alpha`: the covariances of four
/// bands at a time.
pub(super) fn predictors(xl: &[[Cpx; XL_LEN]], alpha: &mut [(Cpx, Cpx)]) {
    for (rows, alpha) in xl.chunks(4).zip(alpha.chunks_mut(4)) {
        // a short last group repeats its last band in the spare lanes
        let lanes: [&[Cpx; XL_LEN]; 4] = std::array::from_fn(|j| &rows[j.min(rows.len() - 1)]);
        for (a, sums) in alpha.iter_mut().zip(covariance4(lanes)) {
            *a = predictor(&sums);
        }
    }
}

/// The covariances of one band each for four bands, lane j from band `x[j]`. Each lane
/// adds the scalar code's terms in its order. The bands are transposed two slots at a
/// time, so that every vector holds one slot's real or imaginary parts of the four bands,
/// and the sums run over a window of the last three slots.
fn covariance4(x: [&[Cpx; XL_LEN]; 4]) -> [Covariance; 4] {
    // slots n and n + 1 of the four bands, as (re, im) vectors
    let slots = |n: usize| {
        let [a, b, c, d] = x.map(|x| ld2(x, n));
        let (ab_lo, cd_lo) = (
            simd_swizzle!(a, b, [0, 4, 1, 5]),
            simd_swizzle!(c, d, [0, 4, 1, 5]),
        );
        let (ab_hi, cd_hi) = (
            simd_swizzle!(a, b, [2, 6, 3, 7]),
            simd_swizzle!(c, d, [2, 6, 3, 7]),
        );
        (
            (
                simd_swizzle!(ab_lo, cd_lo, [0, 1, 4, 5]),
                simd_swizzle!(ab_lo, cd_lo, [2, 3, 6, 7]),
            ),
            (
                simd_swizzle!(ab_hi, cd_hi, [0, 1, 4, 5]),
                simd_swizzle!(ab_hi, cd_hi, [2, 3, 6, 7]),
            ),
        )
    };
    let zero = V::splat(0.0);
    let (mut r01re, mut r01im, mut r02re, mut r02im) = (zero, zero, zero, zero);
    let (mut r11, mut r12re, mut r12im, mut r22) = (zero, zero, zero, zero);
    // the term of slot n + 2 (a), with b and c the two slots before it
    let mut add = |(ar, ai): (V, V), (br, bi): (V, V), (cr, ci): (V, V)| {
        r01re += ar * br + ai * bi;
        r01im += ai * br - ar * bi;
        r02re += ar * cr + ai * ci;
        r02im += ai * cr - ar * ci;
        r11 += br * br + bi * bi;
        r12re += br * cr + bi * ci;
        r12im += bi * cr - br * ci;
        r22 += cr * cr + ci * ci;
    };
    let (mut c, mut b) = slots(0);
    for n in (2..XL_LEN).step_by(2) {
        let (a0, a1) = slots(n);
        add(a0, b, c);
        add(a1, a0, b);
        (c, b) = (a0, a1);
    }
    std::array::from_fn(|j| Covariance {
        r01: Cpx::new(r01re[j], r01im[j]),
        r02: Cpx::new(r02re[j], r02im[j]),
        r11: r11[j],
        r12: Cpx::new(r12re[j], r12im[j]),
        r22: r22[j],
    })
}

/// One Parametric Stereo hybrid filter over a frame: `out[t] = sum_d f[d] e[t + 12 - d]`,
/// t < 32, each sum from zero in tap order, with the real taps alone when `real` (a
/// mirror pair). Four slots a vector, real and imaginary parts apart.
pub(super) fn hybrid_filter(f: &[Cpx; 13], e: &[Cpx; 44], real: bool, out: &mut [Cpx; 32]) {
    let mut re = [0f32; 44];
    let mut im = [0f32; 44];
    for ((r, i), v) in re.iter_mut().zip(im.iter_mut()).zip(e) {
        *r = v.re;
        *i = v.im;
    }
    for (b, out) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let t = 4 * b;
        let (mut acc_re, mut acc_im) = (V::splat(0.0), V::splat(0.0));
        for (d, c) in f.iter().enumerate() {
            let (vr, vi) = (ld(&re, t + 12 - d), ld(&im, t + 12 - d));
            let (cr, ci) = (V::splat(c.re), V::splat(c.im));
            if real {
                acc_re += cr * vr;
                acc_im += cr * vi;
            } else {
                acc_re += cr * vr - ci * vi;
                acc_im += cr * vi + ci * vr;
            }
        }
        for (o, (r, i)) in out
            .iter_mut()
            .zip(acc_re.to_array().into_iter().zip(acc_im.to_array()))
        {
            *o = Cpx::new(r, i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::scalar;
    use super::*;

    /// Values well inside the normal range, so that no product or sum is subnormal and
    /// the kernels must equal the scalar arithmetic bit for bit on every target.
    fn values<const N: usize>(seed: u32) -> [f32; N] {
        let mut s = seed;
        std::array::from_fn(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s as i32) as f32 / 2_147_483_648.0 * 1000.0
        })
    }

    fn cpx<const N: usize>(seed: u32) -> [Cpx; N] {
        let (re, im) = (values::<N>(seed), values::<N>(seed + 1000));
        std::array::from_fn(|k| Cpx::new(re[k], im[k]))
    }

    fn bits(v: &[f32]) -> Vec<u32> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    fn cpx_bits(v: &[Cpx]) -> Vec<(u32, u32)> {
        v.iter().map(|c| (c.re.to_bits(), c.im.to_bits())).collect()
    }

    #[test]
    fn analysis_kernels_equal_the_scalar_arithmetic() {
        let w = values::<320>(1);
        let input = values::<32>(2);
        let (mut x, mut expected) = (values::<320>(3), values::<320>(3));
        let u = analysis_window(&mut x, &input, 0.5, &w);
        let scalar_u = scalar::analysis_window(&mut expected, &input, 0.5, &w);
        assert_eq!(bits(&x), bits(&expected));
        assert_eq!(bits(&u), bits(&scalar_u));
        let ((d, r), (scalar_d, scalar_r)) = (analysis_fold(&u), scalar::analysis_fold(&u));
        assert_eq!(bits(&d), bits(&scalar_d));
        assert_eq!(bits(&r), bits(&scalar_r));
    }

    #[test]
    fn synthesis_kernels_equal_the_scalar_arithmetic() {
        let x = cpx::<64>(4);
        let (mut a, mut b) = ([0f32; 64], [0f32; 64]);
        let (mut scalar_a, mut scalar_b) = ([0f32; 64], [0f32; 64]);
        synthesis_inputs(&x, &mut a, &mut b);
        scalar::synthesis_inputs(&x, &mut scalar_a, &mut scalar_b);
        assert_eq!((bits(&a), bits(&b)), (bits(&scalar_a), bits(&scalar_b)));

        let [ce, co, se, so] = [6, 7, 8, 9].map(values::<32>);
        let (mut v, mut expected) = ([0f32; 128], [0f32; 128]);
        synthesis_butterflies(&ce, &co, &se, &so, &mut v);
        scalar::synthesis_butterflies(&ce, &co, &se, &so, &mut expected);
        assert_eq!(bits(&v), bits(&expected));

        let history = values::<1280>(10);
        let w = values::<640>(11);
        let (mut out, mut scalar_out) = ([0f32; 64], [0f32; 64]);
        synthesis_window(&history, &w, 0.25, &mut out);
        scalar::synthesis_window(&history, &w, 0.25, &mut scalar_out);
        assert_eq!(bits(&out), bits(&scalar_out));
    }

    #[test]
    fn covariance_kernel_equals_the_scalar_arithmetic() {
        let bands: [[Cpx; XL_LEN]; 5] = std::array::from_fn(|j| cpx::<XL_LEN>(20 + j as u32));
        let sums = covariance4(std::array::from_fn(|j| &bands[j]));
        for (band, sums) in bands.iter().zip(sums) {
            let scalar = scalar::covariance(band);
            let parts = |c: &Covariance| cpx_bits(&[c.r01, c.r02, c.r12, Cpx::new(c.r11, c.r22)]);
            assert_eq!(parts(&sums), parts(&scalar));
        }
        // five bands: the second group repeats its one band in the spare lanes
        let (mut alpha, mut scalar_alpha) = (
            [(Cpx::default(), Cpx::default()); 5],
            [(Cpx::default(), Cpx::default()); 5],
        );
        predictors(&bands, &mut alpha);
        scalar::predictors(&bands, &mut scalar_alpha);
        let flat = |a: &[(Cpx, Cpx)]| {
            cpx_bits(&a.iter().flat_map(|&(a0, a1)| [a0, a1]).collect::<Vec<_>>())
        };
        assert_eq!(flat(&alpha), flat(&scalar_alpha));
    }

    #[test]
    fn hybrid_filter_equals_the_scalar_arithmetic() {
        let (f, e) = (cpx::<13>(30), cpx::<44>(31));
        for real in [false, true] {
            let (mut out, mut scalar_out) = ([Cpx::default(); 32], [Cpx::default(); 32]);
            hybrid_filter(&f, &e, real, &mut out);
            scalar::hybrid_filter(&f, &e, real, &mut scalar_out);
            assert_eq!(cpx_bits(&out), cpx_bits(&scalar_out), "real taps {real}");
        }
    }
}
