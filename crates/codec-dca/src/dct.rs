//! DCT-IV: `y[k] = sum_n x[n] cos(pi/N (n + 1/2)(k + 1/2))`, in place, for N = 32 or 64.
//!
//! Computed through an N/2-point complex FFT: `z[m] = (x[2m] + i x[N-1-2m]) e^{-i pi (m + 1/4)/N}`,
//! `Z = FFT(z)`, `Z'[m] = Z[m] e^{-i pi m / N}`, `y[2m] = Re Z'[m]`, `y[N-1-2m] = -Im Z'[m]`.

const MAX_M: usize = 32;

#[derive(Debug)]
pub(crate) struct Dct4 {
    n: usize,
    pre: [(f32, f32); MAX_M],
    post: [(f32, f32); MAX_M],
    /// FFT twiddles e^{-2 pi i j / M}, j < M/2.
    tw: [(f32, f32); MAX_M / 2],
    rev: [u8; MAX_M],
}

impl Dct4 {
    pub(crate) fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n / 2 <= MAX_M && n >= 4);
        let m = n / 2;
        let log2m = m.trailing_zeros();
        let pi = std::f64::consts::PI;
        let mut pre = [(0.0, 0.0); MAX_M];
        let mut post = [(0.0, 0.0); MAX_M];
        let mut tw = [(0.0, 0.0); MAX_M / 2];
        let mut rev = [0; MAX_M];
        for k in 0..m {
            let a = -pi * (k as f64 + 0.25) / n as f64;
            pre[k] = (a.cos() as f32, a.sin() as f32);
            let a = -pi * k as f64 / n as f64;
            post[k] = (a.cos() as f32, a.sin() as f32);
            rev[k] = ((k as u32).reverse_bits() >> (32 - log2m)) as u8;
        }
        for (j, t) in tw[..m / 2].iter_mut().enumerate() {
            let a = -2.0 * pi * j as f64 / m as f64;
            *t = (a.cos() as f32, a.sin() as f32);
        }
        Self {
            n,
            pre,
            post,
            tw,
            rev,
        }
    }

    #[inline]
    pub(crate) fn transform(&self, x: &mut [f32]) {
        match self.n {
            32 => self.transform_n::<32, 16>(x),
            64 => self.transform_n::<64, 32>(x),
            _ => unreachable!("DCT-IV size"),
        }
    }

    #[inline]
    fn transform_n<const N: usize, const M: usize>(&self, x: &mut [f32]) {
        let x: &mut [f32; N] = (&mut x[..N]).try_into().expect("DCT length");
        let pre: &[(f32, f32); M] = self.pre[..M].try_into().expect("twiddles");
        let post: &[(f32, f32); M] = self.post[..M].try_into().expect("twiddles");
        let rev: &[u8; M] = self.rev[..M].try_into().expect("bit reversal");
        let mut re = [0f32; M];
        let mut im = [0f32; M];
        for k in 0..M {
            let a = x[2 * k];
            let b = x[N - 1 - 2 * k];
            let (c, s) = pre[k];
            let j = rev[k] as usize & (M - 1);
            re[j] = a * c - b * s;
            im[j] = a * s + b * c;
        }
        // Iterative radix-2 decimation-in-time FFT of size M.
        let mut half = 1;
        while half < M {
            let step = M / (2 * half);
            let mut base = 0;
            while base < M {
                for j in 0..half {
                    let (wr, wi) = self.tw[(j * step) & (M / 2 - 1)];
                    let p = base + j;
                    let q = (p + half) & (M - 1);
                    let tr = re[q] * wr - im[q] * wi;
                    let ti = re[q] * wi + im[q] * wr;
                    re[q] = re[p] - tr;
                    im[q] = im[p] - ti;
                    re[p] += tr;
                    im[p] += ti;
                }
                base += 2 * half;
            }
            half *= 2;
        }
        for k in 0..M {
            let (c, s) = post[k];
            x[2 * k] = re[k] * c - im[k] * s;
            x[N - 1 - 2 * k] = -(re[k] * s + im[k] * c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Dct4;

    #[test]
    fn matches_direct_definition() {
        for n in [32usize, 64] {
            let d = Dct4::new(n);
            let mut x: Vec<f32> = (0..n)
                .map(|i| ((i * 7919 % 97) as f32 - 48.0) / 17.0)
                .collect();
            let src = x.clone();
            d.transform(&mut x);
            for k in 0..n {
                let mut acc = 0f64;
                for (i, &v) in src.iter().enumerate() {
                    let a = std::f64::consts::PI / n as f64 * (i as f64 + 0.5) * (k as f64 + 0.5);
                    acc += f64::from(v) * a.cos();
                }
                assert!(
                    (acc - f64::from(x[k])).abs() < 1e-4,
                    "n={n} k={k}: {acc} vs {}",
                    x[k]
                );
            }
        }
    }
}
