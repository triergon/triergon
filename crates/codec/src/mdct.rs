//! The inverse MDCT shared by the Vorbis and Opus (CELT) decoders.
//!
//! [`Imdct`] computes, for `K = N / 2` coefficients `X[k]`,
//!
//! ```text
//! y[n] = scale * sum_{k=0}^{K-1} X[k] * cos(2π/N * (n + 1/2 + N/4) * (k + 1/2)),  0 <= n < N
//! ```
//!
//! which is the IMDCT of Vorbis I section 1.3.2 and RFC 6716 section 4.3.7 before
//! windowing. It runs as a DCT-IV of size `K` through an `N / 4`-point complex FFT with
//! pre- and post-twiddles. The FFT is mixed radix, so `N / 4` may be any product of 2, 3
//! and 5: the power-of-two Vorbis blocks and the 120 · 2^j-sample CELT frames both fit.
//! Windowing and overlap-add stay with each codec.

use core::f64::consts::PI;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Cpx {
    re: f32,
    im: f32,
}

impl Cpx {
    #[inline(always)]
    fn new(re: f32, im: f32) -> Cpx {
        Cpx { re, im }
    }
    #[inline(always)]
    fn mul(self, o: Cpx) -> Cpx {
        Cpx::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }
    #[inline(always)]
    fn add(self, o: Cpx) -> Cpx {
        Cpx::new(self.re + o.re, self.im + o.im)
    }
    #[inline(always)]
    fn sub(self, o: Cpx) -> Cpx {
        Cpx::new(self.re - o.re, self.im - o.im)
    }
    #[inline(always)]
    fn scale(self, s: f32) -> Cpx {
        Cpx::new(self.re * s, self.im * s)
    }
    /// Multiplication by -i.
    #[inline(always)]
    fn rot_neg(self) -> Cpx {
        Cpx::new(self.im, -self.re)
    }
}

/// One stage of the decomposition: `radix` sub-transforms of length `span` are combined.
#[derive(Clone, Copy, Debug)]
struct Stage {
    radix: usize,
    span: usize,
}

/// Forward complex FFT, `Z[k] = sum_n z[n] e^{-2πi kn/L}`, by mixed-radix decimation in
/// time.
#[derive(Clone, Debug)]
struct Fft {
    len: usize,
    stages: Vec<Stage>,
    /// `e^{-2πi j / len}` for `j < len`.
    twiddle: Vec<Cpx>,
}

impl Fft {
    fn new(len: usize) -> Option<Fft> {
        if len == 0 {
            return None;
        }
        let mut stages = Vec::new();
        let mut rest = len;
        for radix in [4, 2, 3, 5] {
            while rest.is_multiple_of(radix) && rest > 1 {
                rest /= radix;
                stages.push(Stage { radix, span: rest });
            }
        }
        if rest != 1 {
            return None;
        }
        let twiddle = (0..len)
            .map(|j| {
                let a = -2.0 * PI * j as f64 / len as f64;
                Cpx::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        Some(Fft {
            len,
            stages,
            twiddle,
        })
    }

    /// Transform `input` (read with `stride`) into `out`.
    fn run(&self, input: &[Cpx], out: &mut [Cpx]) {
        if self.stages.is_empty() {
            out[0] = input[0];
            return;
        }
        self.work(out, input, 0, 1, 0);
    }

    fn work(&self, out: &mut [Cpx], input: &[Cpx], offset: usize, stride: usize, stage: usize) {
        let Stage { radix, span } = self.stages[stage];
        if span == 1 {
            for (q, o) in out[..radix].iter_mut().enumerate() {
                *o = input[offset + q * stride];
            }
        } else {
            for q in 0..radix {
                self.work(
                    &mut out[q * span..(q + 1) * span],
                    input,
                    offset + q * stride,
                    stride * radix,
                    stage + 1,
                );
            }
        }
        // `stride` steps through the twiddle table for the sub-transform length.
        let out = &mut out[..radix * span];
        match radix {
            2 => self.butterfly2(out, stride, span),
            3 => self.butterfly3(out, stride, span),
            4 => self.butterfly4(out, stride, span),
            5 => self.butterfly5(out, stride, span),
            _ => unreachable!("radices are 2, 3, 4 and 5"),
        }
    }

    #[inline(always)]
    fn tw(&self, index: usize) -> Cpx {
        self.twiddle[index % self.len]
    }

    fn butterfly2(&self, out: &mut [Cpx], stride: usize, m: usize) {
        let (a, b) = out.split_at_mut(m);
        for k in 0..m {
            let t = b[k].mul(self.twiddle[k * stride]);
            b[k] = a[k].sub(t);
            a[k] = a[k].add(t);
        }
    }

    fn butterfly4(&self, out: &mut [Cpx], stride: usize, m: usize) {
        for k in 0..m {
            let s0 = out[k];
            let s1 = out[k + m].mul(self.twiddle[k * stride]);
            let s2 = out[k + 2 * m].mul(self.twiddle[2 * k * stride]);
            let s3 = out[k + 3 * m].mul(self.twiddle[3 * k * stride]);
            let a0 = s0.add(s2);
            let a1 = s0.sub(s2);
            let a2 = s1.add(s3);
            let a3 = s1.sub(s3).rot_neg();
            out[k] = a0.add(a2);
            out[k + m] = a1.add(a3);
            out[k + 2 * m] = a0.sub(a2);
            out[k + 3 * m] = a1.sub(a3);
        }
    }

    fn butterfly3(&self, out: &mut [Cpx], stride: usize, m: usize) {
        // e^{-2πi/3} = -1/2 - i sqrt(3)/2
        let s = -(3f32.sqrt() / 2.0);
        for k in 0..m {
            let x0 = out[k];
            let x1 = out[k + m].mul(self.twiddle[k * stride]);
            let x2 = out[k + 2 * m].mul(self.twiddle[2 * k * stride]);
            let sum = x1.add(x2);
            let dif = x1.sub(x2);
            let mid = x0.sub(sum.scale(0.5));
            // i * s * dif
            let rot = Cpx::new(-dif.im * s, dif.re * s);
            out[k] = x0.add(sum);
            out[k + m] = mid.add(rot);
            out[k + 2 * m] = mid.sub(rot);
        }
    }

    fn butterfly5(&self, out: &mut [Cpx], stride: usize, m: usize) {
        let w1 = self.tw(self.len / 5);
        let w2 = self.tw(2 * self.len / 5);
        for k in 0..m {
            let x0 = out[k];
            let x1 = out[k + m].mul(self.twiddle[k * stride]);
            let x2 = out[k + 2 * m].mul(self.twiddle[2 * k * stride]);
            let x3 = out[k + 3 * m].mul(self.twiddle[3 * k * stride]);
            let x4 = out[k + 4 * m].mul(self.twiddle[4 * k * stride]);
            let s14 = x1.add(x4);
            let d14 = x1.sub(x4);
            let s23 = x2.add(x3);
            let d23 = x2.sub(x3);
            out[k] = x0.add(s14).add(s23);
            // Real parts of the fifth roots weight the sums, imaginary parts the differences.
            let a = x0.add(s14.scale(w1.re)).add(s23.scale(w2.re));
            let b = x0.add(s14.scale(w2.re)).add(s23.scale(w1.re));
            // i * (w1.im * d14 + w2.im * d23) and i * (w2.im * d14 - w1.im * d23)
            let c = Cpx::new(
                -(d14.im * w1.im + d23.im * w2.im),
                d14.re * w1.im + d23.re * w2.im,
            );
            let d = Cpx::new(
                -(d14.im * w2.im - d23.im * w1.im),
                d14.re * w2.im - d23.re * w1.im,
            );
            out[k + m] = a.add(c);
            out[k + 4 * m] = a.sub(c);
            out[k + 2 * m] = b.add(d);
            out[k + 3 * m] = b.sub(d);
        }
    }
}

/// Inverse MDCT of one block size; see the [module documentation](self).
#[derive(Clone, Debug)]
pub struct Imdct {
    /// Output length `N`.
    len: usize,
    fft: Fft,
    /// `e^{-iπ(j + 1/8) / K}` for `j < N / 4`, applied before the FFT.
    pre: Vec<Cpx>,
    /// `scale` times the same, applied after it.
    post: Vec<Cpx>,
    input: Vec<Cpx>,
    output: Vec<Cpx>,
    /// The DCT-IV of the last [`Imdct::imdct`] input.
    dct: Vec<f32>,
}

impl Imdct {
    /// Heap bytes an [`Imdct`] of output length `n` holds, for charging a budget first.
    #[must_use]
    pub fn table_bytes(n: usize) -> usize {
        let q = n / 4;
        // FFT twiddles, pre/post twiddles and two work vectors; the DCT-IV output; the
        // stage list.
        q.saturating_mul(5 * size_of::<Cpx>())
            .saturating_add((n / 2).saturating_mul(size_of::<f32>()))
            .saturating_add(32 * size_of::<Stage>())
    }

    /// The transform producing `n` outputs from `n / 2` coefficients, scaled by `scale`.
    /// `None` unless `n` is a multiple of 4 and `n / 4` a product of 2, 3 and 5.
    #[must_use]
    pub fn new(n: usize, scale: f64) -> Option<Imdct> {
        if n < 4 || !n.is_multiple_of(4) {
            return None;
        }
        let q = n / 4;
        let k = n / 2;
        let fft = Fft::new(q)?;
        let twiddle = |j: usize, s: f64| {
            let a = -PI * (j as f64 + 0.125) / k as f64;
            Cpx::new((s * a.cos()) as f32, (s * a.sin()) as f32)
        };
        Some(Imdct {
            len: n,
            fft,
            pre: (0..q).map(|j| twiddle(j, 1.0)).collect(),
            post: (0..q).map(|j| twiddle(j, scale)).collect(),
            input: vec![Cpx::default(); q],
            output: vec![Cpx::default(); q],
            dct: vec![0.0; k],
        })
    }

    /// Output length `N`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always false: a transform has at least four outputs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The DCT-IV of size `K = N / 2`:
    /// `w[m] = scale * sum_k x[k] cos(π/K (m + 1/2)(k + 1/2))`.
    ///
    /// # Panics
    /// If `x` or `w` is shorter than `K`.
    pub fn dct4(&mut self, x: &[f32], w: &mut [f32]) {
        let k = self.len / 2;
        let q = self.len / 4;
        let (x, w) = (&x[..k], &mut w[..k]);
        for j in 0..q {
            let z = Cpx::new(x[2 * j], x[k - 1 - 2 * j]);
            self.input[j] = z.mul(self.pre[j]);
        }
        self.fft.run(&self.input, &mut self.output);
        for j in 0..q {
            let u = self.output[j].mul(self.post[j]);
            w[2 * j] = u.re;
            w[k - 1 - 2 * j] = -u.im;
        }
    }

    /// The full inverse MDCT: `N` outputs `y` from `K = N / 2` coefficients `x`.
    ///
    /// # Panics
    /// If `x` is shorter than `K` or `y` shorter than `N`.
    pub fn imdct(&mut self, x: &[f32], y: &mut [f32]) {
        let k = self.len / 2;
        let half = k / 2;
        let mut dct = core::mem::take(&mut self.dct);
        self.dct4(x, &mut dct);
        let y = &mut y[..self.len];
        // y[n] = w[n + K/2] for n < K/2; -w[3K/2 - 1 - n] for K/2 <= n < 3K/2;
        // -w[n - 3K/2] after.
        y[..half].copy_from_slice(&dct[half..]);
        for n in half..3 * half {
            y[n] = -dct[3 * half - 1 - n];
        }
        for n in 3 * half..2 * k {
            y[n] = -dct[n - 3 * half];
        }
        self.dct = dct;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(state: &mut u32) -> f32 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    }

    #[test]
    fn fft_matches_the_definition_for_every_radix() {
        let mut seed = 7;
        for len in [
            1, 2, 3, 4, 5, 6, 8, 12, 15, 16, 30, 60, 64, 120, 240, 480, 512,
        ] {
            let fft = Fft::new(len).unwrap();
            let input: Vec<Cpx> = (0..len)
                .map(|_| Cpx::new(lcg(&mut seed), lcg(&mut seed)))
                .collect();
            let mut out = vec![Cpx::default(); len];
            fft.run(&input, &mut out);
            for (k, got) in out.iter().enumerate() {
                let (mut re, mut im) = (0f64, 0f64);
                for (n, z) in input.iter().enumerate() {
                    let a = -2.0 * PI * (k * n) as f64 / len as f64;
                    re += f64::from(z.re) * a.cos() - f64::from(z.im) * a.sin();
                    im += f64::from(z.re) * a.sin() + f64::from(z.im) * a.cos();
                }
                let tolerance = 2e-5 * (len as f64).sqrt();
                assert!(
                    (f64::from(got.re) - re).abs() < tolerance
                        && (f64::from(got.im) - im).abs() < tolerance,
                    "len {len} bin {k}: {got:?} vs ({re}, {im})"
                );
            }
        }
        assert!(Fft::new(7).is_none());
    }

    #[test]
    fn imdct_matches_the_definition() {
        let mut seed = 11;
        for n in [8, 16, 64, 240, 480, 960, 1920, 256, 2048] {
            let k = n / 2;
            let x: Vec<f32> = (0..k).map(|_| lcg(&mut seed)).collect();
            let mut imdct = Imdct::new(n, -0.5).unwrap();
            let mut y = vec![0f32; n];
            imdct.imdct(&x, &mut y);
            for (i, got) in y.iter().enumerate() {
                let want: f64 = -0.5
                    * x.iter()
                        .enumerate()
                        .map(|(j, &v)| {
                            f64::from(v)
                                * (2.0 * PI / n as f64
                                    * (i as f64 + 0.5 + n as f64 / 4.0)
                                    * (j as f64 + 0.5))
                                    .cos()
                        })
                        .sum::<f64>();
                assert!(
                    (f64::from(*got) - want).abs() < 2e-5 * (n as f64).sqrt(),
                    "n {n} sample {i}: {got} vs {want}"
                );
            }
        }
        assert!(Imdct::new(28, 1.0).is_none());
        assert!(Imdct::new(30, 1.0).is_none());
    }
}
