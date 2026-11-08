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
    fn percentiles_interpolate() {
        assert_eq!(percentile(&[4.0, 1.0, 3.0, 2.0], 0.5), 2.5);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 0.975), 2.95);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
    }
}
