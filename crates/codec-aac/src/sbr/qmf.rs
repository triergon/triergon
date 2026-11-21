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
//! synthesis), here at the decoder's band counts. Both modulations are DCT-IVs and DST-IVs of
//! half the input length after folding the input by the basis functions' symmetries, the
//! standard reduction of a modulated filter bank to a DCT-IV (H. S. Malvar, *Signal
//! Processing with Lapped Transforms*, Artech House 1992, ch. 2 and 4); a DST-IV is a DCT-IV
//! of the reversed input with alternating output signs. The derivations are in the comments
//! of `Analysis::run` and `Synthesis::run`; the unit tests check both against the direct
//! definitions.

use super::kernels;
use super::tables::QMF_WINDOW;
use crate::fft::{Cpx, Dct4};
use std::sync::OnceLock;
use triergon_codec::{Budget, BudgetVec, StorageError};

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

/// Windows and the analysis rotation, shared by every bank.
#[derive(Debug)]
pub(crate) struct Tables {
    /// analysis window c[2n], n < 320
    analysis_win: [f32; 320],
    /// synthesis window c[n], n < 640
    synthesis_win: [f32; 640],
    /// The analysis output rotation of band k, in the order the DCT-IVs return their
    /// outputs (band 2j at j, band 31 - 2j at 16 + j): cos and sin of
    /// phi_k = 3 pi (2k + 1) / 256, then both times (-1)^k for the DST-IV's signs.
    rotation: [[f32; 4]; 32],
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let rotation = |k: usize| {
            let phi = 3.0 * std::f64::consts::PI * (2 * k + 1) as f64 / 256.0;
            let sign = if k.is_multiple_of(2) { 1.0 } else { -1.0 };
            let (s, c) = phi.sin_cos();
            [c as f32, s as f32, (sign * c) as f32, (sign * s) as f32]
        };
        Tables {
            analysis_win: std::array::from_fn(|n| window(2 * n) as f32),
            synthesis_win: std::array::from_fn(|n| window(n) as f32),
            rotation: std::array::from_fn(|j| {
                if j < 16 {
                    rotation(2 * j)
                } else {
                    rotation(31 - 2 * (j - 16))
                }
            }),
        }
    })
}

#[derive(Debug)]
pub(crate) struct Analysis {
    /// Input history, newest first: the 320 samples x[0..320] live at x[pos..pos + 320]
    /// of a double-length buffer so no per-slot shifting is needed.
    x: BudgetVec<f32>,
    pos: usize,
    t: &'static Tables,
}

#[derive(Debug)]
pub(crate) struct Synthesis {
    v: BudgetVec<f32>,
    /// write position of the circular v buffer (in units of 128)
    pos: usize,
    t: &'static Tables,
    a: [f32; 64],
    b: [f32; 64],
    /// The two DCT-IVs' outputs, split as `Dct4::run_split` returns them.
    c: [[f32; 32]; 2],
    d: [[f32; 32]; 2],
}

/// The `N` values at the start of `s`; panics as indexing does when `s` is shorter.
#[inline(always)]
fn head<const N: usize>(s: &[f32]) -> &[f32; N] {
    &s.as_chunks::<N>().0[0]
}

/// The `N` values at the start of `s`; panics as indexing does when `s` is shorter.
#[inline(always)]
fn head_mut<const N: usize>(s: &mut [f32]) -> &mut [f32; N] {
    &mut s.as_chunks_mut::<N>().0[0]
}

/// The modulations of the banks: DCT-IVs with work space, which the decoder keeps one pair
/// of for all its banks.
#[derive(Debug)]
pub(crate) struct Modulation {
    /// 32 points, scaled by 2
    analysis: Dct4,
    /// 64 points, scaled by 1/64
    synthesis: Dct4,
}

impl Modulation {
    pub(crate) fn new() -> Modulation {
        Modulation {
            analysis: Dct4::new(32, 2.0),
            synthesis: Dct4::new(64, 1.0 / 64.0),
        }
    }
}

impl Analysis {
    /// An analysis bank, or with `used` false an empty one that never runs.
    pub(crate) fn new(budget: &Budget, used: bool) -> Result<Analysis, StorageError> {
        Ok(Analysis {
            x: BudgetVec::try_filled(budget, 0.0, if used { 640 } else { 0 })?,
            pos: 320,
            t: tables(),
        })
    }

    pub(crate) fn reset(&mut self) {
        self.x.fill(0.0);
        self.pos = 320;
    }

    /// Process 32 new input samples (oldest first) into 32 complex subband samples.
    ///
    /// X(k) = 2 sum_{n<64} u[n] exp(i pi/32 (k + 1/2)(n - 1/4)), k < 32. With
    /// theta = pi/32 (k + 1/2)(n + 1/2) the phase is theta - phi_k, phi_k = 3 pi (2k + 1)/256,
    /// and n -> 63 - n turns theta into 2 pi (k + 1/2) - theta, which flips the cosine's sign
    /// and keeps the sine's. So X(k) = exp(-i phi_k) (C(k) + i S(k)), where C is the DCT-IV
    /// of d[n] = u[n] - u[63 - n] and S the DST-IV of u[n] + u[63 - n], n < 32, both scaled
    /// by 2.
    pub(crate) fn run(
        &mut self,
        m: &mut Modulation,
        input: &[f32; 32],
        scale: f32,
        out: &mut [Cpx; 32],
    ) {
        if self.pos < 32 {
            self.x.copy_within(self.pos..self.pos + 288, 640 - 288);
            self.pos = 640 - 288;
        }
        self.pos -= 32;
        let x = head_mut::<320>(&mut self.x[self.pos..]);
        let u = kernels::analysis_window(x, input, scale, &self.t.analysis_win);
        let (d, r) = kernels::analysis_fold(&u);
        let (mut c, mut s) = ([0f32; 32], [0f32; 32]);
        let (ce, co) = c.split_at_mut(16);
        m.analysis.run_split(&d, ce, co);
        let (se, so) = s.split_at_mut(16);
        m.analysis.run_split(&r, se, so);
        // re = C cos phi + S sin phi, im = S cos phi - C sin phi, S(k) = (-1)^k DCT-IV(r)(k)
        let rotate = |c: f32, s: f32, [cos, sin, scos, ssin]: [f32; 4]| {
            Cpx::new(c * cos + s * ssin, s * scos - c * sin)
        };
        let t = &self.t.rotation;
        for j in 0..16 {
            out[2 * j] = rotate(c[j], s[j], t[j]);
            out[31 - 2 * j] = rotate(c[16 + j], s[16 + j], t[16 + j]);
        }
    }
}

impl Synthesis {
    pub(crate) fn new(budget: &Budget) -> Result<Synthesis, StorageError> {
        Ok(Synthesis {
            v: BudgetVec::try_filled(budget, 0.0, 2560)?,
            pos: 1280,
            t: tables(),
            a: [0.0; 64],
            b: [0.0; 64],
            c: [[0.0; 32]; 2],
            d: [[0.0; 32]; 2],
        })
    }

    pub(crate) fn reset(&mut self) {
        self.v.fill(0.0);
        self.pos = 1280;
        for scratch in [&mut self.a, &mut self.b] {
            *scratch = [0.0; 64];
        }
        for scratch in [&mut self.c, &mut self.d] {
            *scratch = [[0.0; 32]; 2];
        }
    }

    /// One time slot: `x` holds 64 complex subband samples; writes 64 output samples.
    ///
    /// v[n] = 1/64 sum_k Re(X(k) exp(i pi (k + 1/2)(2n - 255) / 128)), n < 128. With
    /// n = 64 + m and a = pi (k + 1/2)(m + 1/2) / 64 the phase is a - pi (k + 1/2), so
    /// v[64 + m] = C[m] + S[m] and v[63 - m] = C[m] - S[m], where C is the DCT-IV of
    /// (-1)^k Im X(k) and S the DST-IV of (-1)^k Re X(k), both scaled by 1/64 (the DST-IV is
    /// a DCT-IV of the reversed input with alternating output signs).
    pub(crate) fn run(
        &mut self,
        m: &mut Modulation,
        x: &[Cpx; 64],
        scale: f32,
        out: &mut [f32; 64],
    ) {
        if self.pos < 128 {
            self.v.copy_within(self.pos..self.pos + 1152, 2560 - 1152);
            self.pos = 2560 - 1152;
        }
        self.pos -= 128;
        let p = self.pos;
        kernels::synthesis_inputs(x, &mut self.a, &mut self.b);
        // C and S split as `Dct4::run_split` returns them: c[2k] = ce[k], c[63 - 2k] = co[k]
        let [ce, co] = &mut self.c;
        m.synthesis.run_split(&self.a, ce, co);
        let [se, so] = &mut self.d;
        m.synthesis.run_split(&self.b, se, so);
        // v[64 + m] = C[m] + (-1)^m S[m] and v[63 - m] = C[m] - (-1)^m S[m], for the even
        // m = 2k and the odd m = 63 - 2k
        kernels::synthesis_butterflies(ce, co, se, so, head_mut::<128>(&mut self.v[p..]));
        // Each output's sum in a register over its ten taps, in tap order, four outputs a
        // block.
        let v = head::<1280>(&self.v[p..]);
        kernels::synthesis_window(v, &self.t.synthesis_win, scale, out);
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
        let budget = Budget::new(1 << 20).unwrap();
        // analysis (32 bands) -> synthesis (64 bands, upper half zero) is a 2x interpolator
        // with a delay of 578 output samples; residual ripple / aliasing below -60 dB
        let mut a = Analysis::new(&budget, true).unwrap();
        let mut sy = Synthesis::new(&budget).unwrap();
        let mut m = Modulation::new();
        let f = 0.1f64;
        let mut x = [Cpx::default(); 64];
        let mut out = vec![0f32; 0];
        for slot in 0..200 {
            let inp: [f32; 32] = std::array::from_fn(|n| {
                (2.0 * std::f64::consts::PI * f * (32 * slot + n) as f64).sin() as f32
            });
            let mut w = [Cpx::default(); 32];
            a.run(&mut m, &inp, 1.0, &mut w);
            x[..32].copy_from_slice(&w);
            let mut o = [0f32; 64];
            sy.run(&mut m, &x, 1.0, &mut o);
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
        let budget = Budget::new(1 << 20).unwrap();
        let mut a = Analysis::new(&budget, true).unwrap();
        let mut m = Modulation::new();
        let mut s = 7u32;
        let mut rnd = || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s as i32) as f32 / 2147483648.0
        };
        let mut hist = vec![0f64; 320];
        let mut out = [Cpx::default(); 32];
        for _slot in 0..12 {
            let inp: [f32; 32] = std::array::from_fn(|_| rnd());
            a.run(&mut m, &inp, 1.0, &mut out);
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
        let budget = Budget::new(1 << 20).unwrap();
        let mut sy = Synthesis::new(&budget).unwrap();
        let mut m = Modulation::new();
        let mut s = 9u32;
        let mut rnd = || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s as i32) as f32 / 2147483648.0
        };
        let mut v = vec![0f64; 1280];
        let mut out = [0f32; 64];
        for _slot in 0..14 {
            let x: [Cpx; 64] = std::array::from_fn(|_| Cpx::new(rnd(), rnd()));
            sy.run(&mut m, &x, 1.0, &mut out);
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
