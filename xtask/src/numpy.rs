//! The floating-point reductions numpy performs, in numpy's order, so results
//! recorded by numpy-based tooling are reproduced bit for bit.
//!
//! `np.add.reduce` over a contiguous run of `float64` sums blocks of
//! [`BUFFER`] elements with numpy's pairwise summation and adds the block sums
//! in order; that is what `np.mean` of a 1-D array (or of an `(n, 1)` array
//! along axis 0) computes. Along axis 0 of an `(n, k)` array with `k > 1`
//! numpy adds row after row, a plain sequential sum per column.

/// numpy's iterator buffer size: the length of the runs pairwise summation sees.
const BUFFER: usize = 8192;

/// numpy's `pairwise_sum` of one contiguous run.
fn pairwise(a: &[f64]) -> f64 {
    let n = a.len();
    if n < 8 {
        let mut res = -0.0;
        for &x in a {
            res += x;
        }
        res
    } else if n <= 128 {
        let mut r = [0.0; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - n % 8 {
            for j in 0..8 {
                r[j] += a[i + j];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        for &x in &a[i..] {
            res += x;
        }
        res
    } else {
        let mut half = n / 2;
        half -= half % 8;
        pairwise(&a[..half]) + pairwise(&a[half..])
    }
}

/// `np.sum` of a contiguous 1-D `float64` array.
pub fn sum(a: &[f64]) -> f64 {
    let mut blocks = a.chunks(BUFFER).map(pairwise);
    let first = blocks.next().unwrap_or(0.0);
    blocks.fold(first, |total, block| total + block)
}

/// `np.mean` of a contiguous 1-D `float64` array (`nan` when empty).
pub fn mean(a: &[f64]) -> f64 {
    sum(a) / a.len() as f64
}

/// `np.mean(d, axis=0)` of `d` laid out as `d.reshape(-1, columns)`.
pub fn column_means(d: &[f64], columns: usize) -> Vec<f64> {
    let rows = d.len() / columns;
    if columns == 1 {
        return vec![mean(d)];
    }
    let mut sums = vec![0.0; columns];
    for row in d.chunks_exact(columns) {
        for (total, &x) in sums.iter_mut().zip(row) {
            *total += x;
        }
    }
    sums.into_iter().map(|s| s / rows as f64).collect()
}

/// `np.max(d, axis=0)` of `d` laid out as `d.reshape(-1, columns)`: `nan`
/// propagates.
pub fn column_max(d: &[f64], columns: usize) -> Vec<f64> {
    let mut best = vec![f64::NEG_INFINITY; columns];
    for row in d.chunks_exact(columns) {
        for (m, &x) in best.iter_mut().zip(row) {
            if x.is_nan() || m.is_nan() {
                *m = f64::NAN;
            } else if x > *m {
                *m = x;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| ((i * 7919) % 1000) as f64 * 1e-3 + 1.0 / (i + 1) as f64)
            .collect()
    }

    #[test]
    fn mean_matches_numpy_pairwise_blocks() {
        // python3 -c "import numpy as np; a=np.array([((i*7919)%1000)*1e-3+1.0/(i+1) for i in range(20000)]); print([float(np.mean(a[:n])) for n in (5, 100, 1000, 20000)])"
        let a = sample(20000);
        assert_eq!(mean(&a[..5]), 1.0946666666666667);
        assert_eq!(mean(&a[..100]), 0.5423737751763962);
        assert_eq!(mean(&a[..1000]), 0.5069854708605503);
        assert_eq!(mean(&a), 0.5000240364108615);
        assert_eq!(sum(&[]), 0.0);
    }

    #[test]
    fn column_means_match_numpy_axis_zero() {
        // python3 -c "import numpy as np; d=np.array([((i*31)%17-8)*1e-6+1e-9*i for i in range(30000)]).reshape(-1,3); print([float(v) for v in np.mean(d*d, axis=0)])"
        let d: Vec<f64> = (0..30000)
            .map(|i| f64::from((i * 31) % 17 - 8) * 1e-6 + 1e-9 * f64::from(i))
            .collect();
        let squares: Vec<f64> = d.iter().map(|x| x * x).collect();
        assert_eq!(
            column_means(&squares, 3),
            [
                3.2392832129999927e-10,
                3.240079876999994e-10,
                3.239851945000003e-10
            ]
        );
        // python3 -c "import numpy as np; m=np.array([((i*13)%11-5)*1e-7 for i in range(20001)]).reshape(-1,1); print(float(np.sqrt(np.mean(m*m, axis=0))[0]))"
        let m: Vec<f64> = (0..20001)
            .map(|i| f64::from((i * 13) % 11 - 5) * 1e-7)
            .collect();
        let squares: Vec<f64> = m.iter().map(|x| x * x).collect();
        assert_eq!(column_means(&squares, 1)[0].sqrt(), 3.162317186415781e-07);
    }

    #[test]
    fn column_max_propagates_nan() {
        assert_eq!(column_max(&[1.0, 5.0, 3.0, 2.0], 2), [3.0, 5.0]);
        assert!(column_max(&[1.0, f64::NAN, 3.0, 2.0], 2)[1].is_nan());
    }
}
