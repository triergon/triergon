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

/// One stage of the decomposition: `radix` transforms of length `span` combine into
/// transforms of length `radix * span`, with `twiddles` holding `e^{-2πi qk / (radix
/// span)}` for `k < span` and `1 <= q < radix`, `k` major.
#[derive(Clone, Debug)]
struct Stage {
    radix: usize,
    span: usize,
    twiddles: Vec<Cpx>,
}

/// Forward complex FFT, `Z[k] = sum_n z[n] e^{-2πi kn/L}`, by iterative mixed-radix
/// decimation in time: the input is read in digit-reversed order ([`Fft::order`]) and
/// the stages run in place, innermost first.
#[derive(Clone, Debug)]
struct Fft {
    len: usize,
    /// Innermost stage first.
    stages: Vec<Stage>,
    /// The input index each slot of the in-place buffer starts from.
    order: Vec<u32>,
}

impl Fft {
    fn new(len: usize) -> Option<Fft> {
        if len == 0 || u32::try_from(len).is_err() {
            return None;
        }
        // Outermost factor first: radix 4 while it divides, then 2, 3 and 5.
        let mut radices = Vec::new();
        let mut rest = len;
        for radix in [4, 2, 3, 5] {
            while rest.is_multiple_of(radix) && rest > 1 {
                rest /= radix;
                radices.push(radix);
            }
        }
        if rest != 1 {
            return None;
        }
        let mut order = Vec::with_capacity(len);
        digit_reversal(&radices, 0, 1, &mut order);
        let mut stages = Vec::with_capacity(radices.len());
        let mut span = 1;
        for &radix in radices.iter().rev() {
            let block = radix * span;
            let stride = len / block;
            let twiddles = (0..span)
                .flat_map(|k| {
                    (1..radix).map(move |q| {
                        let a = -2.0 * PI * ((q * k * stride) % len) as f64 / len as f64;
                        Cpx::new(a.cos() as f32, a.sin() as f32)
                    })
                })
                .collect();
            stages.push(Stage {
                radix,
                span,
                twiddles,
            });
            span = block;
        }
        Some(Fft { len, stages, order })
    }

    /// Bytes the tables of a transform of length `len` hold.
    fn table_bytes(len: usize) -> usize {
        len.saturating_mul(size_of::<u32>() + size_of::<Cpx>())
    }

    /// The input index slot `slot` of the in-place buffer starts from.
    fn order(&self) -> &[u32] {
        &self.order
    }

    /// Transform `data`, already filled in [`Fft::order`].
    fn transform(&self, data: &mut [Cpx]) {
        let data = &mut data[..self.len];
        // A one-point transform has no stage.
        let Some((first, rest)) = self.stages.split_first() else {
            return;
        };
        // The innermost stage combines single points: its twiddles are all 1.
        if first.radix == 4 {
            for x in data.chunks_exact_mut(4) {
                let (a0, a1) = (x[0].add(x[2]), x[0].sub(x[2]));
                let (a2, a3) = (x[1].add(x[3]), x[1].sub(x[3]).rot_neg());
                x[0] = a0.add(a2);
                x[1] = a1.add(a3);
                x[2] = a0.sub(a2);
                x[3] = a1.sub(a3);
            }
        } else {
            run_stage(data, first);
        }
        for stage in rest {
            run_stage(data, stage);
        }
    }
    /// Transform `input` into `out` (both of the transform's length).
    #[cfg(test)]
    fn run(&self, input: &[Cpx], out: &mut [Cpx]) {
        for (slot, &from) in out.iter_mut().zip(&self.order) {
            *slot = input[from as usize];
        }
        self.transform(out);
    }
}

/// One stage over every block of `data`.
fn run_stage(data: &mut [Cpx], stage: &Stage) {
    let block = stage.radix * stage.span;
    for chunk in data.chunks_exact_mut(block) {
        match stage.radix {
            2 => butterfly2(chunk, stage),
            3 => butterfly3(chunk, stage),
            4 => butterfly4(chunk, stage),
            5 => butterfly5(chunk, stage),
            _ => unreachable!("radices are 2, 3, 4 and 5"),
        }
    }
}

/// Input index of each leaf of the decomposition `radices` (outermost first), in leaf
/// order: the mixed-radix digit reversal.
fn digit_reversal(radices: &[usize], offset: usize, stride: usize, out: &mut Vec<u32>) {
    match radices.split_first() {
        None => out.push(offset as u32),
        Some((&radix, inner)) => {
            for q in 0..radix {
                digit_reversal(inner, offset + q * stride, stride * radix, out);
            }
        }
    }
}

fn butterfly2(chunk: &mut [Cpx], stage: &Stage) {
    let (a, b) = chunk.split_at_mut(stage.span);
    for ((a, b), &w) in a.iter_mut().zip(b.iter_mut()).zip(&stage.twiddles) {
        let t = b.mul(w);
        *b = a.sub(t);
        *a = a.add(t);
    }
}

fn butterfly4(chunk: &mut [Cpx], stage: &Stage) {
    let m = stage.span;
    let (c0, rest) = chunk.split_at_mut(m);
    let (c1, rest) = rest.split_at_mut(m);
    let (c2, c3) = rest.split_at_mut(m);
    let twiddles = stage.twiddles.chunks_exact(3);
    for ((((x0, x1), x2), x3), w) in c0.iter_mut().zip(c1).zip(c2).zip(c3).zip(twiddles) {
        let s0 = *x0;
        let s1 = x1.mul(w[0]);
        let s2 = x2.mul(w[1]);
        let s3 = x3.mul(w[2]);
        let a0 = s0.add(s2);
        let a1 = s0.sub(s2);
        let a2 = s1.add(s3);
        let a3 = s1.sub(s3).rot_neg();
        *x0 = a0.add(a2);
        *x1 = a1.add(a3);
        *x2 = a0.sub(a2);
        *x3 = a1.sub(a3);
    }
}

fn butterfly3(chunk: &mut [Cpx], stage: &Stage) {
    // e^{-2πi/3} = -1/2 - i sqrt(3)/2
    let s = -(3f32.sqrt() / 2.0);
    let m = stage.span;
    let (c0, rest) = chunk.split_at_mut(m);
    let (c1, c2) = rest.split_at_mut(m);
    let twiddles = stage.twiddles.chunks_exact(2);
    for (((x0, x1), x2), w) in c0.iter_mut().zip(c1).zip(c2).zip(twiddles) {
        let a = *x0;
        let b = x1.mul(w[0]);
        let c = x2.mul(w[1]);
        let sum = b.add(c);
        let dif = b.sub(c);
        let mid = a.sub(sum.scale(0.5));
        // i * s * dif
        let rot = Cpx::new(-dif.im * s, dif.re * s);
        *x0 = a.add(sum);
        *x1 = mid.add(rot);
        *x2 = mid.sub(rot);
    }
}

fn butterfly5(chunk: &mut [Cpx], stage: &Stage) {
    // The fifth roots of unity e^{-2πi/5} and e^{-4πi/5}.
    let (w1, w2) = {
        let a = -2.0 * PI / 5.0;
        (
            Cpx::new(a.cos() as f32, a.sin() as f32),
            Cpx::new((2.0 * a).cos() as f32, (2.0 * a).sin() as f32),
        )
    };
    let m = stage.span;
    let (c0, rest) = chunk.split_at_mut(m);
    let (c1, rest) = rest.split_at_mut(m);
    let (c2, rest) = rest.split_at_mut(m);
    let (c3, c4) = rest.split_at_mut(m);
    let twiddles = stage.twiddles.chunks_exact(4);
    for (((((x0, x1), x2), x3), x4), w) in
        c0.iter_mut().zip(c1).zip(c2).zip(c3).zip(c4).zip(twiddles)
    {
        let a = *x0;
        let b = x1.mul(w[0]);
        let c = x2.mul(w[1]);
        let d = x3.mul(w[2]);
        let e = x4.mul(w[3]);
        let s14 = b.add(e);
        let d14 = b.sub(e);
        let s23 = c.add(d);
        let d23 = c.sub(d);
        *x0 = a.add(s14).add(s23);
        // Real parts of the roots weight the sums, imaginary parts the differences.
        let p = a.add(s14.scale(w1.re)).add(s23.scale(w2.re));
        let q = a.add(s14.scale(w2.re)).add(s23.scale(w1.re));
        // i * (w1.im * d14 + w2.im * d23) and i * (w2.im * d14 - w1.im * d23)
        let r = Cpx::new(
            -(d14.im * w1.im + d23.im * w2.im),
            d14.re * w1.im + d23.re * w2.im,
        );
        let t = Cpx::new(
            -(d14.im * w2.im - d23.im * w1.im),
            d14.re * w2.im - d23.re * w1.im,
        );
        *x1 = p.add(r);
        *x4 = p.sub(r);
        *x2 = q.add(t);
        *x3 = q.sub(t);
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
    /// The FFT's in-place buffer.
    work: Vec<Cpx>,
    /// The DCT-IV of the last [`Imdct::imdct`] input.
    dct: Vec<f32>,
}

impl Imdct {
    /// Heap bytes an [`Imdct`] of output length `n` holds, for charging a budget first.
    #[must_use]
    pub fn table_bytes(n: usize) -> usize {
        let q = n / 4;
        // FFT tables, pre/post twiddles and the work buffer; the DCT-IV output; the
        // stage list.
        Fft::table_bytes(q)
            .saturating_add(q.saturating_mul(3 * size_of::<Cpx>()))
            .saturating_add((n / 2).saturating_mul(size_of::<f32>()))
            .saturating_add(16 * size_of::<Stage>())
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
            work: vec![Cpx::default(); q],
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
        let (x, w) = (&x[..k], &mut w[..k]);
        // Pre-twiddle straight into the FFT's digit-reversed order.
        for (slot, &j) in self.work.iter_mut().zip(self.fft.order()) {
            let j = j as usize;
            *slot = Cpx::new(x[2 * j], x[k - 1 - 2 * j]).mul(self.pre[j]);
        }
        self.fft.transform(&mut self.work);
        for (j, (u, &post)) in self.work.iter().zip(&self.post).enumerate() {
            let u = u.mul(post);
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
