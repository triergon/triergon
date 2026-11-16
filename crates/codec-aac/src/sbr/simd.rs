//! `std::simd` forms of the SBR hot loops, used with the opt-in `simd` feature (nightly).
//!
//! Every lane performs the scalar code's operations in the scalar code's order, so the
//! results equal the scalar code's wherever the vector unit follows IEEE 754 (AArch64).
//! ARMv7 NEON flushes subnormal operands and results to zero, so there the results can
//! differ from the scalar code's for subnormal values.

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
