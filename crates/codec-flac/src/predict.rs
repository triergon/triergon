//! Signal restoration: residual plus linear prediction (RFC 9639 sections 9.2.5 and 9.2.6).
//!
//! The fixed predictors are linear predictors with small integer coefficients and no
//! shift, so one kernel serves both subframe types. It is instantiated per order, which
//! unrolls the dot product; the history is read from the samples already restored.
use crate::sample::Sample;

/// Most predictor coefficients (LPC order 32).
pub(crate) const MAX_ORDER: usize = 32;

/// Coefficients of fixed predictor orders 1 to 4, oldest sample first.
const FIXED: [&[i32]; 4] = [&[1], &[-1, 2], &[1, -3, 3], &[-1, 4, -6, 4]];

/// How a prediction is accumulated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Accumulator {
    /// 32-bit wrapping arithmetic: exact whenever the sum cannot exceed 32 bits, and for
    /// an unshifted predictor whenever the restored sample fits (arithmetic mod 2^32).
    Narrow,
    /// 64-bit arithmetic.
    Wide,
}

impl Accumulator {
    /// The accumulator RFC 9639 section 9.2.5 allows for `sample_bits`-bit samples and
    /// `order` coefficients of `precision` bits: 32 bits suffice when the sum of the
    /// products cannot exceed them.
    pub(crate) fn for_lpc(sample_bits: u32, precision: u32, order: usize) -> Accumulator {
        let order_bits = order.next_power_of_two().trailing_zeros();
        if sample_bits + precision + order_bits <= 32 {
            Accumulator::Narrow
        } else {
            Accumulator::Wide
        }
    }
}

/// A prediction sum over one sample type.
trait Sum<S>: Copy {
    const ZERO: Self;
    fn add_product(self, sample: S, coefficient: i32) -> Self;
    fn prediction(self, shift: u32) -> S;
}

impl Sum<i32> for i32 {
    const ZERO: Self = 0;
    #[inline(always)]
    fn add_product(self, sample: i32, coefficient: i32) -> Self {
        self.wrapping_add(sample.wrapping_mul(coefficient))
    }
    #[inline(always)]
    fn prediction(self, shift: u32) -> i32 {
        self >> shift
    }
}

impl Sum<i32> for i64 {
    const ZERO: Self = 0;
    #[inline(always)]
    fn add_product(self, sample: i32, coefficient: i32) -> Self {
        // At most 32 products of 32 and 16 bits: no overflow.
        self + i64::from(sample) * i64::from(coefficient)
    }
    #[inline(always)]
    fn prediction(self, shift: u32) -> i32 {
        (self >> shift) as i32
    }
}

impl Sum<i64> for i64 {
    const ZERO: Self = 0;
    #[inline(always)]
    fn add_product(self, sample: i64, coefficient: i32) -> Self {
        self.wrapping_add(sample.wrapping_mul(i64::from(coefficient)))
    }
    #[inline(always)]
    fn prediction(self, shift: u32) -> i64 {
        self >> shift
    }
}

/// Sample types the kernels restore.
pub(crate) trait Restore: Sample {
    /// Adds the prediction to every sample after the first `coefficients.len()` (the
    /// warm-up), in order. `coefficients` run oldest sample first.
    fn restore(samples: &mut [Self], coefficients: &[i32], shift: u32, accumulator: Accumulator);
}

impl Restore for i32 {
    fn restore(samples: &mut [i32], coefficients: &[i32], shift: u32, accumulator: Accumulator) {
        match accumulator {
            Accumulator::Narrow => dispatch::<i32, i32>(samples, coefficients, shift),
            Accumulator::Wide => dispatch::<i32, i64>(samples, coefficients, shift),
        }
    }
}

impl Restore for i64 {
    /// 33-bit samples always accumulate in 64 bits.
    fn restore(samples: &mut [i64], coefficients: &[i32], shift: u32, _: Accumulator) {
        dispatch::<i64, i64>(samples, coefficients, shift);
    }
}

/// Fixed predictor of `order` (1 to 4) over `samples`.
pub(crate) fn restore_fixed<S: Restore>(samples: &mut [S], order: usize) {
    if let Some(coefficients) = order.checked_sub(1).and_then(|i| FIXED.get(i)) {
        S::restore(samples, coefficients, 0, Accumulator::Narrow);
    }
}

/// Selects the kernel of the coefficient count.
fn dispatch<S: Sample, A: Sum<S>>(samples: &mut [S], coefficients: &[i32], shift: u32) {
    macro_rules! orders {
        ($($n:literal)*) => {
            match coefficients.len() {
                $($n => {
                    if let Ok(coefficients) = <&[i32; $n]>::try_from(coefficients) {
                        kernel::<S, A, $n>(samples, coefficients, shift);
                    }
                })*
                _ => {}
            }
        };
    }
    orders!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
}

fn kernel<S: Sample, A: Sum<S>, const N: usize>(
    samples: &mut [S],
    coefficients: &[i32; N],
    shift: u32,
) {
    for i in N..samples.len() {
        let (past, now) = samples.split_at_mut(i);
        let (Some(history), Some(sample)) = (past.last_chunk::<N>(), now.first_mut()) else {
            return;
        };
        let sum = history
            .iter()
            .zip(coefficients)
            .fold(A::ZERO, |sum, (&s, &c)| sum.add_product(s, c));
        *sample = sample.wrapping_add(sum.prediction(shift));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The predictor as RFC 9639 writes it, newest coefficient first, in 64-bit arithmetic.
    fn reference(residual: &[i64], coefficients: &[i64], shift: u32) -> Vec<i64> {
        let mut out = residual.to_vec();
        for i in coefficients.len()..out.len() {
            let sum: i64 = coefficients
                .iter()
                .enumerate()
                .map(|(j, c)| c * out[i - 1 - j])
                .sum();
            out[i] += sum >> shift;
        }
        out
    }

    #[test]
    fn every_order_and_accumulator_matches_the_reference() {
        let mut seed = 1u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as i32 - 32768
        };
        for order in 1..=MAX_ORDER {
            // |coefficients| sum below 2^shift: a contracting, bounded predictor.
            let newest_first: Vec<i32> = (0..order).map(|_| next() >> 11).collect();
            let oldest_first: Vec<i32> = newest_first.iter().rev().copied().collect();
            let residual: Vec<i32> = (0..200).map(|_| next() >> 1).collect();
            let wide: Vec<i64> = residual.iter().map(|&r| i64::from(r)).collect();
            let coefficients: Vec<i64> = newest_first.iter().map(|&c| i64::from(c)).collect();
            let expected = reference(&wide, &coefficients, 11);
            for accumulator in [Accumulator::Narrow, Accumulator::Wide] {
                let mut samples = residual.clone();
                i32::restore(&mut samples, &oldest_first, 11, accumulator);
                let got: Vec<i64> = samples.iter().map(|&s| i64::from(s)).collect();
                assert_eq!(got, expected, "order {order} {accumulator:?}");
            }
            let mut samples = wide.clone();
            i64::restore(&mut samples, &oldest_first, 11, Accumulator::Wide);
            assert_eq!(samples, expected, "order {order} i64");
        }
    }

    #[test]
    fn fixed_orders_are_the_rfc_polynomials() {
        let residual = [5i32, -3, 7, 1, 0, 2, -9, 4, 4, 4];
        for order in 0..=4 {
            let mut samples = residual;
            restore_fixed(&mut samples, order);
            let coefficients: &[i64] =
                [&[][..], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]][order];
            let wide: Vec<i64> = residual.iter().map(|&r| i64::from(r)).collect();
            let expected = reference(&wide, coefficients, 0);
            let got: Vec<i64> = samples.iter().map(|&s| i64::from(s)).collect();
            assert_eq!(got, expected, "order {order}");
        }
    }
}
