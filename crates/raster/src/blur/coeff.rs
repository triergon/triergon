//! The cascade blur's parameters for a variance: how often to halve, and the
//! symmetric kernel of the filter run at the reduced size.
//!
//! The kernel solves a least-squares fit of the cascade's frequency response
//! (halving with [1, 5, 10, 10, 5, 1], the kernel, doubling with [5, 10, 1] and
//! [1, 10, 5]) to the Gaussian's, as libass derives it, in the same floating
//! point order.

use crate::fp;

/// A cascade blur along one axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Method {
    /// Halvings before and doublings after the kernel.
    pub(crate) level: u32,
    /// Kernel radius, 4 to 8.
    pub(crate) radius: usize,
    /// Kernel taps 1..=radius in 1/65536; the centre tap is implied.
    pub(crate) coeff: [i16; 8],
}

impl Method {
    /// The method for variance `r2` (in pixels²).
    pub(crate) fn new(r2: f64) -> Self {
        let mut mu = [0.0f64; 8];
        let (level, radius);
        if r2 < 0.5 {
            level = 0;
            radius = 4;
            mu[1] = 0.085 * r2 * r2 * r2;
            mu[0] = 0.5 * r2 - 4.0 * mu[1];
        } else {
            let (frac, exponent) = fp::frexp((0.11569 * r2 + 0.20591047).sqrt());
            level = exponent.max(0) as u32;
            let mul = fp::ldexp(1.0, -2 * exponent); // 0.25^level
            let shrink = (10.1525 + 0.8335 * mul) * (1.0 - frac);
            // Converted as AArch64 converts: saturating, NaN to 0.
            radius = 8i32.wrapping_sub(shrink as i32).clamp(4, 8) as usize;
            calc_coeff(&mut mu, radius, r2, mul);
        }
        let mut coeff = [0i16; 8];
        for (c, &m) in coeff.iter_mut().zip(&mu).take(radius) {
            *c = (65536.0 * m + 0.5) as i32 as i16;
        }
        Self {
            level,
            radius,
            coeff,
        }
    }
}

/// `res[i] = r2`-Gaussian samples at 0..n, unnormalised: `√(α/π) · e^(−α i²)`.
fn gauss(res: &mut [f64], r2: f64) {
    let alpha = 0.5 / r2;
    let mut mul = (-alpha).exp();
    let mul2 = mul * mul;
    let mut cur = (alpha / std::f64::consts::PI).sqrt();
    res[0] = cur;
    cur *= mul;
    res[1] = cur;
    for r in &mut res[2..] {
        mul *= mul2;
        cur *= mul;
        *r = cur;
    }
}

/// Convolves the symmetric sequence `coeff` (its mirror image implied) with
/// the 7-tap symmetric `kernel` in place, for the first `n` entries.
fn filter(coeff: &mut [f64], n: usize, kernel: &[f64; 4]) {
    let (mut prev1, mut prev2, mut prev3) = (coeff[1], coeff[2], coeff[3]);
    for i in 0..n {
        let res = coeff[i] * kernel[0]
            + (prev1 + coeff[i + 1]) * kernel[1]
            + (prev2 + coeff[i + 2]) * kernel[2]
            + (prev3 + coeff[i + 3]) * kernel[3];
        prev3 = prev2;
        prev2 = prev1;
        prev1 = coeff[i];
        coeff[i] = res;
    }
}

/// The normal matrix of the fit, inverted in place by Gauss–Jordan elimination.
fn normal_matrix(freq: &[f64; 17], n: usize) -> [[f64; 8]; 8] {
    let mut mat = [[0.0f64; 8]; 8];
    for i in 0..n {
        mat[i][i] = freq[2 * i + 2] + 3.0 * freq[0] - 4.0 * freq[i + 1];
        for j in i + 1..n {
            let value = freq[i + j + 2] + freq[j - i] + 2.0 * (freq[0] - freq[i + 1] - freq[j + 1]);
            mat[i][j] = value;
            mat[j][i] = value;
        }
    }
    for k in 0..n {
        let z = 1.0 / mat[k][k];
        mat[k][k] = 1.0;
        for i in 0..n {
            if i == k {
                continue;
            }
            let mul = mat[i][k] * z;
            mat[i][k] = 0.0;
            for j in 0..n {
                mat[i][j] -= mat[k][j] * mul;
            }
        }
        for j in 0..n {
            mat[k][j] *= z;
        }
    }
    mat
}

/// Solves for the `n` kernel taps of variance `r2` at scale `mul` = 4^−level.
fn calc_coeff(mu: &mut [f64; 8], n: usize, r2: f64, mul: f64) {
    const W: f64 = 12096.0;
    let poly = |a: f64, b: f64, c: f64, d: f64| ((a / W * mul + b / W) * mul + c / W) * mul + d / W;
    let kernel = [
        poly(3280.0, 1092.0, 2520.0, 5204.0),
        poly(-2460.0, -273.0, -210.0, 2943.0),
        poly(984.0, -546.0, -924.0, 486.0),
        poly(-164.0, 273.0, -126.0, 17.0),
    ];
    let mut mat_freq = [0.0f64; 17];
    mat_freq[..4].copy_from_slice(&kernel);
    filter(&mut mat_freq, 7, &kernel);

    let mut vec_freq = [0.0f64; 12];
    gauss(&mut vec_freq[..n + 4], r2 * mul);
    filter(&mut vec_freq, n + 1, &kernel);

    let mat = normal_matrix(&mat_freq, n);
    let mut vec = [0.0f64; 8];
    for i in 0..n {
        vec[i] = mat_freq[0] - mat_freq[i + 1] - vec_freq[0] + vec_freq[i + 1];
    }
    for i in 0..n {
        let mut res = 0.0f64;
        for j in 0..n {
            res += mat[i][j] * vec[j];
        }
        mu[i] = fp::max(0.0, res);
    }
}

#[cfg(test)]
mod tests {
    use super::Method;

    #[test]
    fn small_variances_use_the_direct_kernel() {
        let method = Method::new(0.25);
        assert_eq!((method.level, method.radius), (0, 4));
        assert!(method.coeff[0] > 0 && method.coeff[2] == 0);
    }

    #[test]
    fn large_variances_halve_first() {
        let method = Method::new(400.0);
        assert!(method.level >= 2);
        assert!((4..=8).contains(&method.radius));
    }
}
