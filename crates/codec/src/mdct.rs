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

/// `(a + ib)(c + id)`, in the order every stage uses.
#[inline(always)]
fn mul(a: f32, b: f32, c: f32, d: f32) -> (f32, f32) {
    (a * c - b * d, a * d + b * c)
}

/// One stage of the decomposition: `radix` transforms of length `span` combine into
/// transforms of length `radix * span`. The twiddles `e^{-2πi qk / (radix span)}` for
/// `1 <= q < radix` and `k < span` are stored `q` major in split real and imaginary
/// parts, so a stage reads them as contiguous runs.
#[derive(Clone, Debug)]
struct Stage {
    radix: usize,
    span: usize,
    re: Vec<f32>,
    im: Vec<f32>,
}

impl Stage {
    /// The twiddles of `q` (1 to `radix - 1`), real and imaginary.
    #[inline(always)]
    fn twiddles(&self, q: usize) -> (&[f32], &[f32]) {
        let start = (q - 1) * self.span;
        (
            &self.re[start..][..self.span],
            &self.im[start..][..self.span],
        )
    }
}

/// Forward complex FFT, `Z[k] = sum_n z[n] e^{-2πi kn/L}`, by iterative mixed-radix
/// decimation in time over split real and imaginary planes: the input is read in
/// digit-reversed order ([`Fft::order`]) and the stages run in place, innermost first.
/// Every loop runs over contiguous runs of one plane, so it vectorises.
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
        // Outermost factor first. Radix 4 runs innermost and 3 and 5 outermost, so every
        // stage after the first spans a multiple of four points and its loop vectorises.
        let mut rest = len;
        let mut count = |radix: usize| {
            let mut n = 0;
            while rest.is_multiple_of(radix) && rest > 1 {
                rest /= radix;
                n += 1;
            }
            n
        };
        let (fours, twos, threes, fives) = (count(4), count(2), count(3), count(5));
        if rest != 1 {
            return None;
        }
        let radices: Vec<usize> = [(5, fives), (3, threes), (2, twos), (4, fours)]
            .into_iter()
            .flat_map(|(radix, n)| core::iter::repeat_n(radix, n))
            .collect();
        let mut order = Vec::with_capacity(len);
        digit_reversal(&radices, 0, 1, &mut order);
        let mut stages = Vec::with_capacity(radices.len());
        let mut span = 1;
        for &radix in radices.iter().rev() {
            let stride = len / (radix * span);
            let angle =
                |q: usize, k: usize| -2.0 * PI * ((q * k * stride) % len) as f64 / len as f64;
            let pairs = || (1..radix).flat_map(|q| (0..span).map(move |k| angle(q, k)));
            stages.push(Stage {
                radix,
                span,
                re: pairs().map(|a| a.cos() as f32).collect(),
                im: pairs().map(|a| a.sin() as f32).collect(),
            });
            span *= radix;
        }
        Some(Fft { len, stages, order })
    }

    /// Bytes the tables of a transform of length `len` hold.
    fn table_bytes(len: usize) -> usize {
        len.saturating_mul(size_of::<u32>() + 2 * size_of::<f32>())
    }

    /// The input index each slot of the in-place buffer starts from.
    fn order(&self) -> &[u32] {
        &self.order
    }

    /// Transform the planes `re` and `im`, already filled in [`Fft::order`].
    #[cfg(test)]
    fn transform(&self, re: &mut [f32], im: &mut [f32]) {
        let (re, im) = (&mut re[..self.len], &mut im[..self.len]);
        // A one-point transform has no stage.
        let Some(first) = self.stages.first() else {
            return;
        };
        if first.radix == 4 {
            for (r, i) in re.chunks_exact_mut(4).zip(im.chunks_exact_mut(4)) {
                let x = |j: usize| (r[j], i[j]);
                let y = leaf4([x(0), x(1), x(2), x(3)]);
                for (j, (yr, yi)) in y.into_iter().enumerate() {
                    (r[j], i[j]) = (yr, yi);
                }
            }
        } else {
            run_stage(re, im, first);
        }
        self.transform_after_first(re, im);
    }

    /// Whether the innermost stage is radix 4, which [`leaf4`] computes as the caller
    /// fills the planes.
    fn leaf_is_radix4(&self) -> bool {
        self.stages.first().is_some_and(|stage| stage.radix == 4)
    }

    /// Every stage after the innermost one.
    fn transform_after_first(&self, re: &mut [f32], im: &mut [f32]) {
        let (re, im) = (&mut re[..self.len], &mut im[..self.len]);
        for stage in self.stages.iter().skip(1) {
            run_stage(re, im, stage);
        }
    }
}

/// The innermost radix-4 stage on four points: it combines single points, so its
/// twiddles are all 1.
#[inline(always)]
fn leaf4(x: [(f32, f32); 4]) -> [(f32, f32); 4] {
    let [(r0, i0), (r1, i1), (r2, i2), (r3, i3)] = x;
    let (a0r, a0i, a1r, a1i) = (r0 + r2, i0 + i2, r0 - r2, i0 - i2);
    let (a2r, a2i) = (r1 + r3, i1 + i3);
    // (x1 - x3) times -i.
    let (a3r, a3i) = (i1 - i3, -(r1 - r3));
    [
        (a0r + a2r, a0i + a2i),
        (a1r + a3r, a1i + a3i),
        (a0r - a2r, a0i - a2i),
        (a1r - a3r, a1i - a3i),
    ]
}

impl Fft {}

/// One stage over every block of the planes.
fn run_stage(re: &mut [f32], im: &mut [f32], stage: &Stage) {
    let block = stage.radix * stage.span;
    let blocks = re.chunks_exact_mut(block).zip(im.chunks_exact_mut(block));
    for (r, i) in blocks {
        match stage.radix {
            2 => butterfly2(r, i, stage),
            3 => butterfly3(r, i, stage),
            4 => butterfly4(r, i, stage),
            5 => butterfly5(r, i, stage),
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

/// The `parts` equal runs of one block's plane.
#[inline(always)]
fn runs<const N: usize>(plane: &mut [f32], span: usize) -> [&mut [f32]; N] {
    let mut rest = plane;
    core::array::from_fn(|_| {
        let (run, tail) = core::mem::take(&mut rest).split_at_mut(span);
        rest = tail;
        &mut run[..span]
    })
}

fn butterfly2(re: &mut [f32], im: &mut [f32], stage: &Stage) {
    let m = stage.span;
    let [r0, r1] = runs::<2>(re, m);
    let [i0, i1] = runs::<2>(im, m);
    radix2([r0, i0, r1, i1], stage.twiddles(1));
}

/// The radix-2 butterflies of one block. Each run is its own argument, so the compiler
/// knows they do not overlap and vectorises the loop.
#[inline(never)]
fn radix2([r0, i0, r1, i1]: [&mut [f32]; 4], (wr, wi): (&[f32], &[f32])) {
    let m = r0.len();
    let (i0, r1, i1, wr, wi) = (&mut i0[..m], &mut r1[..m], &mut i1[..m], &wr[..m], &wi[..m]);
    for k in 0..m {
        let (tr, ti) = mul(r1[k], i1[k], wr[k], wi[k]);
        r1[k] = r0[k] - tr;
        i1[k] = i0[k] - ti;
        r0[k] += tr;
        i0[k] += ti;
    }
}

fn butterfly4(re: &mut [f32], im: &mut [f32], stage: &Stage) {
    let m = stage.span;
    let [r0, r1, r2, r3] = runs::<4>(re, m);
    let [i0, i1, i2, i3] = runs::<4>(im, m);
    radix4(
        [r0, i0, r1, i1, r2, i2, r3, i3],
        [stage.twiddles(1), stage.twiddles(2), stage.twiddles(3)],
    );
}

/// The radix-4 butterflies of one block; see [`radix2`].
#[inline(never)]
fn radix4(
    [r0, i0, r1, i1, r2, i2, r3, i3]: [&mut [f32]; 8],
    [(w1r, w1i), (w2r, w2i), (w3r, w3i)]: [(&[f32], &[f32]); 3],
) {
    let m = r0.len();
    let (i0, r1, i1, r2, i2, r3, i3) = (
        &mut i0[..m],
        &mut r1[..m],
        &mut i1[..m],
        &mut r2[..m],
        &mut i2[..m],
        &mut r3[..m],
        &mut i3[..m],
    );
    let (w1r, w1i, w2r, w2i, w3r, w3i) = (
        &w1r[..m],
        &w1i[..m],
        &w2r[..m],
        &w2i[..m],
        &w3r[..m],
        &w3i[..m],
    );
    for k in 0..m {
        let (s0r, s0i) = (r0[k], i0[k]);
        let (s1r, s1i) = mul(r1[k], i1[k], w1r[k], w1i[k]);
        let (s2r, s2i) = mul(r2[k], i2[k], w2r[k], w2i[k]);
        let (s3r, s3i) = mul(r3[k], i3[k], w3r[k], w3i[k]);
        let (a0r, a0i) = (s0r + s2r, s0i + s2i);
        let (a1r, a1i) = (s0r - s2r, s0i - s2i);
        let (a2r, a2i) = (s1r + s3r, s1i + s3i);
        // (s1 - s3) times -i.
        let (a3r, a3i) = (s1i - s3i, -(s1r - s3r));
        r0[k] = a0r + a2r;
        i0[k] = a0i + a2i;
        r1[k] = a1r + a3r;
        i1[k] = a1i + a3i;
        r2[k] = a0r - a2r;
        i2[k] = a0i - a2i;
        r3[k] = a1r - a3r;
        i3[k] = a1i - a3i;
    }
}

fn butterfly3(re: &mut [f32], im: &mut [f32], stage: &Stage) {
    let m = stage.span;
    let [r0, r1, r2] = runs::<3>(re, m);
    let [i0, i1, i2] = runs::<3>(im, m);
    radix3(
        [r0, i0, r1, i1, r2, i2],
        [stage.twiddles(1), stage.twiddles(2)],
    );
}

/// The radix-3 butterflies of one block; see [`radix2`].
#[inline(never)]
fn radix3(
    [r0, i0, r1, i1, r2, i2]: [&mut [f32]; 6],
    [(w1r, w1i), (w2r, w2i)]: [(&[f32], &[f32]); 2],
) {
    // e^{-2πi/3} = -1/2 - i sqrt(3)/2
    let s = -(3f32.sqrt() / 2.0);
    let m = r0.len();
    let (i0, r1, i1, r2, i2) = (
        &mut i0[..m],
        &mut r1[..m],
        &mut i1[..m],
        &mut r2[..m],
        &mut i2[..m],
    );
    let (w1r, w1i, w2r, w2i) = (&w1r[..m], &w1i[..m], &w2r[..m], &w2i[..m]);
    for k in 0..m {
        let (ar, ai) = (r0[k], i0[k]);
        let (br, bi) = mul(r1[k], i1[k], w1r[k], w1i[k]);
        let (cr, ci) = mul(r2[k], i2[k], w2r[k], w2i[k]);
        let (sumr, sumi) = (br + cr, bi + ci);
        let (difr, difi) = (br - cr, bi - ci);
        let (midr, midi) = (ar - sumr * 0.5, ai - sumi * 0.5);
        // i * s * dif
        let (rotr, roti) = (-difi * s, difr * s);
        r0[k] = ar + sumr;
        i0[k] = ai + sumi;
        r1[k] = midr + rotr;
        i1[k] = midi + roti;
        r2[k] = midr - rotr;
        i2[k] = midi - roti;
    }
}

fn butterfly5(re: &mut [f32], im: &mut [f32], stage: &Stage) {
    let m = stage.span;
    let [r0, r1, r2, r3, r4] = runs::<5>(re, m);
    let [i0, i1, i2, i3, i4] = runs::<5>(im, m);
    radix5(
        [r0, i0, r1, i1, r2, i2, r3, i3, r4, i4],
        [
            stage.twiddles(1),
            stage.twiddles(2),
            stage.twiddles(3),
            stage.twiddles(4),
        ],
    );
}

/// The radix-5 butterflies of one block; see [`radix2`].
#[inline(never)]
fn radix5(
    [r0, i0, r1, i1, r2, i2, r3, i3, r4, i4]: [&mut [f32]; 10],
    [t1, t2, t3, t4]: [(&[f32], &[f32]); 4],
) {
    // The fifth roots of unity e^{-2πi/5} and e^{-4πi/5}.
    let a = -2.0 * PI / 5.0;
    let (w1r, w1i) = (a.cos() as f32, a.sin() as f32);
    let (w2r, w2i) = ((2.0 * a).cos() as f32, (2.0 * a).sin() as f32);
    let m = r0.len();
    let (i0, r1, i1, r2, i2, r3, i3, r4, i4) = (
        &mut i0[..m],
        &mut r1[..m],
        &mut i1[..m],
        &mut r2[..m],
        &mut i2[..m],
        &mut r3[..m],
        &mut i3[..m],
        &mut r4[..m],
        &mut i4[..m],
    );
    let t = [t1, t2, t3, t4].map(|(re, im)| (&re[..m], &im[..m]));
    for k in 0..m {
        let (ar, ai) = (r0[k], i0[k]);
        let (br, bi) = mul(r1[k], i1[k], t[0].0[k], t[0].1[k]);
        let (cr, ci) = mul(r2[k], i2[k], t[1].0[k], t[1].1[k]);
        let (dr, di) = mul(r3[k], i3[k], t[2].0[k], t[2].1[k]);
        let (er, ei) = mul(r4[k], i4[k], t[3].0[k], t[3].1[k]);
        let (s14r, s14i) = (br + er, bi + ei);
        let (d14r, d14i) = (br - er, bi - ei);
        let (s23r, s23i) = (cr + dr, ci + di);
        let (d23r, d23i) = (cr - dr, ci - di);
        r0[k] = ar + s14r + s23r;
        i0[k] = ai + s14i + s23i;
        // Real parts of the roots weight the sums, imaginary parts the differences.
        let (pr, pi) = (ar + s14r * w1r + s23r * w2r, ai + s14i * w1r + s23i * w2r);
        let (qr, qi) = (ar + s14r * w2r + s23r * w1r, ai + s14i * w2r + s23i * w1r);
        // i * (w1.im * d14 + w2.im * d23) and i * (w2.im * d14 - w1.im * d23)
        let (ur, ui) = (-(d14i * w1i + d23i * w2i), d14r * w1i + d23r * w2i);
        let (vr, vi) = (-(d14i * w2i - d23i * w1i), d14r * w2i - d23r * w1i);
        r1[k] = pr + ur;
        i1[k] = pi + ui;
        r4[k] = pr - ur;
        i4[k] = pi - ui;
        r2[k] = qr + vr;
        i2[k] = qi + vi;
        r3[k] = qr - vr;
        i3[k] = qi - vi;
    }
}

/// Inverse MDCT of one block size; see the [module documentation](self).
#[derive(Clone, Debug)]
pub struct Imdct {
    /// Output length `N`.
    len: usize,
    fft: Fft,
    /// `e^{-iπ(j + 1/8) / K}` for `j < N / 4`, applied before the FFT, real and
    /// imaginary parts.
    pre: (Vec<f32>, Vec<f32>),
    /// `scale` times the same, applied after it.
    post: (Vec<f32>, Vec<f32>),
    /// The FFT's in-place planes.
    re: Vec<f32>,
    im: Vec<f32>,
    /// The DCT-IV of the last [`Imdct::imdct`] input.
    dct: Vec<f32>,
}

impl Imdct {
    /// Heap bytes an [`Imdct`] of output length `n` holds, for charging a budget first.
    #[must_use]
    pub fn table_bytes(n: usize) -> usize {
        let q = n / 4;
        // FFT tables, pre/post twiddles and the work planes; the DCT-IV output; the
        // stage list.
        Fft::table_bytes(q)
            .saturating_add(q.saturating_mul(6 * size_of::<f32>()))
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
        let twiddles = |s: f64| -> (Vec<f32>, Vec<f32>) {
            let angle = |j: usize| -PI * (j as f64 + 0.125) / k as f64;
            (
                (0..q).map(|j| (s * angle(j).cos()) as f32).collect(),
                (0..q).map(|j| (s * angle(j).sin()) as f32).collect(),
            )
        };
        Some(Imdct {
            len: n,
            fft,
            pre: twiddles(1.0),
            post: twiddles(scale),
            re: vec![0.0; q],
            im: vec![0.0; q],
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
        let (pre_re, pre_im) = (&self.pre.0, &self.pre.1);
        let twiddled = |j: u32| {
            let j = j as usize;
            mul(x[2 * j], x[k - 1 - 2 * j], pre_re[j], pre_im[j])
        };
        // Pre-twiddle straight into the FFT's digit-reversed order, computing the
        // innermost radix-4 stage on each four points before they are stored.
        let order = self.fft.order();
        if self.fft.leaf_is_radix4() {
            let slots = self.re.chunks_exact_mut(4).zip(self.im.chunks_exact_mut(4));
            for ((r, i), from) in slots.zip(order.chunks_exact(4)) {
                let y = leaf4([from[0], from[1], from[2], from[3]].map(twiddled));
                for (j, (yr, yi)) in y.into_iter().enumerate() {
                    (r[j], i[j]) = (yr, yi);
                }
            }
        } else {
            let slots = self.re.iter_mut().zip(self.im.iter_mut());
            for ((re, im), &j) in slots.zip(order) {
                (*re, *im) = twiddled(j);
            }
            // A one-point transform has no stage at all.
            if let Some(first) = self.fft.stages.first() {
                run_stage(&mut self.re, &mut self.im, first);
            }
        }
        self.fft.transform_after_first(&mut self.re, &mut self.im);
        let (post_re, post_im) = (&self.post.0, &self.post.1);
        for j in 0..k / 2 {
            let (ur, ui) = mul(self.re[j], self.im[j], post_re[j], post_im[j]);
            w[2 * j] = ur;
            w[k - 1 - 2 * j] = -ui;
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
            let input: Vec<(f32, f32)> =
                (0..len).map(|_| (lcg(&mut seed), lcg(&mut seed))).collect();
            let mut re: Vec<f32> = fft.order().iter().map(|&j| input[j as usize].0).collect();
            let mut im: Vec<f32> = fft.order().iter().map(|&j| input[j as usize].1).collect();
            fft.transform(&mut re, &mut im);
            for (k, got) in re.iter().zip(&im).enumerate() {
                let (mut want_re, mut want_im) = (0f64, 0f64);
                for (n, &(zr, zi)) in input.iter().enumerate() {
                    let a = -2.0 * PI * (k * n) as f64 / len as f64;
                    want_re += f64::from(zr) * a.cos() - f64::from(zi) * a.sin();
                    want_im += f64::from(zr) * a.sin() + f64::from(zi) * a.cos();
                }
                let (re, im) = (want_re, want_im);
                let got = (*got.0, *got.1);
                let tolerance = 2e-5 * (len as f64).sqrt();
                assert!(
                    (f64::from(got.0) - re).abs() < tolerance
                        && (f64::from(got.1) - im).abs() < tolerance,
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
        let mut tiny = Imdct::new(4, 1.0).unwrap();
        let mut y = [0.0; 4];
        tiny.imdct(&[1.0, 0.0], &mut y);
        assert!(Imdct::new(28, 1.0).is_none());
        assert!(Imdct::new(30, 1.0).is_none());
    }
}
