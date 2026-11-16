//! Inverse MDCT of A/52:2018 7.9.4 (512-sample and block-switched 2x256-sample transforms)
//! computed with an N/4-point (resp. N/8-point) complex IFFT and pre/post twiddles, followed
//! by windowing and overlap-add. The "2 *" output gain of step 6 is folded into the window.

use crate::tables::kbd_window;
use core::f64::consts::PI;

/// 1/sqrt(2), the real and imaginary part of the eighth root of unity.
const H: f32 = core::f32::consts::FRAC_1_SQRT_2;

/// Complex inverse FFT (Z(n) = sum z(k) e^{+j 2 pi k n / N}) of N = 16 B points, B = 8 for
/// N = 128 and B = 4 for N = 64: B-point DFTs of the inputs in digit-reversed order, then
/// two radix-4 decimation-in-time passes (J. W. Cooley and J. W. Tukey, "An algorithm for
/// the machine calculation of complex Fourier series", 1965; the eight-point kernel as in
/// G. D. Bergland, "A fast Fourier transform algorithm using base 8 iterations", 1968).
/// Every pass reads and writes each point once, so the transform makes three passes over
/// memory instead of a radix-2 transform's six, and the pre- and post-twiddles ride along
/// with the first and the last pass.
#[derive(Debug)]
struct Fft<const N: usize> {
    /// cos and sin of 2 pi q k / L for q = 1..3, for the pass of length L at [L/4 + k].
    cos: [[f32; N]; 3],
    sin: [[f32; N]; 3],
}

impl<const N: usize> Fft<N> {
    fn new() -> Self {
        let mut cos = [[0f32; N]; 3];
        let mut sin = [[0f32; N]; 3];
        for l in [N / 4, N] {
            for k in 0..l / 4 {
                for q in 0..3 {
                    let a = 2.0 * PI * ((q + 1) * k) as f64 / l as f64;
                    cos[q][l / 4 + k] = a.cos() as f32;
                    sin[q][l / 4 + k] = a.sin() as f32;
                }
            }
        }
        Fft { cos, sin }
    }

    /// Transforms the points `z(k)`, k = 0..N, into `re`/`im` in natural order with output n
    /// multiplied by `post` (cos, sin) at n.
    #[inline(always)]
    fn run(
        &self,
        z: impl Fn(usize) -> (f32, f32),
        post: (&[f32; N], &[f32; N]),
        re: &mut [f32; N],
        im: &mut [f32; N],
    ) {
        if N == 128 {
            dft8_pass(z, re, im);
        } else {
            dft4_pass(z, re, im);
        }
        self.radix4_pass(N / 4, re, im);
        self.last_pass(post, re, im);
    }

    /// Combines four DFTs of length L/4 at each multiple of L into one of length L.
    #[inline(always)]
    fn radix4_pass(&self, l: usize, re: &mut [f32; N], im: &mut [f32; N]) {
        let q = l / 4;
        let (c, s) = (&self.cos, &self.sin);
        for (re, im) in re.chunks_exact_mut(l).zip(im.chunks_exact_mut(l)) {
            let [r0, r1, r2, r3] = quarters(re, q);
            let [i0, i1, i2, i3] = quarters(im, q);
            for k in 0..q {
                let a1 = cmul((r1[k], i1[k]), (c[0][q + k], s[0][q + k]));
                let a2 = cmul((r2[k], i2[k]), (c[1][q + k], s[1][q + k]));
                let a3 = cmul((r3[k], i3[k]), (c[2][q + k], s[2][q + k]));
                let x = dft4((r0[k], i0[k]), a1, a2, a3);
                (r0[k], i0[k]) = x[0];
                (r1[k], i1[k]) = x[1];
                (r2[k], i2[k]) = x[2];
                (r3[k], i3[k]) = x[3];
            }
        }
    }

    /// The final radix-4 pass (length N) with the post-twiddle of every output.
    #[inline(always)]
    fn last_pass(&self, (pc, ps): (&[f32; N], &[f32; N]), re: &mut [f32; N], im: &mut [f32; N]) {
        let q = N / 4;
        let (c, s) = (&self.cos, &self.sin);
        let [r0, r1, r2, r3] = quarters(re, q);
        let [i0, i1, i2, i3] = quarters(im, q);
        let [pc0, pc1, pc2, pc3] = quarters_of(pc, q);
        let [ps0, ps1, ps2, ps3] = quarters_of(ps, q);
        for k in 0..q {
            let a1 = cmul((r1[k], i1[k]), (c[0][q + k], s[0][q + k]));
            let a2 = cmul((r2[k], i2[k]), (c[1][q + k], s[1][q + k]));
            let a3 = cmul((r3[k], i3[k]), (c[2][q + k], s[2][q + k]));
            let x = dft4((r0[k], i0[k]), a1, a2, a3);
            (r0[k], i0[k]) = cmul(x[0], (pc0[k], ps0[k]));
            (r1[k], i1[k]) = cmul(x[1], (pc1[k], ps1[k]));
            (r2[k], i2[k]) = cmul(x[2], (pc2[k], ps2[k]));
            (r3[k], i3[k]) = cmul(x[3], (pc3[k], ps3[k]));
        }
    }
}

/// The four consecutive quarters of length `q` of `v`.
#[inline(always)]
fn quarters(v: &mut [f32], q: usize) -> [&mut [f32]; 4] {
    let (a, v) = v.split_at_mut(q);
    let (b, v) = v.split_at_mut(q);
    let (c, d) = v.split_at_mut(q);
    [a, b, c, &mut d[..q]]
}

#[inline(always)]
fn quarters_of(v: &[f32], q: usize) -> [&[f32]; 4] {
    let (a, v) = v.split_at(q);
    let (b, v) = v.split_at(q);
    let (c, d) = v.split_at(q);
    [a, b, c, &d[..q]]
}

type Complex = (f32, f32);

#[inline(always)]
fn cmul(a: Complex, w: Complex) -> Complex {
    (a.0 * w.0 - a.1 * w.1, a.1 * w.0 + a.0 * w.1)
}

/// Inverse 4-point DFT.
#[inline(always)]
fn dft4(a0: Complex, a1: Complex, a2: Complex, a3: Complex) -> [Complex; 4] {
    let t0 = (a0.0 + a2.0, a0.1 + a2.1);
    let t1 = (a0.0 - a2.0, a0.1 - a2.1);
    let t2 = (a1.0 + a3.0, a1.1 + a3.1);
    let t3 = (a1.0 - a3.0, a1.1 - a3.1);
    [
        (t0.0 + t2.0, t0.1 + t2.1),
        (t1.0 - t3.1, t1.1 + t3.0),
        (t0.0 - t2.0, t0.1 - t2.1),
        (t1.0 + t3.1, t1.1 - t3.0),
    ]
}

/// First input of the bottom DFT that lands at block `g` of 16: the two base-4 digits of
/// `g` swapped.
#[inline(always)]
fn digit_reversed(g: usize) -> usize {
    (g >> 2) | ((g & 3) << 2)
}

/// 16 inverse 4-point DFTs of the inputs k = b + 16 m, block g at 4g.
#[inline(always)]
fn dft4_pass<const N: usize>(
    z: impl Fn(usize) -> (f32, f32),
    re: &mut [f32; N],
    im: &mut [f32; N],
) {
    for g in 0..16 {
        let b = digit_reversed(g);
        let x = dft4(z(b), z(b + 16), z(b + 32), z(b + 48));
        for (m, x) in x.iter().enumerate() {
            re[4 * g + m] = x.0;
            im[4 * g + m] = x.1;
        }
    }
}

/// 16 inverse 8-point DFTs of the inputs k = b + 16 m, block g at 8g.
#[inline(always)]
fn dft8_pass<const N: usize>(
    z: impl Fn(usize) -> (f32, f32),
    re: &mut [f32; N],
    im: &mut [f32; N],
) {
    for g in 0..16 {
        let b = digit_reversed(g);
        let e = dft4(z(b), z(b + 32), z(b + 64), z(b + 96));
        let o = dft4(z(b + 16), z(b + 48), z(b + 80), z(b + 112));
        // o[m] times e^{j pi m / 4}.
        let o = [
            o[0],
            ((o[1].0 - o[1].1) * H, (o[1].0 + o[1].1) * H),
            (-o[2].1, o[2].0),
            ((-o[3].0 - o[3].1) * H, (o[3].0 - o[3].1) * H),
        ];
        for m in 0..4 {
            re[8 * g + m] = e[m].0 + o[m].0;
            im[8 * g + m] = e[m].1 + o[m].1;
            re[8 * g + m + 4] = e[m].0 - o[m].0;
            im[8 * g + m + 4] = e[m].1 - o[m].1;
        }
    }
}

#[derive(Debug)]
pub(crate) struct Imdct {
    fft128: Fft<128>,
    fft64: Fft<64>,
    /// xcos1/xsin1 of 7.9.4.1.
    c1: [f32; 128],
    s1: [f32; 128],
    /// xcos2/xsin2 of 7.9.4.2.
    c2: [f32; 64],
    s2: [f32; 64],
    /// 2 * w[n], n = 0..256 (first half of the symmetric 512-point window).
    w: [f32; 256],
    /// Post-twiddled IFFT output y[n] (two 64-point halves for the short transforms).
    yr: [f32; 128],
    yi: [f32; 128],
}

impl Imdct {
    pub(crate) fn new() -> Box<Self> {
        let mut c1 = [0f32; 128];
        let mut s1 = [0f32; 128];
        for k in 0..128 {
            let a = 2.0 * PI * (8 * k + 1) as f64 / (8.0 * 512.0);
            c1[k] = -a.cos() as f32;
            s1[k] = -a.sin() as f32;
        }
        let mut c2 = [0f32; 64];
        let mut s2 = [0f32; 64];
        for k in 0..64 {
            let a = 2.0 * PI * (8 * k + 1) as f64 / (4.0 * 512.0);
            c2[k] = -a.cos() as f32;
            s2[k] = -a.sin() as f32;
        }
        let win = kbd_window();
        let mut w = [0f32; 256];
        for (d, s) in w.iter_mut().zip(win.iter()) {
            *d = 2.0 * *s;
        }
        Box::new(Imdct {
            fft128: Fft::new(),
            fft64: Fft::new(),
            c1,
            s1,
            c2,
            s2,
            w,
            yr: [0.0; 128],
            yi: [0.0; 128],
        })
    }

    /// 512-sample transform (blksw = 0). Writes 256 output samples and updates `delay`.
    pub(crate) fn long(&mut self, x: &[f32; 256], delay: &mut [f32; 256], out: &mut [f32]) {
        let out: &mut [f32; 256] = (&mut out[..256]).try_into().expect("256 samples");
        let (c1, s1) = (&self.c1, &self.s1);
        // Pre-twiddle (step 2) as the transform reads its input.
        let z = |k: usize| {
            let (xr, xi) = (x[255 - 2 * k], x[2 * k]);
            (xr * c1[k] - xi * s1[k], xi * c1[k] + xr * s1[k])
        };
        self.fft128.run(z, (c1, s1), &mut self.yr, &mut self.yi);
        window_overlap(&self.yr, &self.yi, &self.w, delay, out);
    }

    /// Two interleaved 256-sample transforms (blksw = 1).
    pub(crate) fn short(&mut self, x: &[f32; 256], delay: &mut [f32; 256], out: &mut [f32]) {
        let out = &mut out[..256];
        let (c2, s2) = (&self.c2, &self.s2);
        // The even coefficients give y1 in yr/yi[0..64], the odd ones y2 in [64..128].
        let (y1r, y2r) = self.yr.split_at_mut(64);
        let (y1i, y2i) = self.yi.split_at_mut(64);
        for (half, yr, yi) in [(0, y1r, y1i), (1, y2r, y2i)] {
            let z = |k: usize| {
                let (xr, xi) = (x[2 * (127 - 2 * k) + half], x[4 * k + half]);
                (xr * c2[k] - xi * s2[k], xi * c2[k] + xr * s2[k])
            };
            let yr: &mut [f32; 64] = yr.try_into().expect("64 points");
            let yi: &mut [f32; 64] = yi.try_into().expect("64 points");
            self.fft64.run(z, (c2, s2), yr, yi);
        }
        let (y1r, y2r) = self.yr.split_at(64);
        let (y1i, y2i) = self.yi.split_at(64);
        let w = &self.w;
        for n in 0..64 {
            out[2 * n] = -y1i[n] * w[2 * n] + delay[2 * n];
            out[2 * n + 1] = y1r[63 - n] * w[2 * n + 1] + delay[2 * n + 1];
            out[128 + 2 * n] = -y1r[n] * w[128 + 2 * n] + delay[128 + 2 * n];
            out[128 + 2 * n + 1] = y1i[63 - n] * w[128 + 2 * n + 1] + delay[128 + 2 * n + 1];
        }
        for n in 0..64 {
            delay[2 * n] = -y2r[n] * w[255 - 2 * n];
            delay[2 * n + 1] = y2i[63 - n] * w[254 - 2 * n];
            delay[128 + 2 * n] = y2i[n] * w[127 - 2 * n];
            delay[128 + 2 * n + 1] = -y2r[63 - n] * w[126 - 2 * n];
        }
    }
}

/// Step 5 and 6 of 7.9.4.1 for the 512-sample transform: windowing, de-interleaving and
/// overlap-add with the previous block's second half.
#[inline(always)]
fn window_overlap(
    yr: &[f32; 128],
    yi: &[f32; 128],
    w: &[f32; 256],
    delay: &mut [f32; 256],
    out: &mut [f32; 256],
) {
    let (o_lo, o_hi) = out.split_at_mut(128);
    let (d_lo, d_hi) = delay.split_at_mut(128);
    let (w_lo, w_hi) = w.split_at(128);
    for n in 0..64 {
        o_lo[2 * n] = -yi[64 + n] * w_lo[2 * n] + d_lo[2 * n];
        o_lo[2 * n + 1] = yr[63 - n] * w_lo[2 * n + 1] + d_lo[2 * n + 1];
    }
    for n in 0..64 {
        o_hi[2 * n] = -yr[n] * w_hi[2 * n] + d_hi[2 * n];
        o_hi[2 * n + 1] = yi[127 - n] * w_hi[2 * n + 1] + d_hi[2 * n + 1];
    }
    for n in 0..64 {
        d_lo[2 * n] = -yr[64 + n] * w_hi[127 - 2 * n];
        d_lo[2 * n + 1] = yi[63 - n] * w_hi[126 - 2 * n];
    }
    for n in 0..64 {
        d_hi[2 * n] = yi[n] * w_lo[127 - 2 * n];
        d_hi[2 * n + 1] = -yr[127 - n] * w_lo[126 - 2 * n];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct evaluation of the inverse transform of 7.9.4 (pre-twiddle, O(N^2) IDFT,
    /// post-twiddle) used to validate the fast path.
    fn reference_long(x: &[f32; 256], delay: &mut [f64; 256]) -> Vec<f64> {
        let n_ = 512usize;
        let w = kbd_window();
        let mut zr = [0f64; 128];
        let mut zi = [0f64; 128];
        for k in 0..128 {
            let a = 2.0 * PI * (8 * k + 1) as f64 / (8.0 * n_ as f64);
            let (c, s) = (-a.cos(), -a.sin());
            let xr = x[255 - 2 * k] as f64;
            let xi = x[2 * k] as f64;
            zr[k] = xr * c - xi * s;
            zi[k] = xi * c + xr * s;
        }
        let mut yr = [0f64; 128];
        let mut yi = [0f64; 128];
        for n in 0..128 {
            let (mut sr, mut si) = (0.0, 0.0);
            for k in 0..128 {
                let a = 8.0 * PI * (k * n) as f64 / n_ as f64;
                sr += zr[k] * a.cos() - zi[k] * a.sin();
                si += zr[k] * a.sin() + zi[k] * a.cos();
            }
            let a = 2.0 * PI * (8 * n + 1) as f64 / (8.0 * n_ as f64);
            let (c, s) = (-a.cos(), -a.sin());
            yr[n] = sr * c - si * s;
            yi[n] = si * c + sr * s;
        }
        let w = |i: usize| w[i] as f64;
        let mut xo = [0f64; 512];
        for n in 0..64 {
            xo[2 * n] = -yi[64 + n] * w(2 * n);
            xo[2 * n + 1] = yr[63 - n] * w(2 * n + 1);
            xo[128 + 2 * n] = -yr[n] * w(128 + 2 * n);
            xo[128 + 2 * n + 1] = yi[127 - n] * w(128 + 2 * n + 1);
            xo[256 + 2 * n] = -yr[64 + n] * w(255 - 2 * n);
            xo[256 + 2 * n + 1] = yi[63 - n] * w(254 - 2 * n);
            xo[384 + 2 * n] = yi[n] * w(127 - 2 * n);
            xo[384 + 2 * n + 1] = -yr[127 - n] * w(126 - 2 * n);
        }
        let mut pcm = vec![0f64; 256];
        for n in 0..256 {
            pcm[n] = 2.0 * (xo[n] + delay[n]);
            delay[n] = xo[256 + n];
        }
        pcm
    }

    /// Inverse of the forward transform of A/52:2018 8.2.3.2 (synthesis kernel with the
    /// alpha phase term), windowed: returns 2N/... samples for one transform of length n_.
    fn direct_synthesis(x: &[f64], n_: usize, alpha: f64) -> Vec<f64> {
        (0..n_)
            .map(|n| {
                (0..n_ / 2)
                    .map(|k| {
                        let a = 2.0 * PI / (4.0 * n_ as f64) * ((2 * n + 1) * (2 * k + 1)) as f64
                            + PI / 4.0 * (2 * k + 1) as f64 * (1.0 + alpha);
                        x[k] * a.cos()
                    })
                    .sum::<f64>()
            })
            .collect()
    }

    #[test]
    fn transforms_match_the_forward_transform_definition() {
        let win = kbd_window();
        let w = |i: usize| {
            if i < 256 {
                win[i] as f64
            } else {
                win[511 - i] as f64
            }
        };
        let mut seed = 99u32;
        let mut x = [0f32; 256];
        for v in &mut x {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        }
        let xd: Vec<f64> = x.iter().map(|&v| v as f64).collect();
        let mut t = Imdct::new();
        // Long: 512 windowed samples = first 256 outputs (zero delay) + new delay.
        let mut delay = [0f32; 256];
        let mut out = [0f32; 256];
        t.long(&x, &mut delay, &mut out);
        let got: Vec<f64> = out.iter().chain(delay.iter()).map(|&v| v as f64).collect();
        let d = direct_synthesis(&xd, 512, 0.0);
        let want: Vec<f64> = (0..512).map(|n| d[n] * w(n)).collect();
        let scale = got.iter().zip(&want).map(|(a, b)| a * b).sum::<f64>()
            / want.iter().map(|b| b * b).sum::<f64>();
        let err = got
            .iter()
            .zip(&want)
            .map(|(a, b)| (a - scale * b).abs())
            .fold(0.0, f64::max);
        assert!(err < 1e-4, "long err {err} scale {scale}");
        // Short: first transform (even coefficients, alpha = -1) -> samples 0..256,
        // second (odd coefficients, alpha = +1) -> samples 256..512.
        let mut delay = [0f32; 256];
        let mut out = [0f32; 256];
        t.short(&x, &mut delay, &mut out);
        let got: Vec<f64> = out.iter().chain(delay.iter()).map(|&v| v as f64).collect();
        let x1: Vec<f64> = (0..128).map(|k| xd[2 * k]).collect();
        let x2: Vec<f64> = (0..128).map(|k| xd[2 * k + 1]).collect();
        let d1 = direct_synthesis(&x1, 256, -1.0);
        let d2 = direct_synthesis(&x2, 256, 1.0);
        let want: Vec<f64> = (0..512)
            .map(|n| if n < 256 { d1[n] } else { d2[n - 256] } * w(n))
            .collect();
        let s2 = got.iter().zip(&want).map(|(a, b)| a * b).sum::<f64>()
            / want.iter().map(|b| b * b).sum::<f64>();
        let err = got
            .iter()
            .zip(&want)
            .map(|(a, b)| (a - s2 * b).abs())
            .fold(0.0, f64::max);
        assert!(
            err < 1e-4,
            "short err {err} scale {s2} vs long scale {scale}"
        );
        eprintln!("long scale {scale} short scale {s2}");
    }

    #[test]
    fn fast_long_transform_matches_direct_evaluation() {
        let mut t = Imdct::new();
        let mut seed = 12345u32;
        let mut delay = [0f32; 256];
        let mut rdelay = [0f64; 256];
        for _block in 0..3 {
            let mut x = [0f32; 256];
            for v in &mut x {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            }
            let mut out = [0f32; 256];
            t.long(&x, &mut delay, &mut out);
            let r = reference_long(&x, &mut rdelay);
            for n in 0..256 {
                assert!(
                    (out[n] as f64 - r[n]).abs() < 2e-5,
                    "n={n} {} {}",
                    out[n],
                    r[n]
                );
            }
        }
    }

    #[test]
    fn single_coefficient_energy_is_preserved() {
        // A single coefficient yields a windowed cosine; energy check guards scaling.
        let mut t = Imdct::new();
        let mut x = [0f32; 256];
        x[10] = 1.0;
        let mut delay = [0f32; 256];
        let mut out = [0f32; 256];
        t.long(&x, &mut delay, &mut out);
        let e: f32 =
            out.iter().map(|v| v * v).sum::<f32>() + delay.iter().map(|v| v * v).sum::<f32>();
        // Time-domain energy of one unit coefficient through the "2 *" scaled transform.
        assert!((e - 512.0).abs() < 0.1, "energy {e}");
        let mut delay2 = [0f32; 256];
        t.short(&x, &mut delay2, &mut out);
        assert!(out.iter().any(|v| v.abs() > 1e-3));
    }
}

#[cfg(test)]
mod perf {
    use super::*;
    #[test]
    #[ignore = "timing probe; run on demand with --release"]
    fn imdct_speed() {
        let mut t = Imdct::new();
        let mut x = [0f32; 256];
        for (i, v) in x.iter_mut().enumerate() {
            *v = (i as f32 * 0.37).sin();
        }
        let mut d = [0f32; 256];
        let mut o = [0f32; 256];
        let n = 500_000;
        let (mut long, mut short) = (f64::MAX, f64::MAX);
        for _ in 0..7 {
            let st = std::time::Instant::now();
            for _ in 0..n {
                t.long(&x, &mut d, &mut o);
                x[3] += o[5] * 1e-9;
            }
            long = long.min(st.elapsed().as_secs_f64());
            let st = std::time::Instant::now();
            for _ in 0..n {
                t.short(&x, &mut d, &mut o);
                x[3] += o[5] * 1e-9;
            }
            short = short.min(st.elapsed().as_secs_f64());
        }
        eprintln!(
            "long: {:.1} ns/call, short: {:.1} ns/call",
            long / n as f64 * 1e9,
            short / n as f64 * 1e9
        );
    }
}
