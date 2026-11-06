//! Complex FFT, DCT-IV and (I)MDCT built on it. Own derivation (see unit tests that compare
//! against the direct O(N^2) definitions).

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Cpx {
    pub re: f32,
    pub im: f32,
}

impl Cpx {
    #[inline(always)]
    pub(crate) fn new(re: f32, im: f32) -> Cpx {
        Cpx { re, im }
    }
    #[inline(always)]
    pub(crate) fn mul(self, o: Cpx) -> Cpx {
        Cpx {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }
}

/// One radix-4 stage: twiddles W^k, W^2k, W^3k of the 4h-point transform, k < h, split
/// into real and imaginary parts.
#[derive(Debug)]
struct Stage {
    h: usize,
    w: [Vec<f32>; 6],
}

/// In-place forward complex FFT (sign -1) of power-of-two size on split real / imaginary
/// arrays: decimation in time on bit-reversed input, a twiddle-free radix-4 first stage,
/// radix-4 stages and, for odd log2(n), a final radix-2 stage. The split layout keeps every
/// inner loop contiguous so that it vectorises.
#[derive(Debug)]
pub(crate) struct Fft {
    n: usize,
    bitrev: Vec<u16>,
    stages: Vec<Stage>,
    /// final radix-2 stage twiddles W_n^k, k < n/2 (odd log2(n) only)
    last: Option<[Vec<f32>; 2]>,
}

fn twiddle(k: usize, n: usize) -> (f32, f32) {
    let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
    (a.cos() as f32, a.sin() as f32)
}

impl Fft {
    pub(crate) fn new(n: usize) -> Fft {
        assert!(n.is_power_of_two() && n >= 4);
        let bits = n.trailing_zeros();
        let bitrev = (0..n)
            .map(|i| ((i as u32).reverse_bits() >> (32 - bits)) as u16)
            .collect();
        let mut stages = Vec::new();
        let mut h = 4;
        while 4 * h <= n {
            let mut w: [Vec<f32>; 6] = Default::default();
            for k in 0..h {
                for m in 0..3 {
                    let (c, s) = twiddle((m + 1) * k, 4 * h);
                    w[2 * m].push(c);
                    w[2 * m + 1].push(s);
                }
            }
            stages.push(Stage { h, w });
            h *= 4;
        }
        let last = (bits % 2 == 1).then(|| {
            let (c, s): (Vec<f32>, Vec<f32>) = (0..n / 2).map(|k| twiddle(k, n)).unzip();
            [c, s]
        });
        Fft {
            n,
            bitrev,
            stages,
            last,
        }
    }

    /// Position of input sample `i` for `run_permuted` (bit reversal).
    #[inline(always)]
    pub(crate) fn perm(&self, i: usize) -> usize {
        self.bitrev[i] as usize
    }

    #[cfg(test)]
    pub(crate) fn run(&self, re: &mut [f32], im: &mut [f32]) {
        for (i, &j) in self.bitrev.iter().enumerate() {
            let j = j as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        self.run_permuted(re, im);
    }

    /// FFT of data already stored in bit-reversed order (`d[perm(i)] = x[i]`).
    pub(crate) fn run_permuted(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        let (re, im) = (&mut re[..n], &mut im[..n]);
        for (r, i) in re.chunks_exact_mut(4).zip(im.chunks_exact_mut(4)) {
            let (or, oi) = bfly4([r[0], r[1], r[2], r[3]], [i[0], i[1], i[2], i[3]]);
            r.copy_from_slice(&or);
            i.copy_from_slice(&oi);
        }
        self.middle(re, im);
        if let Some([wr, wi]) = &self.last {
            let (lr, hr) = re.split_at_mut(n / 2);
            let (li, hi) = im.split_at_mut(n / 2);
            for ((((lr, li), (hr, hi)), &wr), &wi) in lr
                .iter_mut()
                .zip(li.iter_mut())
                .zip(hr.iter_mut().zip(hi.iter_mut()))
                .zip(wr)
                .zip(wi)
            {
                let tr = *hr * wr - *hi * wi;
                let ti = *hr * wi + *hi * wr;
                let (ar, ai) = (*lr, *li);
                *lr = ar + tr;
                *li = ai + ti;
                *hr = ar - tr;
                *hi = ai - ti;
            }
        }
    }

    /// The radix-4 stages after the first (twiddle-free) one.
    fn middle(&self, re: &mut [f32], im: &mut [f32]) {
        for st in &self.stages {
            if st.h == 4 {
                // one 4-lane vector per quarter: fixed-size blocks so the lanes vectorise
                let w: [[f32; 4]; 6] = std::array::from_fn(|t| st.w[t][..4].try_into().unwrap());
                for (br, bi) in re.chunks_exact_mut(16).zip(im.chunks_exact_mut(16)) {
                    let br: &mut [f32; 16] = br.try_into().unwrap();
                    let bi: &mut [f32; 16] = bi.try_into().unwrap();
                    for k in 0..4 {
                        radix4(
                            br,
                            bi,
                            [k, 4 + k, 8 + k, 12 + k],
                            [w[0][k], w[1][k], w[2][k], w[3][k], w[4][k], w[5][k]],
                        );
                    }
                }
                continue;
            }
            let h = st.h;
            for (br, bi) in re.chunks_exact_mut(4 * h).zip(im.chunks_exact_mut(4 * h)) {
                let (r01, r23) = br.split_at_mut(2 * h);
                let (r0, r1) = r01.split_at_mut(h);
                let (r2, r3) = r23.split_at_mut(h);
                let (i01, i23) = bi.split_at_mut(2 * h);
                let (i0, i1) = i01.split_at_mut(h);
                let (i2, i3) = i23.split_at_mut(h);
                radix4_block(r0, r1, r2, r3, i0, i1, i2, i3, &st.w);
            }
        }
    }
}

/// h radix-4 butterflies between the quarters `r`/`i` (each of length h). A separate,
/// non-inlined function: its `&mut` arguments are known not to alias, so the loop
/// vectorises without runtime overlap checks.
#[inline(never)]
// separate slice arguments are the point: each gets its own no-alias guarantee
#[allow(clippy::too_many_arguments)]
fn radix4_block(
    r0: &mut [f32],
    r1: &mut [f32],
    r2: &mut [f32],
    r3: &mut [f32],
    i0: &mut [f32],
    i1: &mut [f32],
    i2: &mut [f32],
    i3: &mut [f32],
    w: &[Vec<f32>; 6],
) {
    let h = r0.len();
    let (r1, r2, r3, i0, i1, i2, i3) = (
        &mut r1[..h],
        &mut r2[..h],
        &mut r3[..h],
        &mut i0[..h],
        &mut i1[..h],
        &mut i2[..h],
        &mut i3[..h],
    );
    let [w1r, w1i, w2r, w2i, w3r, w3i] = w;
    let (w1r, w1i, w2r, w2i, w3r, w3i) = (
        &w1r[..h],
        &w1i[..h],
        &w2r[..h],
        &w2i[..h],
        &w3r[..h],
        &w3i[..h],
    );
    for k in 0..h {
        let (t0r, t0i) = (r0[k], i0[k]);
        let t1r = r1[k] * w2r[k] - i1[k] * w2i[k];
        let t1i = r1[k] * w2i[k] + i1[k] * w2r[k];
        let t2r = r2[k] * w1r[k] - i2[k] * w1i[k];
        let t2i = r2[k] * w1i[k] + i2[k] * w1r[k];
        let t3r = r3[k] * w3r[k] - i3[k] * w3i[k];
        let t3i = r3[k] * w3i[k] + i3[k] * w3r[k];
        let (s01r, s01i, d01r, d01i) = (t0r + t1r, t0i + t1i, t0r - t1r, t0i - t1i);
        let (s23r, s23i, d23r, d23i) = (t2r + t3r, t2i + t3i, t2r - t3r, t2i - t3i);
        r0[k] = s01r + s23r;
        i0[k] = s01i + s23i;
        r2[k] = s01r - s23r;
        i2[k] = s01i - s23i;
        r1[k] = d01r + d23i;
        i1[k] = d01i - d23r;
        r3[k] = d01r - d23i;
        i3[k] = d01i + d23r;
    }
}

/// One radix-4 butterfly on positions `p` (quarters 0..3) with twiddles [W^k, W^2k, W^3k]
/// as (re, im) pairs. Same arithmetic as the general stage loop.
#[inline(always)]
fn radix4(r: &mut [f32; 16], i: &mut [f32; 16], p: [usize; 4], w: [f32; 6]) {
    let (t0r, t0i) = (r[p[0]], i[p[0]]);
    let t1r = r[p[1]] * w[2] - i[p[1]] * w[3];
    let t1i = r[p[1]] * w[3] + i[p[1]] * w[2];
    let t2r = r[p[2]] * w[0] - i[p[2]] * w[1];
    let t2i = r[p[2]] * w[1] + i[p[2]] * w[0];
    let t3r = r[p[3]] * w[4] - i[p[3]] * w[5];
    let t3i = r[p[3]] * w[5] + i[p[3]] * w[4];
    let (s01r, s01i, d01r, d01i) = (t0r + t1r, t0i + t1i, t0r - t1r, t0i - t1i);
    let (s23r, s23i, d23r, d23i) = (t2r + t3r, t2i + t3i, t2r - t3r, t2i - t3i);
    r[p[0]] = s01r + s23r;
    i[p[0]] = s01i + s23i;
    r[p[2]] = s01r - s23r;
    i[p[2]] = s01i - s23i;
    r[p[1]] = d01r + d23i;
    i[p[1]] = d01i - d23r;
    r[p[3]] = d01r - d23i;
    i[p[3]] = d01i + d23r;
}

/// DCT-IV pre-twiddle: (x[2n] + i x[m - 1 - 2n]) * pre[n].
#[inline(never)]
fn pre(x: &[f32], pr: &[f32], pi: &[f32], tr: &mut [f32], ti: &mut [f32]) {
    for (((((a, b), &cr), &ci), r), i) in x
        .chunks_exact(2)
        .zip(x.rchunks_exact(2))
        .zip(pr)
        .zip(pi)
        .zip(tr.iter_mut())
        .zip(ti.iter_mut())
    {
        let (a, b) = (a[0], b[1]);
        *r = a * cr - b * ci;
        *i = a * ci + b * cr;
    }
}

/// DCT-IV post-twiddle: ye = Re(z q), yo = -Im(z q).
#[inline(never)]
fn post(re: &[f32], im: &[f32], qr: &[f32], qi: &[f32], ye: &mut [f32], yo: &mut [f32]) {
    let h = re.len();
    let (im, qr, qi, ye, yo) = (&im[..h], &qr[..h], &qi[..h], &mut ye[..h], &mut yo[..h]);
    for k in 0..h {
        ye[k] = re[k] * qr[k] - im[k] * qi[k];
        yo[k] = -(re[k] * qi[k] + im[k] * qr[k]);
    }
}

/// Last radix-2 FFT stage (z = lo +/- w hi) fused with the DCT-IV post-twiddle of both
/// halves. Non-inlined so the non-aliasing arguments let the loop vectorise.
#[inline(never)]
// separate slice arguments are the point: each gets its own no-alias guarantee
#[allow(clippy::too_many_arguments)]
fn last_post(
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
    let (re1, im0, im1, wr, wi) = (&re1[..q], &im0[..q], &im1[..q], &wr[..q], &wi[..q]);
    let (qr0, qr1) = (&qr[..q], &qr[q..2 * q]);
    let (qi0, qi1) = (&qi[..q], &qi[q..2 * q]);
    let (ye0, ye1, yo0, yo1) = (&mut ye0[..q], &mut ye1[..q], &mut yo0[..q], &mut yo1[..q]);
    for k in 0..q {
        let tr = re1[k] * wr[k] - im1[k] * wi[k];
        let ti = re1[k] * wi[k] + im1[k] * wr[k];
        let (zr, zi) = (re0[k] + tr, im0[k] + ti);
        ye0[k] = zr * qr0[k] - zi * qi0[k];
        yo0[k] = -(zr * qi0[k] + zi * qr0[k]);
        let (zr, zi) = (re0[k] - tr, im0[k] - ti);
        ye1[k] = zr * qr1[k] - zi * qi1[k];
        yo1[k] = -(zr * qi1[k] + zi * qr1[k]);
    }
}

/// Twiddle-free radix-4 butterfly of the first stage; the inputs are the residues 0, 2, 1, 3
/// (mod 4) in bit-reversed storage order.
#[inline(always)]
fn bfly4(r: [f32; 4], i: [f32; 4]) -> ([f32; 4], [f32; 4]) {
    let (s01r, s01i, d01r, d01i) = (r[0] + r[1], i[0] + i[1], r[0] - r[1], i[0] - i[1]);
    let (s23r, s23i, d23r, d23i) = (r[2] + r[3], i[2] + i[3], r[2] - r[3], i[2] - i[3]);
    // outputs 1 and 3: d01 -/+ i d23
    (
        [s01r + s23r, d01r + d23i, s01r - s23r, d01r - d23i],
        [s01i + s23i, d01i - d23r, s01i - s23i, d01i + d23r],
    )
}

/// DCT-IV of even length `m` via an m/2-point complex FFT.
#[derive(Debug)]
pub(crate) struct Dct4 {
    m: usize,
    fft: Fft,
    /// pre-twiddle exp(-i pi (n + 1/4) / m) * scale
    pre: [Vec<f32>; 2],
    /// source index n of FFT position j (bit reversal)
    src: Vec<u16>,
    /// post-twiddle exp(-i pi k / m)
    post: [Vec<f32>; 2],
    /// pre-twiddled input, natural order
    tr: Vec<f32>,
    ti: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    /// y split as in `run_split` (for `run`)
    ye: Vec<f32>,
    yo: Vec<f32>,
}

impl Dct4 {
    pub(crate) fn new(m: usize, scale: f64) -> Dct4 {
        let h = m / 2;
        let pi = std::f64::consts::PI;
        let fft = Fft::new(h);
        let mut src = vec![0u16; h];
        for n in 0..h {
            src[fft.perm(n)] = n as u16;
        }
        let tw = |f: fn(f64) -> f64, off: f64, s: f64| {
            (0..h)
                .map(|n| (f(-pi * (n as f64 + off) / m as f64) * s) as f32)
                .collect::<Vec<f32>>()
        };
        let pre = [tw(f64::cos, 0.25, scale), tw(f64::sin, 0.25, scale)];
        let post = [tw(f64::cos, 0.0, 1.0), tw(f64::sin, 0.0, 1.0)];
        let z = vec![0.0; h];
        Dct4 {
            m,
            fft,
            pre,
            src,
            post,
            tr: z.clone(),
            ti: z.clone(),
            re: z.clone(),
            im: z.clone(),
            ye: z.clone(),
            yo: z,
        }
    }

    /// y[k] = scale * sum_n x[n] cos(pi/m (n+1/2)(k+1/2)), returned split into the even
    /// outputs ye[k] = y[2k] and the reversed odd outputs yo[k] = y[m - 1 - 2k], k < m/2.
    ///
    /// Pre-twiddle in natural order, the bit-reversal gather fused with the first FFT stage
    /// and, for odd log2(m/2), the post-twiddle fused with the last stage: every pass but the
    /// gather is a contiguous loop.
    pub(crate) fn run_split(&mut self, x: &[f32], ye: &mut [f32], yo: &mut [f32]) {
        let m = self.m;
        let h = m / 2;
        let x = &x[..m];
        let (pr, pi) = (&self.pre[0][..h], &self.pre[1][..h]);
        let (tr, ti) = (&mut self.tr[..h], &mut self.ti[..h]);
        pre(x, pr, pi, tr, ti);
        let (re, im) = (&mut self.re[..h], &mut self.im[..h]);
        for ((r4, i4), s4) in re
            .chunks_exact_mut(4)
            .zip(im.chunks_exact_mut(4))
            .zip(self.src.chunks_exact(4))
        {
            let g = |k: usize| s4[k] as usize;
            let (or, oi) = bfly4(
                [tr[g(0)], tr[g(1)], tr[g(2)], tr[g(3)]],
                [ti[g(0)], ti[g(1)], ti[g(2)], ti[g(3)]],
            );
            r4.copy_from_slice(&or);
            i4.copy_from_slice(&oi);
        }
        self.fft.middle(re, im);
        let (qr, qi) = (&self.post[0][..h], &self.post[1][..h]);
        let (ye, yo) = (&mut ye[..h], &mut yo[..h]);
        if let Some([wr, wi]) = &self.fft.last {
            let q = h / 2;
            let (re0, re1) = re.split_at(q);
            let (im0, im1) = im.split_at(q);
            let (ye0, ye1) = ye.split_at_mut(q);
            let (yo0, yo1) = yo.split_at_mut(q);
            last_post(re0, re1, im0, im1, wr, wi, qr, qi, ye0, ye1, yo0, yo1);
        } else {
            post(re, im, qr, qi, ye, yo);
        }
    }

    /// y[k] = scale * sum_n x[n] cos(pi/m (n+1/2)(k+1/2)).
    pub(crate) fn run(&mut self, x: &[f32], y: &mut [f32]) {
        let (mut ye, mut yo) = (std::mem::take(&mut self.ye), std::mem::take(&mut self.yo));
        self.run_split(x, &mut ye, &mut yo);
        let m = self.m;
        for (k, (&e, &o)) in ye.iter().zip(&yo).enumerate() {
            y[2 * k] = e;
            y[m - 1 - 2 * k] = o;
        }
        (self.ye, self.yo) = (ye, yo);
    }
}

/// IMDCT / MDCT of window length n (= 2 * m coefficients).
#[derive(Debug)]
pub(crate) struct Mdct {
    m: usize,
    dct: Dct4,
    tmp: Vec<f32>,
    tmp2: Vec<f32>,
}

impl Mdct {
    /// `scale` multiplies the transform sum.
    pub(crate) fn new(n: usize, scale: f64) -> Mdct {
        let m = n / 2;
        Mdct {
            m,
            dct: Dct4::new(m, scale),
            tmp: vec![0.0; m],
            tmp2: vec![0.0; m],
        }
    }

    /// The DCT-IV core of the IMDCT, split as in `Dct4::run_split`: the IMDCT output is
    /// out[n] = y[n + m/2] (n < m/2), -y[3m/2 - 1 - n] (m/2 <= n < 3m/2), -y[n - 3m/2] (n >= 3m/2)
    /// with y[2k] = ye[k], y[m - 1 - 2k] = yo[k].
    pub(crate) fn imdct_split(&mut self, x: &[f32]) -> (&[f32], &[f32]) {
        let m = self.m;
        let (ye, yo) = self.tmp.split_at_mut(m / 2);
        self.dct.run_split(&x[..m], ye, yo);
        self.tmp.split_at(m / 2)
    }

    /// out[n] = scale * sum_k X[k] cos(2pi/N (n + n0)(k + 1/2)), n0 = (N/2 + 1)/2, N = 2m.
    pub(crate) fn imdct(&mut self, x: &[f32], out: &mut [f32]) {
        let m = self.m;
        let h = m / 2;
        self.dct.run(&x[..m], &mut self.tmp);
        let y = &self.tmp;
        out[..h].copy_from_slice(&y[h..2 * h]);
        for (o, &v) in out[h..3 * h].iter_mut().zip(y[..2 * h].iter().rev()) {
            *o = -v;
        }
        for (o, &v) in out[3 * h..2 * m].iter_mut().zip(&y[..h]) {
            *o = -v;
        }
    }

    /// X[k] = scale * sum_n x[n] cos(2pi/N (n + n0)(k + 1/2)), x has length N = 2m.
    pub(crate) fn mdct(&mut self, x: &[f32], out: &mut [f32]) {
        let m = self.m;
        let h = m / 2;
        let u = &mut self.tmp2;
        for j in 0..h {
            u[j] = -x[3 * h - 1 - j] - x[j + 3 * h];
        }
        for j in h..m {
            u[j] = x[j - h] - x[3 * h - 1 - j];
        }
        let u2: &[f32] = &self.tmp2;
        self.dct.run(u2, &mut out[..m]);
    }
}

#[cfg(test)]
// direct O(N^2) reference definitions, written index-style to mirror the formulas
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    fn lcg(s: &mut u32) -> f32 {
        *s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        (*s as i32) as f32 / 2147483648.0
    }

    #[test]
    fn fft_matches_definition() {
        for &n in &[4usize, 8, 32, 64, 128, 512] {
            let mut s = 3u32;
            let x: Vec<Cpx> = (0..n).map(|_| Cpx::new(lcg(&mut s), lcg(&mut s))).collect();
            let mut yr: Vec<f32> = x.iter().map(|c| c.re).collect();
            let mut yi: Vec<f32> = x.iter().map(|c| c.im).collect();
            Fft::new(n).run(&mut yr, &mut yi);
            let y: Vec<Cpx> = yr.iter().zip(&yi).map(|(&r, &i)| Cpx::new(r, i)).collect();
            for k in 0..n {
                let (mut re, mut im) = (0f64, 0f64);
                for (i, v) in x.iter().enumerate() {
                    let a = -2.0 * std::f64::consts::PI * (i * k % n) as f64 / n as f64;
                    re += v.re as f64 * a.cos() - v.im as f64 * a.sin();
                    im += v.re as f64 * a.sin() + v.im as f64 * a.cos();
                }
                assert!(
                    (re - y[k].re as f64).abs() < 1e-4 && (im - y[k].im as f64).abs() < 1e-4,
                    "n={n} k={k}"
                );
            }
        }
    }

    #[test]
    fn imdct_matches_definition() {
        for &n in &[256usize, 2048] {
            let m = n / 2;
            let mut s = 1u32;
            let x: Vec<f32> = (0..m).map(|_| lcg(&mut s)).collect();
            let mut md = Mdct::new(n, 1.0);
            let mut out = vec![0.0f32; n];
            md.imdct(&x, &mut out);
            let n0 = (n as f64 / 2.0 + 1.0) / 2.0;
            let mut maxerr = 0.0f64;
            for i in 0..n {
                let mut acc = 0.0f64;
                for k in 0..m {
                    acc += x[k] as f64
                        * (2.0 * std::f64::consts::PI / n as f64
                            * (i as f64 + n0)
                            * (k as f64 + 0.5))
                            .cos();
                }
                maxerr = maxerr.max((acc - out[i] as f64).abs());
            }
            assert!(maxerr < 1e-3, "n={n} err={maxerr}");
            // forward
            let xt: Vec<f32> = (0..n).map(|_| lcg(&mut s)).collect();
            let mut fx = vec![0.0f32; m];
            md.mdct(&xt, &mut fx);
            let mut maxerr = 0.0f64;
            for k in 0..m {
                let mut acc = 0.0f64;
                for i in 0..n {
                    acc += xt[i] as f64
                        * (2.0 * std::f64::consts::PI / n as f64
                            * (i as f64 + n0)
                            * (k as f64 + 0.5))
                            .cos();
                }
                maxerr = maxerr.max((acc - fx[k] as f64).abs());
            }
            assert!(maxerr < 1e-3, "mdct n={n} err={maxerr}");
        }
    }
}

#[cfg(test)]
mod timing {
    use super::*;
    /// Development aid: `AAC_TIME=dct4|fft COUNT cargo test --release ... -- --ignored`, run
    /// under `/usr/bin/time -l` to read instructions / cycles per call.
    #[test]
    #[ignore = "timing probe; run on demand with --release"]
    fn time_transform() {
        let which = std::env::var("AAC_TIME").unwrap_or_default();
        let count: usize = std::env::var("AAC_COUNT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100000);
        let mut d = Dct4::new(1024, 1.0);
        let x: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.37).sin()).collect();
        let mut y = vec![0f32; 1024];
        let n: usize = std::env::var("AAC_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(512);
        let fft = Fft::new(n);
        let (mut re, mut im) = (vec![0.1f32; n], vec![0.2f32; n]);
        let t = std::time::Instant::now();
        for _ in 0..count {
            match which.as_str() {
                "dct4" => {
                    let (ye, yo) = y.split_at_mut(512);
                    d.run_split(&x, ye, yo);
                }
                "fft" => {
                    fft.run_permuted(&mut re, &mut im);
                    re[3] = 0.5;
                    im[9] = 0.25;
                }
                _ => {}
            }
        }
        eprintln!(
            "{which}: {:.0} ns/call {}",
            t.elapsed().as_nanos() as f64 / count as f64,
            y[5] + re[7]
        );
    }
}
