//! SBR QMF banks: 32-band complex analysis and 64-band complex synthesis.
//!
//! Definitions (ISO/IEC 14496-3 4.6.18.4.1 / 4.6.18.4.2; the ISO text is not available here,
//! the equations are the implementer's recollection, confirmed exactly against the FFmpeg
//! 9.0.2 binary with `examples/sbr_mute.rs` and `harness/codecs/aac/tools/qmffit.py`):
//!
//! - analysis: x = 320 input samples, newest first; u\[n\] = sum_{j<5} x\[n + 64j\]
//!   c\[2(n + 64j)\]; X(k) = sum_{n<64} 2 u\[n\] exp(i pi (k + 1/2)(2n - 1/2) / 64), k < 32;
//! - synthesis: v (1280, newest 128 first) with v\[n\] = 1/64 sum_{k<64} Re(X(k) exp(i pi
//!   (k + 1/2)(2n - 255) / 128)); g\[128i + j\] = v\[256i + j\], g\[128i + 64 + j\] =
//!   v\[256i + 192 + j\]; out\[j\] = sum_{i<10} g\[64i + j\] c\[64i + j\].
//!
//! The buffer layout, windowing and summation steps are those printed in the flowcharts of
//! 3GPP TS 26.404 V19.0.0 Figure 6 (64-band analysis) and TS 26.405 V19.0.0 Figure 4 (32-band
//! synthesis), here at the decoder's band counts. Implemented with FFT / DCT-IV modulation
//! (own derivation, checked against the direct definitions in the unit tests).

use super::tables::QMF_WINDOW;
use crate::fft::{Cpx, Dct4, Fft};

/// Prototype window c[n], n < 640, of the analysis (c[2n]) and synthesis (c[n]) banks:
/// ETSI TS 101 980 V1.1.1 Annex I Table I.1 read in reverse order, c[n] = c_DRM[639 - n]
/// (so c[0] = 0). The DRM table lists the same 640 magnitudes as ISO/IEC 14496-3 Table
/// 4.A.89 but in the opposite orientation; the two orientations differ in sign at the four
/// segment boundaries n = 128, 256, 384, 512 and are not interchangeable. The reversed order
/// reproduces the FFmpeg 9.0.2 binary's pure-QMF output to 9.5e-8; the forward order with
/// c[0] = 0 is wrong at every odd output position.
fn window(n: usize) -> f64 {
    QMF_WINDOW[639 - n]
}

pub(crate) struct Analysis {
    /// Input history, newest first: the 320 samples x[0..320] live at x[pos..pos + 320]
    /// of a double-length buffer so no per-slot shifting is needed.
    x: Vec<f32>,
    pos: usize,
    win: Vec<f32>,
    // modulation via a 64-point complex transform
    pre: Vec<Cpx>,
    post: Vec<Cpx>,
    fft: Fft,
    re: Vec<f32>,
    im: Vec<f32>,
}

pub(crate) struct Synthesis {
    v: Vec<f32>,
    /// write position of the circular v buffer (in units of 128)
    pos: usize,
    win: Vec<f32>,
    dct: Dct4,
    a: [f32; 64],
    b: [f32; 64],
    c: [f32; 64],
    d: [f32; 64],
}

impl Analysis {
    pub(crate) fn new() -> Analysis {
        // X(k) = sum_{n<64} 2 u[n] exp(i pi (k + 1/2)(2n + A) / 64), k < 32, A = -1/2:
        // exp(i pi (k+1/2)(2n+A)/64) = exp(i pi (2n + A)/128) * exp(i 2 pi k n / 64) * exp(i pi k A / 64)
        // i.e. a 64-point inverse DFT between a pre- and a post-twiddle.
        let pi = std::f64::consts::PI;
        let aoff: f64 = -0.5;
        let pre = (0..64)
            .map(|n| {
                let a = pi * (2.0 * n as f64 + aoff) / 128.0;
                Cpx::new((2.0 * a.cos()) as f32, (2.0 * a.sin()) as f32)
            })
            .collect();
        let post = (0..32)
            .map(|k| {
                let a = pi * k as f64 * aoff / 64.0;
                Cpx::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        Analysis {
            x: vec![0.0; 640],
            pos: 320,
            win: (0..320).map(|n| window(2 * n) as f32).collect(),
            pre,
            post,
            fft: Fft::new(64),
            re: vec![0.0; 64],
            im: vec![0.0; 64],
        }
    }

    pub(crate) fn reset(&mut self) {
        self.x.fill(0.0);
        self.pos = 320;
    }

    /// Process 32 new input samples (oldest first) into 32 complex subband samples.
    pub(crate) fn run(&mut self, input: &[f32], scale: f32, out: &mut [Cpx]) {
        if self.pos < 32 {
            self.x.copy_within(self.pos..self.pos + 288, 640 - 288);
            self.pos = 640 - 288;
        }
        self.pos -= 32;
        let p = self.pos;
        for n in 0..32 {
            self.x[p + 31 - n] = input[n] * scale;
        }
        let x = &self.x[p..p + 320];
        let w = &self.win[..320];
        let mut u = [0f32; 64];
        for j in 0..5 {
            let xs = &x[64 * j..64 * j + 64];
            let ws = &w[64 * j..64 * j + 64];
            for n in 0..64 {
                u[n] += xs[n] * ws[n];
            }
        }
        // inverse DFT (sign +): use the forward FFT on conjugated data
        for n in 0..64 {
            let t = self.pre[n];
            let j = self.fft.perm(n);
            self.re[j] = u[n] * t.re;
            self.im[j] = -(u[n] * t.im);
        }
        self.fft.run_permuted(&mut self.re, &mut self.im);
        for k in 0..32 {
            let b = Cpx::new(self.re[k], -self.im[k]);
            out[k] = b.mul(self.post[k]);
        }
    }
}

impl Synthesis {
    pub(crate) fn new() -> Synthesis {
        Synthesis {
            v: vec![0.0; 2560],
            pos: 1280,
            win: (0..640).map(|n| window(n) as f32).collect(),
            dct: Dct4::new(64, 1.0 / 64.0),
            a: [0.0; 64],
            b: [0.0; 64],
            c: [0.0; 64],
            d: [0.0; 64],
        }
    }

    pub(crate) fn reset(&mut self) {
        self.v.fill(0.0);
        self.pos = 1280;
    }

    /// One time slot: `x` holds 64 complex subband samples; writes 64 output samples.
    ///
    /// v[n] = 1/64 sum_k Re(X(k) exp(i pi (k + 1/2)(2n - 255) / 128)), n < 128. With
    /// n = 64 + m and a = pi (k + 1/2)(m + 1/2) / 64 the phase is a - pi (k + 1/2), so
    /// v[64 + m] = C[m] + S[m] and v[63 - m] = C[m] - S[m], where C is the DCT-IV of
    /// (-1)^k Im X(k) and S the DST-IV of (-1)^k Re X(k), both scaled by 1/64 (the DST-IV is
    /// a DCT-IV of the reversed input with alternating output signs).
    pub(crate) fn run(&mut self, x: &[Cpx], scale: f32, out: &mut [f32]) {
        if self.pos < 128 {
            self.v.copy_within(self.pos..self.pos + 1152, 2560 - 1152);
            self.pos = 2560 - 1152;
        }
        self.pos -= 128;
        let p = self.pos;
        let x = &x[..64];
        for k in 0..64 {
            let sgn = if k & 1 == 0 { 1.0 } else { -1.0 };
            self.a[k] = sgn * x[k].im;
            // reversed, with (-1)^(63 - j) = -(-1)^j
            self.b[k] = -sgn * x[63 - k].re;
        }
        self.dct.run(&self.a, &mut self.c);
        self.dct.run(&self.b, &mut self.d);
        let v = &mut self.v[p..p + 128];
        for m in 0..64 {
            let s = if m & 1 == 0 { self.d[m] } else { -self.d[m] };
            v[64 + m] = self.c[m] + s;
            v[63 - m] = self.c[m] - s;
        }
        let v = &self.v[p..p + 1280];
        let w = &self.win[..640];
        let mut acc = [0f32; 64];
        for i in 0..5 {
            let v0 = &v[256 * i..256 * i + 64];
            let w0 = &w[128 * i..128 * i + 64];
            let v1 = &v[256 * i + 192..256 * i + 256];
            let w1 = &w[128 * i + 64..128 * i + 128];
            for n in 0..64 {
                acc[n] += v0[n] * w0[n] + v1[n] * w1[n];
            }
        }
        for n in 0..64 {
            out[n] = acc[n] * scale;
        }
    }
}

#[cfg(test)]
// direct reference definitions, written index-style to mirror the formulas
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    /// Synthesis modulation phase term S in exp(i pi (k + 1/2)(2n + S) / 128).
    const SYNTH_PHASE: f64 = -255.0;

    #[test]
    fn window_orientation() {
        // c[0] = 0 and the sign changes fall between 127|128, 255|256, 383|384, 511|512
        // (one position earlier than in the DRM table's own order).
        assert_eq!(window(0), 0.0);
        assert_eq!(window(1), QMF_WINDOW[638]);
        for b in [128, 256, 384, 512] {
            assert!(window(b - 1) * window(b) < 0.0, "sign change at {b}");
            assert!(window(b) * window(b + 1) > 0.0);
        }
    }

    #[test]
    fn passthrough_reconstructs_band_limited_input() {
        // analysis (32 bands) -> synthesis (64 bands, upper half zero) is a 2x interpolator
        // with a delay of 578 output samples; residual ripple / aliasing below -60 dB
        let mut a = Analysis::new();
        let mut sy = Synthesis::new();
        let f = 0.1f64;
        let mut x = [Cpx::default(); 64];
        let mut out = vec![0f32; 0];
        for slot in 0..200 {
            let inp: Vec<f32> = (0..32)
                .map(|n| (2.0 * std::f64::consts::PI * f * (32 * slot + n) as f64).sin() as f32)
                .collect();
            let mut w = [Cpx::default(); 32];
            a.run(&inp, 1.0, &mut w);
            x[..32].copy_from_slice(&w);
            let mut o = [0f32; 64];
            sy.run(&x, 1.0, &mut o);
            out.extend_from_slice(&o);
        }
        let mut max = 0f64;
        for (i, &y) in out.iter().enumerate().skip(4000) {
            let ideal = (2.0 * std::f64::consts::PI * f * (i as f64 - 578.0) / 2.0).sin();
            max = max.max((y as f64 - ideal).abs());
        }
        assert!(max < 1e-3, "max {max}");
    }

    #[test]
    fn analysis_matches_definition() {
        let mut a = Analysis::new();
        let mut s = 7u32;
        let mut rnd = || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s as i32) as f32 / 2147483648.0
        };
        let mut hist = vec![0f64; 320];
        let mut out = vec![Cpx::default(); 32];
        for _slot in 0..12 {
            let inp: Vec<f32> = (0..32).map(|_| rnd()).collect();
            a.run(&inp, 1.0, &mut out);
            hist.copy_within(0..288, 32);
            for n in 0..32 {
                hist[31 - n] = inp[n] as f64;
            }
            let mut u = [0f64; 64];
            for n in 0..64 {
                for j in 0..5 {
                    u[n] += hist[n + 64 * j] * window(2 * (n + 64 * j));
                }
            }
            for k in 0..32 {
                let (mut re, mut im) = (0.0, 0.0);
                for n in 0..64 {
                    let ang =
                        std::f64::consts::PI * (k as f64 + 0.5) * (2.0 * n as f64 - 0.5) / 64.0;
                    re += 2.0 * u[n] * ang.cos();
                    im += 2.0 * u[n] * ang.sin();
                }
                assert!(
                    (re - out[k].re as f64).abs() < 1e-4,
                    "re k={k} {re} {}",
                    out[k].re
                );
                assert!(
                    (im - out[k].im as f64).abs() < 1e-4,
                    "im k={k} {im} {}",
                    out[k].im
                );
            }
        }
    }

    #[test]
    fn synthesis_matches_definition() {
        let mut sy = Synthesis::new();
        let mut s = 9u32;
        let mut rnd = || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s as i32) as f32 / 2147483648.0
        };
        let mut v = vec![0f64; 1280];
        let mut out = vec![0f32; 64];
        for _slot in 0..14 {
            let x: Vec<Cpx> = (0..64).map(|_| Cpx::new(rnd(), rnd())).collect();
            sy.run(&x, 1.0, &mut out);
            v.copy_within(0..1152, 128);
            for n in 0..128 {
                let mut acc = 0.0;
                for k in 0..64 {
                    let ang =
                        std::f64::consts::PI * (k as f64 + 0.5) * (2.0 * n as f64 + SYNTH_PHASE)
                            / 128.0;
                    acc += (x[k].re as f64 * ang.cos() - x[k].im as f64 * ang.sin()) / 64.0;
                }
                v[n] = acc;
            }
            let mut o = [0f64; 64];
            for n in 0..64 {
                for i in 0..5 {
                    o[n] += v[256 * i + n] * window(128 * i + n);
                    o[n] += v[256 * i + 192 + n] * window(128 * i + 64 + n);
                }
            }
            for n in 0..64 {
                assert!(
                    (o[n] - out[n] as f64).abs() < 1e-5,
                    "n={n} {} {}",
                    o[n],
                    out[n]
                );
            }
        }
    }
}
