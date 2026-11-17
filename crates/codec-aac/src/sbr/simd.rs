//! `std::simd` forms of the SBR QMF banks' windowing and shuffles, of the covariances HF
//! generation predicts from, and of the Parametric Stereo hybrid filters, used with the
//! `simd` feature (nightly).
//!
//! Every lane performs the scalar code's operations in the scalar code's order, so the
//! results equal the scalar code's wherever the vector unit follows IEEE 754 (AArch64).
//! ARMv7 NEON flushes subnormal operands and results to zero, so there the results can
//! differ from the scalar code's for subnormal values.

use super::{Covariance, XL_LEN};
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
    x: &mut [f32],
    input: &[f32],
    scale: f32,
    w: &[f32; 320],
) -> [f32; 64] {
    let x: &mut [f32; 320] = x.try_into().expect("320 samples");
    let input: &[f32; 32] = input[..32].try_into().expect("32 samples");
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
pub(super) fn synthesis_inputs(x: &[Cpx], a: &mut [f32; 64], b: &mut [f32; 64]) {
    let x: &[Cpx; 64] = x[..64].try_into().expect("64 bands");
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
    ce: &[f32],
    co: &[f32],
    se: &[f32],
    so: &[f32],
    v: &mut [f32; 128],
) {
    let [ce, co, se, so]: [&[f32; 32]; 4] =
        [ce, co, se, so].map(|s| s[..32].try_into().expect("32 values"));
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
pub(super) fn synthesis_window(v: &[f32], w: &[f32; 640], scale: f32, out: &mut [f32]) {
    let v: &[f32; 1280] = v.try_into().expect("1280 samples");
    let out: &mut [f32; 64] = (&mut out[..64]).try_into().expect("64 samples");
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

/// The covariances of [`super::covariance`] for four bands, lane j from band `x[j]`. Each
/// lane adds the scalar code's terms in its order. The bands are transposed two slots at a
/// time, so that every vector holds one slot's real or imaginary parts of the four bands.
pub(super) fn covariance4(x: [&[Cpx; XL_LEN]; 4]) -> [Covariance; 4] {
    let mut re = [V::splat(0.0); XL_LEN];
    let mut im = [V::splat(0.0); XL_LEN];
    for n in (0..XL_LEN).step_by(2) {
        let [a, b, c, d] = x.map(|x| ld2(x, n));
        let (ab_lo, cd_lo) = (
            simd_swizzle!(a, b, [0, 4, 1, 5]),
            simd_swizzle!(c, d, [0, 4, 1, 5]),
        );
        let (ab_hi, cd_hi) = (
            simd_swizzle!(a, b, [2, 6, 3, 7]),
            simd_swizzle!(c, d, [2, 6, 3, 7]),
        );
        re[n] = simd_swizzle!(ab_lo, cd_lo, [0, 1, 4, 5]);
        im[n] = simd_swizzle!(ab_lo, cd_lo, [2, 3, 6, 7]);
        re[n + 1] = simd_swizzle!(ab_hi, cd_hi, [0, 1, 4, 5]);
        im[n + 1] = simd_swizzle!(ab_hi, cd_hi, [2, 3, 6, 7]);
    }
    let zero = V::splat(0.0);
    let (mut r01re, mut r01im, mut r02re, mut r02im) = (zero, zero, zero, zero);
    let (mut r11, mut r12re, mut r12im, mut r22) = (zero, zero, zero, zero);
    for n in 0..XL_LEN - 2 {
        let (ar, ai) = (re[n + 2], im[n + 2]);
        let (br, bi) = (re[n + 1], im[n + 1]);
        let (cr, ci) = (re[n], im[n]);
        r01re += ar * br + ai * bi;
        r01im += ai * br - ar * bi;
        r02re += ar * cr + ai * ci;
        r02im += ai * cr - ar * ci;
        r11 += br * br + bi * bi;
        r12re += br * cr + bi * ci;
        r12im += bi * cr - br * ci;
        r22 += cr * cr + ci * ci;
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
// the scalar forms are written index-style, as the default code's loops are
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    /// Values well inside the normal range, so that no product or sum is subnormal and
    /// the kernels must equal the scalar arithmetic bit for bit on every target.
    fn values(seed: u32, n: usize) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s as i32) as f32 / 2_147_483_648.0 * 1000.0
            })
            .collect()
    }

    fn bits(v: &[f32]) -> Vec<u32> {
        v.iter().map(|x| x.to_bits()).collect()
    }

    #[test]
    fn analysis_kernels_equal_the_scalar_arithmetic() {
        let w: [f32; 320] = values(1, 320).try_into().unwrap();
        let input = values(2, 32);
        let mut x = values(3, 320);
        let mut expected = x.clone();
        for n in 0..32 {
            expected[31 - n] = input[n] * 0.5;
        }
        let u = analysis_window(&mut x, &input, 0.5, &w);
        assert_eq!(bits(&x), bits(&expected));
        let scalar_u: Vec<f32> = (0..64)
            .map(|n| (0..5).fold(0.0, |u, j| u + expected[64 * j + n] * w[64 * j + n]))
            .collect();
        assert_eq!(bits(&u), bits(&scalar_u));
        let (d, r) = analysis_fold(&u);
        let scalar_d: Vec<f32> = (0..32).map(|n| u[n] - u[63 - n]).collect();
        let scalar_r: Vec<f32> = (0..32).map(|n| u[31 - n] + u[32 + n]).collect();
        assert_eq!(bits(&d), bits(&scalar_d));
        assert_eq!(bits(&r), bits(&scalar_r));
    }

    #[test]
    fn synthesis_kernels_equal_the_scalar_arithmetic() {
        let re = values(4, 64);
        let im = values(5, 64);
        let x: Vec<Cpx> = (0..64).map(|k| Cpx::new(re[k], im[k])).collect();
        let (mut a, mut b) = ([0f32; 64], [0f32; 64]);
        synthesis_inputs(&x, &mut a, &mut b);
        for k in 0..64 {
            let sgn = if k & 1 == 0 { 1.0 } else { -1.0 };
            assert_eq!(a[k].to_bits(), (sgn * x[k].im).to_bits(), "a[{k}]");
            assert_eq!(b[k].to_bits(), (-sgn * x[63 - k].re).to_bits(), "b[{k}]");
        }

        let [ce, co, se, so] = [6, 7, 8, 9].map(|seed| values(seed, 32));
        let mut v = [0f32; 128];
        synthesis_butterflies(&ce, &co, &se, &so, &mut v);
        let mut expected = [0f32; 128];
        for k in 0..32 {
            expected[64 + 2 * k] = ce[k] + se[k];
            expected[63 - 2 * k] = ce[k] - se[k];
            expected[127 - 2 * k] = co[k] - so[k];
            expected[2 * k] = co[k] + so[k];
        }
        assert_eq!(bits(&v), bits(&expected));

        let history = values(10, 1280);
        let w: [f32; 640] = values(11, 640).try_into().unwrap();
        let mut out = [0f32; 64];
        synthesis_window(&history, &w, 0.25, &mut out);
        let scalar: Vec<f32> = (0..64)
            .map(|n| {
                let acc = (0..5).fold(0.0, |acc, i| {
                    acc + (history[256 * i + n] * w[128 * i + n]
                        + history[256 * i + 192 + n] * w[128 * i + 64 + n])
                });
                acc * 0.25
            })
            .collect();
        assert_eq!(bits(&out), bits(&scalar));
    }

    fn cpx(seed: u32, n: usize) -> Vec<Cpx> {
        let re = values(seed, n);
        let im = values(seed + 1000, n);
        re.into_iter()
            .zip(im)
            .map(|(re, im)| Cpx::new(re, im))
            .collect()
    }

    fn cpx_bits(v: &[Cpx]) -> Vec<(u32, u32)> {
        v.iter().map(|c| (c.re.to_bits(), c.im.to_bits())).collect()
    }

    #[test]
    fn covariance_kernel_equals_the_scalar_arithmetic() {
        let bands: Vec<[Cpx; XL_LEN]> = (0..4)
            .map(|j| cpx(20 + j, XL_LEN).try_into().unwrap())
            .collect();
        let sums = covariance4(std::array::from_fn(|j| &bands[j]));
        for (band, sums) in bands.iter().zip(sums) {
            let scalar = super::super::covariance(band);
            let parts = |c: &Covariance| cpx_bits(&[c.r01, c.r02, c.r12, Cpx::new(c.r11, c.r22)]);
            assert_eq!(parts(&sums), parts(&scalar));
        }
    }

    #[test]
    fn hybrid_filter_equals_the_scalar_arithmetic() {
        let f: [Cpx; 13] = cpx(30, 13).try_into().unwrap();
        let e: [Cpx; 44] = cpx(31, 44).try_into().unwrap();
        for real in [false, true] {
            let mut out = [Cpx::default(); 32];
            hybrid_filter(&f, &e, real, &mut out);
            let scalar: Vec<Cpx> = (0..32)
                .map(|t| {
                    let mut acc = Cpx::default();
                    for (c, v) in f.iter().zip(e[t..t + 13].iter().rev()) {
                        if real {
                            acc.re += c.re * v.re;
                            acc.im += c.re * v.im;
                        } else {
                            let p = c.mul(*v);
                            acc.re += p.re;
                            acc.im += p.im;
                        }
                    }
                    acc
                })
                .collect();
            assert_eq!(cpx_bits(&out), cpx_bits(&scalar), "real taps {real}");
        }
    }
}
