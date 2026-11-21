//! `std::simd` forms of the DCT-IV's contiguous passes, used with the `simd` feature
//! (nightly): the pre-twiddle, the radix-4 stages after the first, the post-twiddle and
//! the last radix-2 stage fused with it. They serve the QMF banks' DCT-IVs and the
//! (I)MDCT alike.
//!
//! Every lane performs the scalar code's operations in the scalar code's order, so the
//! results equal the scalar code's wherever the vector unit follows IEEE 754 (AArch64).
//! ARMv7 NEON flushes subnormal operands and results to zero, so there the results can
//! differ from the scalar code's for subnormal values.

use std::simd::{f32x4, simd_swizzle};

type V = f32x4;

#[inline(always)]
fn ld(s: &[f32], i: usize) -> V {
    V::from_slice(&s[i..i + 4])
}

#[inline(always)]
fn st(v: V, s: &mut [f32], i: usize) {
    v.copy_to_slice(&mut s[i..i + 4]);
}

/// The pre-twiddle of [`super::pre`]: (x\[2n\] + i x\[2h - 1 - 2n\]) * pre\[n\], n < h.
#[inline(never)]
pub(super) fn pre(x: &[f32], pr: &[f32], pi: &[f32], tr: &mut [f32], ti: &mut [f32]) {
    let h = tr.len();
    let (x, pr, pi, ti) = (&x[..2 * h], &pr[..h], &pi[..h], &mut ti[..h]);
    for n in (0..h).step_by(4) {
        // x[2n..2n + 8] holds the even inputs; x[2h - 8 - 2n..2h - 2n] the odd ones,
        // newest first
        let (f0, f1) = (ld(x, 2 * n), ld(x, 2 * n + 4));
        let (r0, r1) = (ld(x, 2 * h - 8 - 2 * n), ld(x, 2 * h - 4 - 2 * n));
        let x0 = simd_swizzle!(f0, f1, [0, 2, 4, 6]);
        let x1 = simd_swizzle!(r0, r1, [7, 5, 3, 1]);
        let (p, q) = (ld(pr, n), ld(pi, n));
        st(x0 * p - x1 * q, tr, n);
        st(x0 * q + x1 * p, ti, n);
    }
}

/// The post-twiddle of [`super::post`]: ye = Re(z q), yo = -Im(z q).
#[inline(never)]
pub(super) fn post(re: &[f32], im: &[f32], qr: &[f32], qi: &[f32], ye: &mut [f32], yo: &mut [f32]) {
    let h = re.len();
    let (im, qr, qi, ye, yo) = (&im[..h], &qr[..h], &qi[..h], &mut ye[..h], &mut yo[..h]);
    for k in (0..h).step_by(4) {
        let (zr, zi, cr, ci) = (ld(re, k), ld(im, k), ld(qr, k), ld(qi, k));
        st(zr * cr - zi * ci, ye, k);
        st(-(zr * ci + zi * cr), yo, k);
    }
}

/// The last radix-2 stage fused with the post-twiddle of both halves, as
/// [`super::last_post`].
#[inline(never)]
// separate slice arguments, as the scalar form takes them
#[allow(clippy::too_many_arguments)]
pub(super) fn last_post(
    re0: &[f32],
    re1: &[f32],
    im0: &[f32],
    im1: &[f32],
    wr: &[f32],
    wi: &[f32],
    qr: &[f32],
    qi: &[f32],
    ye0: &mut [f32],
    ye1: &mut [f32],
    yo0: &mut [f32],
    yo1: &mut [f32],
) {
    let q = re0.len();
    for k in (0..q).step_by(4) {
        let (r1, i1, cr, ci) = (ld(re1, k), ld(im1, k), ld(wr, k), ld(wi, k));
        let tr = r1 * cr - i1 * ci;
        let ti = r1 * ci + i1 * cr;
        let (r0, i0) = (ld(re0, k), ld(im0, k));
        let (zr, zi) = (r0 + tr, i0 + ti);
        let (pr, pi) = (ld(qr, k), ld(qi, k));
        st(zr * pr - zi * pi, ye0, k);
        st(-(zr * pi + zi * pr), yo0, k);
        let (zr, zi) = (r0 - tr, i0 - ti);
        let (pr, pi) = (ld(qr, q + k), ld(qi, q + k));
        st(zr * pr - zi * pi, ye1, k);
        st(-(zr * pi + zi * pr), yo1, k);
    }
}

/// Four radix-4 butterflies, lane by lane as [`super::radix4`]: quarter `j` of the
/// butterflies is `(r[j], i[j])`, with twiddles W^k, W^2k, W^3k as `w` (re, im pairs).
#[inline(always)]
fn butterflies(r: [V; 4], i: [V; 4], w: [V; 6]) -> ([V; 4], [V; 4]) {
    let (t0r, t0i) = (r[0], i[0]);
    let t1r = r[1] * w[2] - i[1] * w[3];
    let t1i = r[1] * w[3] + i[1] * w[2];
    let t2r = r[2] * w[0] - i[2] * w[1];
    let t2i = r[2] * w[1] + i[2] * w[0];
    let t3r = r[3] * w[4] - i[3] * w[5];
    let t3i = r[3] * w[5] + i[3] * w[4];
    let (s01r, s01i, d01r, d01i) = (t0r + t1r, t0i + t1i, t0r - t1r, t0i - t1i);
    let (s23r, s23i, d23r, d23i) = (t2r + t3r, t2i + t3i, t2r - t3r, t2i - t3i);
    (
        [s01r + s23r, d01r + d23i, s01r - s23r, d01r - d23i],
        [s01i + s23i, d01i - d23r, s01i - s23i, d01i + d23r],
    )
}

/// One radix-4 stage of [`super::Fft::middle`] with quarters of `h` (a multiple of 4)
/// points: the butterflies of each group of `4h` points, four at a time.
pub(super) fn stage(re: &mut [f32], im: &mut [f32], h: usize, w: &[Vec<f32>; 6]) {
    let w: [&[f32]; 6] = std::array::from_fn(|t| &w[t][..h]);
    for (br, bi) in re.chunks_exact_mut(4 * h).zip(im.chunks_exact_mut(4 * h)) {
        for k in (0..h).step_by(4) {
            let at = [k, h + k, 2 * h + k, 3 * h + k];
            let (r, i) = butterflies(
                at.map(|j| ld(br, j)),
                at.map(|j| ld(bi, j)),
                w.map(|w| ld(w, k)),
            );
            for (j, (r, i)) in at.into_iter().zip(r.into_iter().zip(i)) {
                st(r, br, j);
                st(i, bi, j);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Fft, Stage};
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
    fn twiddle_kernels_equal_the_scalar_arithmetic() {
        for h in [16, 32, 256, 512] {
            let x = values(1, 2 * h);
            let [pr, pi] = [2, 3].map(|seed| values(seed, h));
            let (mut tr, mut ti) = (vec![0.0; h], vec![0.0; h]);
            let (mut sr, mut si) = (vec![0.0; h], vec![0.0; h]);
            pre(&x, &pr, &pi, &mut tr, &mut ti);
            super::super::pre(&x, &pr, &pi, &mut sr, &mut si);
            assert_eq!((bits(&tr), bits(&ti)), (bits(&sr), bits(&si)), "pre {h}");

            let [re, im, qr, qi] = [4, 5, 6, 7].map(|seed| values(seed, h));
            let (mut ye, mut yo) = (vec![0.0; h], vec![0.0; h]);
            let (mut se, mut so) = (vec![0.0; h], vec![0.0; h]);
            post(&re, &im, &qr, &qi, &mut ye, &mut yo);
            super::super::post(&re, &im, &qr, &qi, &mut se, &mut so);
            assert_eq!((bits(&ye), bits(&yo)), (bits(&se), bits(&so)), "post {h}");

            let q = h / 2;
            let [wr, wi] = [8, 9].map(|seed| values(seed, q));
            let (re, im) = (re.split_at(q), im.split_at(q));
            let mut vector = [(); 4].map(|()| vec![0.0; q]);
            let mut scalar = vector.clone();
            let [a, b, c, d] = &mut vector;
            last_post(re.0, re.1, im.0, im.1, &wr, &wi, &qr, &qi, a, b, c, d);
            let [a, b, c, d] = &mut scalar;
            super::super::last_post(re.0, re.1, im.0, im.1, &wr, &wi, &qr, &qi, a, b, c, d);
            assert_eq!(
                vector.map(|v| bits(&v)),
                scalar.map(|v| bits(&v)),
                "last {h}"
            );
        }
    }

    #[test]
    fn radix4_stages_equal_the_scalar_arithmetic() {
        for n in [16, 32, 64, 512] {
            let fft = Fft::new(n);
            for Stage { h, w } in &fft.stages {
                let (mut re, mut im) = (values(10, n), values(11, n));
                let (mut sr, mut si) = (re.clone(), im.clone());
                stage(&mut re, &mut im, *h, w);
                super::super::stage(&mut sr, &mut si, *h, w);
                assert_eq!((bits(&re), bits(&im)), (bits(&sr), bits(&si)), "{n} h {h}");
            }
        }
    }
}
