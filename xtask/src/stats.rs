//! Summary statistics computed exactly as the recorded evidence computed them
//! (CPython's `statistics.median`, linear-interpolated percentiles and a seeded
//! bootstrap), so stored intervals can be recomputed and compared for equality.

use crate::pyrandom::PyRandom;

/// `statistics.median`.
pub fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Linear interpolation between closest ranks.
pub fn percentile(values: &[f64], fraction: f64) -> f64 {
    let mut ordered = values.to_vec();
    ordered.sort_by(f64::total_cmp);
    let index = (ordered.len() - 1) as f64 * fraction;
    let (lo, hi) = (index.floor() as usize, index.ceil() as usize);
    ordered[lo] + (ordered[hi] - ordered[lo]) * (index - lo as f64)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Interval {
    pub median: f64,
    pub lower_95: f64,
    pub upper_95: f64,
}

/// Median of the paired ratios `a / b` with a percentile-bootstrap 95% interval
/// (`resamples` resamples drawn from `Random(seed)`).
pub fn ratio_interval(
    pairs: &[(f64, f64)],
    seed: u64,
    resamples: usize,
) -> Result<Interval, String> {
    if pairs.len() < 2
        || pairs
            .iter()
            .any(|&(a, b)| !a.is_finite() || !b.is_finite() || a <= 0.0 || b <= 0.0)
    {
        return Err("ratios require positive finite paired measurements".into());
    }
    Ok(bootstrap_ratio_interval(pairs, seed, resamples))
}

/// The same paired-ratio bootstrap with nonnegative numerators and strictly
/// positive denominators. Zero numerator measurements are retained as zero ratios.
pub fn nonnegative_ratio_interval(
    pairs: &[(f64, f64)],
    seed: u64,
    resamples: usize,
) -> Result<Interval, String> {
    if pairs.len() < 2
        || pairs
            .iter()
            .any(|&(a, b)| !a.is_finite() || !b.is_finite() || a < 0.0 || b <= 0.0)
    {
        return Err(
            "ratios require nonnegative finite numerators and positive finite denominators".into(),
        );
    }
    Ok(bootstrap_ratio_interval(pairs, seed, resamples))
}

/// The looks at which a timing session may end before its planned pairs, each
/// with the order statistic that bounds the median ratio there: after `n` pairs,
/// the `k`-th smallest ratio. A session stops at a look only when that ratio is
/// below the runtime ceiling.
pub const EARLY_LOOKS: [(usize, usize); 2] = [(10, 10), (20, 18)];

/// Median of the paired ratios `a / b` with the order statistics `n + 1 - k` and
/// `k` as its bounds. Whatever the ratios' distribution, the `k`-th smallest of
/// `n` independent ratios lies below their median at most as often as `k` or
/// more of `n` fair coin flips land heads, and likewise for the lower bound.
pub fn order_interval(pairs: &[(f64, f64)], k: usize) -> Result<Interval, String> {
    let n = pairs.len();
    if !(n / 2 < k && k <= n)
        || pairs
            .iter()
            .any(|&(a, b)| !a.is_finite() || !b.is_finite() || a <= 0.0 || b <= 0.0)
    {
        return Err("order bounds require positive finite pairs and k above half".into());
    }
    let mut ratios: Vec<f64> = pairs.iter().map(|&(a, b)| a / b).collect();
    ratios.sort_by(f64::total_cmp);
    Ok(Interval {
        median: median(&ratios),
        lower_95: ratios[n - k],
        upper_95: ratios[k - 1],
    })
}

fn bootstrap_ratio_interval(pairs: &[(f64, f64)], seed: u64, resamples: usize) -> Interval {
    let ratios: Vec<f64> = pairs.iter().map(|&(a, b)| a / b).collect();
    let mut rng = PyRandom::new(seed);
    let estimates: Vec<f64> = (0..resamples)
        .map(|_| median(&rng.choices(&ratios, ratios.len())))
        .collect();
    Interval {
        median: median(&ratios),
        lower_95: percentile(&estimates, 0.025),
        upper_95: percentile(&estimates, 0.975),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How often the `k`-th smallest of `n` independent ratios lies below the
    /// population median, whatever their distribution: at most the chance that `k`
    /// or more of `n` fair coin flips land heads, which this returns exactly.
    fn order_statistic_error(n: usize, k: usize) -> f64 {
        let mut binomial: u128 = 1;
        let mut tail: u128 = 0;
        for j in 0..=n {
            if j >= k {
                tail += binomial;
            }
            binomial = binomial * (n - j) as u128 / (j + 1) as u128;
        }
        tail as f64 / (1u128 << n) as f64
    }

    #[test]
    fn bootstrap_matches_the_recorded_python_values() {
        let pairs: Vec<(f64, f64)> = (0..30)
            .map(|i| (f64::from(100 + i * 7 % 13), f64::from(90 + i * 5 % 11)))
            .collect();
        let interval = ratio_interval(&pairs, 280, 4000).unwrap();
        assert_eq!(
            interval,
            Interval {
                median: 1.1178947368421053,
                lower_95: 1.0777491408934707,
                upper_95: 1.1497369023106843
            }
        );
        let small =
            ratio_interval(&[(1.5, 2.0), (3.0, 2.5), (7.25, 1.0)], (1 << 40) + 3, 50).unwrap();
        assert_eq!(
            small,
            Interval {
                median: 1.2,
                lower_95: 0.75,
                upper_95: 7.25
            }
        );
    }

    #[test]
    fn rejects_unusable_pairs() {
        assert!(ratio_interval(&[(1.0, 1.0)], 0, 10).is_err());
        assert!(ratio_interval(&[(1.0, 0.0), (1.0, 1.0)], 0, 10).is_err());
        assert!(ratio_interval(&[(f64::NAN, 1.0), (1.0, 1.0)], 0, 10).is_err());
        assert!(ratio_interval(&[(0.0, 1.0), (1.0, 1.0)], 0, 10).is_err());
    }

    #[test]
    fn nonnegative_ratios_keep_zero_and_mixed_samples() {
        assert_eq!(
            nonnegative_ratio_interval(&[(0.0, 776.0); 30], 280, 4000).unwrap(),
            Interval {
                median: 0.0,
                lower_95: 0.0,
                upper_95: 0.0
            }
        );
        assert_eq!(
            nonnegative_ratio_interval(
                &[(0.0, 2.0), (1.0, 2.0), (2.0, 2.0), (3.0, 2.0)],
                280,
                4000
            )
            .unwrap(),
            Interval {
                median: 0.75,
                lower_95: 0.0,
                upper_95: 1.5
            }
        );
        for invalid in [
            (-1.0, 1.0),
            (0.0, 0.0),
            (1.0, -1.0),
            (f64::NAN, 1.0),
            (f64::INFINITY, 1.0),
            (0.0, f64::NAN),
            (0.0, f64::INFINITY),
        ] {
            assert!(nonnegative_ratio_interval(&[invalid, (1.0, 1.0)], 280, 4000).is_err());
        }
        assert!(nonnegative_ratio_interval(&[(0.0, 1.0)], 280, 4000).is_err());
    }

    #[test]
    fn positive_ratio_results_are_bit_identical_for_both_domains() {
        let pairs: Vec<_> = (0..30)
            .map(|i| (f64::from(100 + i * 7 % 13), f64::from(90 + i * 5 % 11)))
            .collect();
        for (seed, resamples) in [(0, 50), (280, 4000), ((1 << 40) + 3, 100)] {
            let original = ratio_interval(&pairs, seed, resamples).unwrap();
            let nonnegative = nonnegative_ratio_interval(&pairs, seed, resamples).unwrap();
            assert_eq!(
                [original.median, original.lower_95, original.upper_95].map(f64::to_bits),
                [
                    nonnegative.median,
                    nonnegative.lower_95,
                    nonnegative.upper_95
                ]
                .map(f64::to_bits)
            );
        }
    }

    #[test]
    fn early_looks_spend_little_error() {
        assert_eq!(order_statistic_error(10, 10), 1.0 / 1024.0);
        assert_eq!(order_statistic_error(20, 18), 211.0 / 1_048_576.0);
        assert_eq!(order_statistic_error(4, 0), 1.0);
        let spent: f64 = EARLY_LOOKS
            .iter()
            .map(|&(n, k)| order_statistic_error(n, k))
            .sum();
        assert!(spent < 0.0012, "{spent}");
    }

    #[test]
    fn order_bounds_are_the_kth_ratios() {
        let pairs: Vec<(f64, f64)> = (1..=10).map(|i| (f64::from(i), 10.0)).collect();
        assert_eq!(
            order_interval(&pairs, 10).unwrap(),
            Interval {
                median: 0.55,
                lower_95: 0.1,
                upper_95: 1.0
            }
        );
        assert_eq!(order_interval(&pairs, 8).unwrap().upper_95, 0.8);
        assert!(order_interval(&pairs, 5).is_err());
        assert!(order_interval(&pairs, 11).is_err());
        assert!(order_interval(&[(0.0, 1.0), (1.0, 1.0)], 2).is_err());
    }

    #[test]
    fn percentiles_interpolate() {
        assert_eq!(percentile(&[4.0, 1.0, 3.0, 2.0], 0.5), 2.5);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 0.975), 2.95);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
    }
}
