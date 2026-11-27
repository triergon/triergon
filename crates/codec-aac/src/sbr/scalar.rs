//! The scalar forms of the SBR QMF banks' windowing and shuffles, of the predictors HF
//! generation computes from the covariances, and of the Parametric Stereo hybrid filters:
//! the default code, and the arithmetic the `simd` forms reproduce lane by lane.

use super::{Covariance, NUM_TIME_SLOTS, XL_LEN, predictor};
use crate::fft::Cpx;

/// The analysis input step and window sums: writes the 32 new samples (oldest first)
/// reversed and scaled to `x[..32]`, then returns u\[n\] = sum_{j<5} x\[64j + n\]
/// w\[64j + n\], n < 64, each summed in tap order from zero.
#[inline(always)]
pub(super) fn analysis_window(
    x: &mut [f32; 320],
    input: &[f32; 32],
    scale: f32,
    w: &[f32; 320],
) -> [f32; 64] {
    for n in 0..32 {
        x[31 - n] = input[n] * scale;
    }
    // each sum in a register over its five taps x[64j + n] w[64j + n], in tap order, four
    // sums a block
    let x: [&[[f32; 4]]; 5] = std::array::from_fn(|j| x[64 * j..][..64].as_chunks().0);
    let w: [&[[f32; 4]]; 5] = std::array::from_fn(|j| w[64 * j..][..64].as_chunks().0);
    let mut u = [0f32; 64];
    for (b, u) in u.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        *u = std::array::from_fn(|i| (0..5).fold(0.0, |u, j| u + x[j][b][i] * w[j][b][i]));
    }
    u
}

/// The analysis folds: d\[n\] = u\[n\] - u\[63 - n\] and r\[n\] = u\[31 - n\] + u\[32 + n\],
/// n < 32 (the DST-IV as a DCT-IV of the reversed input).
#[inline(always)]
pub(super) fn analysis_fold(u: &[f32; 64]) -> ([f32; 32], [f32; 32]) {
    (
        std::array::from_fn(|n| u[n] - u[63 - n]),
        std::array::from_fn(|n| u[31 - n] + u[32 + n]),
    )
}

/// The synthesis inputs: a\[k\] = (-1)^k Im x\[k\] and b\[k\] = -(-1)^k Re x\[63 - k\].
#[inline(always)]
pub(super) fn synthesis_inputs(x: &[Cpx; 64], a: &mut [f32; 64], b: &mut [f32; 64]) {
    for k in 0..64 {
        let sgn = if k & 1 == 0 { 1.0 } else { -1.0 };
        a[k] = sgn * x[k].im;
        // reversed, with (-1)^(63 - j) = -(-1)^j
        b[k] = -sgn * x[63 - k].re;
    }
}

/// The synthesis butterflies into v (128): v\[64 + 2k\] = ce + se, v\[63 - 2k\] = ce - se,
/// v\[127 - 2k\] = co - so and v\[2k\] = co + so, k < 32.
#[inline(always)]
pub(super) fn synthesis_butterflies(
    ce: &[f32; 32],
    co: &[f32; 32],
    se: &[f32; 32],
    so: &[f32; 32],
    v: &mut [f32; 128],
) {
    for (k, (((&ce, &co), &se), &so)) in ce.iter().zip(co).zip(se).zip(so).enumerate() {
        v[64 + 2 * k] = ce + se;
        v[63 - 2 * k] = ce - se;
        v[127 - 2 * k] = co - so;
        v[2 * k] = co + so;
    }
}

/// The synthesis window sums: out\[n\] = scale sum_{i<5} (v\[256i + n\] w\[128i + n\] +
/// v\[256i + 192 + n\] w\[128i + 64 + n\]), n < 64, summed in tap order from zero.
#[inline(always)]
pub(super) fn synthesis_window(v: &[f32; 1280], w: &[f32; 640], scale: f32, out: &mut [f32; 64]) {
    // tap 2i reads v[256i + n] and w[128i + n], tap 2i + 1 v[256i + 192 + n] and
    // w[128i + 64 + n]
    let v: [&[[f32; 4]]; 10] =
        std::array::from_fn(|t| v[256 * (t / 2) + 192 * (t % 2)..][..64].as_chunks().0);
    let w: [&[[f32; 4]]; 10] =
        std::array::from_fn(|t| w[128 * (t / 2) + 64 * (t % 2)..][..64].as_chunks().0);
    for (b, out) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        for (j, out) in out.iter_mut().enumerate() {
            let acc = (0..5).fold(0.0, |acc, i| {
                acc + (v[2 * i][b][j] * w[2 * i][b][j] + v[2 * i + 1][b][j] * w[2 * i + 1][b][j])
            });
            *out = acc * scale;
        }
    }
}

/// The prediction coefficients of each band of `xl`, into `alpha`.
pub(super) fn predictors(xl: &[[Cpx; XL_LEN]], alpha: &mut [(Cpx, Cpx)]) {
    for (x, a) in xl.iter().zip(alpha) {
        *a = predictor(&covariance(x));
    }
}

/// The covariances of one band, each summed in time order from zero.
pub(super) fn covariance(x: &[Cpx; XL_LEN]) -> Covariance {
    let mut r01 = Cpx::default();
    let mut r02 = Cpx::default();
    let mut r11 = 0f32;
    let mut r12 = Cpx::default();
    let mut r22 = 0f32;
    for n in 0..(2 * NUM_TIME_SLOTS + 6) {
        let a = x[n + 2];
        let b = x[n + 1];
        let cc = x[n];
        // phi(0,1) = sum a conj(b)
        r01.re += a.re * b.re + a.im * b.im;
        r01.im += a.im * b.re - a.re * b.im;
        r02.re += a.re * cc.re + a.im * cc.im;
        r02.im += a.im * cc.re - a.re * cc.im;
        r11 += b.re * b.re + b.im * b.im;
        r12.re += b.re * cc.re + b.im * cc.im;
        r12.im += b.im * cc.re - b.re * cc.im;
        r22 += cc.re * cc.re + cc.im * cc.im;
    }
    Covariance {
        r01,
        r02,
        r11,
        r12,
        r22,
    }
}

/// One Parametric Stereo hybrid filter over a frame: `out[t] = sum_d f[d] e[t + 12 - d]`,
/// t < 32, each sum from zero in tap order, with the real taps alone when `real` (a
/// mirror pair).
#[inline(always)]
pub(super) fn hybrid_filter(f: &[Cpx; 13], e: &[Cpx; 44], real: bool, out: &mut [Cpx; 32]) {
    for (t, o) in out.iter_mut().enumerate() {
        // M[t] = sum_d f[d] X[t - d], X[t'] at e[t' + 6]
        let w = &e[t..t + 13];
        let mut acc = Cpx::default();
        if real {
            for (c, v) in f.iter().zip(w.iter().rev()) {
                acc.re += c.re * v.re;
                acc.im += c.re * v.im;
            }
        } else {
            for (c, v) in f.iter().zip(w.iter().rev()) {
                let p = c.mul(*v);
                acc.re += p.re;
                acc.im += p.im;
            }
        }
        *o = acc;
    }
}
