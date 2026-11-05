//! Inverse MDCT of A/52:2018 7.9.4 (512-sample and block-switched 2x256-sample transforms)
//! computed with an N/4-point (resp. N/8-point) complex IFFT and pre/post twiddles, followed
//! by windowing and overlap-add. The "2 *" output gain of step 6 is folded into the window.

use crate::tables::kbd_window;
use core::f64::consts::PI;

/// In-place complex inverse FFT (z[n] = sum Z[k] e^{+j 2 pi k n / N}) for a fixed size.
struct Fft<const N: usize> {
    bitrev: [u8; N],
    /// Twiddles for every stage, stage with half-size h at [h..2h].
    tw_re: [f32; N],
    tw_im: [f32; N],
}

impl<const N: usize> Fft<N> {
    fn new() -> Self {
        let bits = N.trailing_zeros();
        let mut bitrev = [0u8; N];
        for (i, b) in bitrev.iter_mut().enumerate() {
            *b = ((i as u32).reverse_bits() >> (32 - bits)) as u8;
        }
        let mut tw_re = [0f32; N];
        let mut tw_im = [0f32; N];
        let mut h = 1;
        while h < N {
            for k in 0..h {
                let a = PI * k as f64 / h as f64;
                tw_re[h + k] = a.cos() as f32;
                tw_im[h + k] = a.sin() as f32;
            }
            h *= 2;
        }
        Fft {
            bitrev,
            tw_re,
            tw_im,
        }
    }

    /// Input must already be in bit-reversed order.
    #[inline]
    fn run(&self, re: &mut [f32; N], im: &mut [f32; N]) {
        // Stage h = 1 and h = 2 as one radix-4 pass (no multiplications).
        let mut s = 0;
        while s < N {
            let (a0r, a0i) = (re[s], im[s]);
            let (a1r, a1i) = (re[s + 1], im[s + 1]);
            let (a2r, a2i) = (re[s + 2], im[s + 2]);
            let (a3r, a3i) = (re[s + 3], im[s + 3]);
            let (b0r, b0i) = (a0r + a1r, a0i + a1i);
            let (b1r, b1i) = (a0r - a1r, a0i - a1i);
            let (b2r, b2i) = (a2r + a3r, a2i + a3i);
            let (b3r, b3i) = (a2r - a3r, a2i - a3i);
            // multiply b3 by +j (inverse transform)
            let (c3r, c3i) = (-b3i, b3r);
            re[s] = b0r + b2r;
            im[s] = b0i + b2i;
            re[s + 2] = b0r - b2r;
            im[s + 2] = b0i - b2i;
            re[s + 1] = b1r + c3r;
            im[s + 1] = b1i + c3i;
            re[s + 3] = b1r - c3r;
            im[s + 3] = b1i - c3i;
            s += 4;
        }
        let mut h = 4;
        while h < N {
            let twr = &self.tw_re[h..2 * h];
            let twi = &self.tw_im[h..2 * h];
            let mut g = 0;
            while g < N {
                let (lo_r, hi_r) = re[g..g + 2 * h].split_at_mut(h);
                let (lo_i, hi_i) = im[g..g + 2 * h].split_at_mut(h);
                for k in 0..h {
                    let tr = hi_r[k] * twr[k] - hi_i[k] * twi[k];
                    let ti = hi_r[k] * twi[k] + hi_i[k] * twr[k];
                    hi_r[k] = lo_r[k] - tr;
                    hi_i[k] = lo_i[k] - ti;
                    lo_r[k] += tr;
                    lo_i[k] += ti;
                }
                g += 2 * h;
            }
            h *= 2;
        }
    }
}

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
    re: [f32; 128],
    im: [f32; 128],
    re2: [f32; 64],
    im2: [f32; 64],
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
            re: [0.0; 128],
            im: [0.0; 128],
            re2: [0.0; 64],
            im2: [0.0; 64],
            yr: [0.0; 128],
            yi: [0.0; 128],
        })
    }

    /// 512-sample transform (blksw = 0). Writes 256 output samples and updates `delay`.
    pub(crate) fn long(&mut self, x: &[f32; 256], delay: &mut [f32; 256], out: &mut [f32]) {
        let out: &mut [f32; 256] = (&mut out[..256]).try_into().expect("256 samples");
        // Pre-twiddle (contiguous, vectorisable), then bit-reversal permutation.
        let (zr, zi) = (&mut self.yr, &mut self.yi);
        for k in 0..128 {
            let xr = x[255 - 2 * k];
            let xi = x[2 * k];
            zr[k] = xr * self.c1[k] - xi * self.s1[k];
            zi[k] = xi * self.c1[k] + xr * self.s1[k];
        }
        for k in 0..128 {
            let p = self.fft128.bitrev[k] as usize;
            self.re[p] = zr[k];
            self.im[p] = zi[k];
        }
        self.fft128.run(&mut self.re, &mut self.im);
        let (yr, yi) = (&mut self.yr, &mut self.yi);
        for n in 0..128 {
            let (zr, zi) = (self.re[n], self.im[n]);
            yr[n] = zr * self.c1[n] - zi * self.s1[n];
            yi[n] = zi * self.c1[n] + zr * self.s1[n];
        }
        window_overlap(yr, yi, &self.w, delay, out);
    }

    /// Two interleaved 256-sample transforms (blksw = 1).
    pub(crate) fn short(&mut self, x: &[f32; 256], delay: &mut [f32; 256], out: &mut [f32]) {
        let out = &mut out[..256];
        // First half-block (even coefficients) -> y1 stored in yr/yi[0..64].
        for half in 0..2 {
            for k in 0..64 {
                let xr = x[2 * (127 - 2 * k) + half];
                let xi = x[2 * (2 * k) + half];
                let p = self.fft64.bitrev[k] as usize;
                self.re2[p] = xr * self.c2[k] - xi * self.s2[k];
                self.im2[p] = xi * self.c2[k] + xr * self.s2[k];
            }
            self.fft64.run(&mut self.re2, &mut self.im2);
            for n in 0..64 {
                let (zr, zi) = (self.re2[n], self.im2[n]);
                self.yr[half * 64 + n] = zr * self.c2[n] - zi * self.s2[n];
                self.yi[half * 64 + n] = zi * self.c2[n] + zr * self.s2[n];
            }
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
        for v in x.iter_mut() {
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
            for v in x.iter_mut() {
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
    #[ignore]
    fn imdct_speed() {
        let mut t = Imdct::new();
        let mut x = [0f32; 256];
        for (i, v) in x.iter_mut().enumerate() {
            *v = (i as f32 * 0.37).sin();
        }
        let mut d = [0f32; 256];
        let mut o = [0f32; 256];
        let n = 500_000;
        let (mut el, mut el2) = (f64::MAX, f64::MAX);
        for _ in 0..7 {
            let st = std::time::Instant::now();
            for _ in 0..n {
                t.long(&x, &mut d, &mut o);
                x[3] += o[5] * 1e-9;
            }
            el = el.min(st.elapsed().as_secs_f64());
            let st2 = std::time::Instant::now();
            for _ in 0..n {
                t.fft128.run(&mut t.re, &mut t.im);
                t.re[1] += 1e-9;
            }
            el2 = el2.min(st2.elapsed().as_secs_f64());
        }
        eprintln!(
            "long: {:.1} ns/call, fft128: {:.1} ns/call",
            el / n as f64 * 1e9,
            el2 / n as f64 * 1e9
        );
    }
}
