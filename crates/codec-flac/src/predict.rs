//! Signal restoration: residual plus linear prediction (RFC 9639 sections 9.2.5 and 9.2.6).
//!
//! The fixed predictors are linear predictors with small integer coefficients and no
//! shift, so one kernel serves both subframe types. It is instantiated per order, which
//! unrolls every sum.
//!
//! Each sample depends on the one before it, so a plain loop waits on that chain, and a
//! vectorised dot product over the history also waits for the stores of the last few
//! samples to reach memory. The kernel instead restores [`STEP`] samples at a time:
//! terms old enough to be long written are summed for all lanes of the step at once from
//! memory, the rest from the previous and current steps held in registers, oldest first,
//! so a single multiply-add per sample stays on the chain.
use crate::sample::Sample;

/// Most predictor coefficients (LPC order 32).
pub(crate) const MAX_ORDER: usize = 32;

/// Coefficients of fixed predictor orders 1 to 4, newest sample first.
const FIXED: [&[i32]; 4] = [&[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]];

/// Samples restored per step of the kernel.
const STEP: usize = 4;

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
    /// The sum a sample's prediction starts from, given its residual. A 64-bit sum
    /// starts from the residual scaled by the shift (`r + (p >> s)` is
    /// `((r << s) + p) >> s`), which takes the addition off the chain between samples.
    fn seed(residual: S, shift: u32) -> Self;
    fn add_product(self, sample: S, coefficient: i32) -> Self;
    fn plus(self, other: Self) -> Self;
    /// The restored sample of a sum started with `seed(residual, shift)`.
    fn restored(self, residual: S, shift: u32) -> S;
}

impl Sum<i32> for i32 {
    const ZERO: Self = 0;
    #[inline(always)]
    fn seed(_: i32, _: u32) -> Self {
        0
    }
    #[inline(always)]
    fn add_product(self, sample: i32, coefficient: i32) -> Self {
        self.wrapping_add(sample.wrapping_mul(coefficient))
    }
    #[inline(always)]
    fn plus(self, other: Self) -> Self {
        self.wrapping_add(other)
    }
    #[inline(always)]
    fn restored(self, residual: i32, shift: u32) -> i32 {
        residual.wrapping_add(self >> shift)
    }
}

impl Sum<i32> for i64 {
    const ZERO: Self = 0;
    #[inline(always)]
    fn seed(residual: i32, shift: u32) -> Self {
        i64::from(residual) << shift
    }
    #[inline(always)]
    fn add_product(self, sample: i32, coefficient: i32) -> Self {
        // A 47-bit seed and at most 32 products of 32 and 16 bits: no overflow.
        self + i64::from(sample) * i64::from(coefficient)
    }
    #[inline(always)]
    fn plus(self, other: Self) -> Self {
        self + other
    }
    #[inline(always)]
    fn restored(self, _: i32, shift: u32) -> i32 {
        (self >> shift) as i32
    }
}

impl Sum<i64> for i64 {
    const ZERO: Self = 0;
    #[inline(always)]
    fn seed(residual: i64, shift: u32) -> Self {
        residual.wrapping_shl(shift)
    }
    #[inline(always)]
    fn add_product(self, sample: i64, coefficient: i32) -> Self {
        self.wrapping_add(sample.wrapping_mul(i64::from(coefficient)))
    }
    #[inline(always)]
    fn plus(self, other: Self) -> Self {
        self.wrapping_add(other)
    }
    #[inline(always)]
    fn restored(self, _: i64, shift: u32) -> i64 {
        self >> shift
    }
}

/// Sample types the kernels restore.
pub(crate) trait Restore: Sample {
    /// Adds the prediction to every sample after the first `coefficients.len()` (the
    /// warm-up), in order. `coefficients[j]` weighs the sample `j + 1` before.
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
    let start = N.max(STEP).min(samples.len());
    for i in N..start {
        restore_one::<S, A, N>(samples, i, coefficients, shift);
    }
    let Some(&first) = samples[..start].last_chunk::<STEP>() else {
        return;
    };
    let mut previous = first;
    let mut i = start;
    loop {
        let (past, rest) = samples.split_at_mut(i);
        let (Some(history), Some(current)) = (past.last_chunk::<N>(), rest.first_chunk_mut())
        else {
            break;
        };
        restore_step::<S, A, N>(history, &previous, current, coefficients, shift);
        previous = *current;
        i += STEP;
    }
    for i in i..samples.len() {
        restore_one::<S, A, N>(samples, i, coefficients, shift);
    }
}

/// Restores `current`, the [`STEP`] samples after `history` (the last `N` samples, whose
/// newest [`STEP`] are `previous`, kept in registers).
#[inline(always)]
fn restore_step<S: Sample, A: Sum<S>, const N: usize>(
    history: &[S; N],
    previous: &[S; STEP],
    current: &mut [S; STEP],
    coefficients: &[i32; N],
    shift: u32,
) {
    // Lane k predicts current[k] from the sample j + 1 before it. Samples before the
    // previous step (j >= 2 * STEP - 1) are summed for all lanes at once: lane k reads
    // history[N - 1 - j + k]. Every sum is split in two by the parity of j, which halves
    // the chains of dependent multiply-adds.
    let near = N.min(2 * STEP - 1);
    let mut sums = [[A::ZERO; STEP]; 2];
    for (j, &c) in coefficients.iter().enumerate().skip(near) {
        let window = &history[N - 1 - j..][..STEP];
        for (sum, &x) in sums[j % 2].iter_mut().zip(window) {
            *sum = sum.add_product(x, c);
        }
    }
    for k in 0..STEP {
        let mut partial = [sums[0][k].plus(A::seed(current[k], shift)), sums[1][k]];
        for j in (1..near).rev() {
            let x = sample_before(history, previous, current, k, j);
            partial[j % 2] = partial[j % 2].add_product(x, coefficients[j]);
        }
        // The newest sample comes last: only this multiply-add waits for lane k - 1.
        let newest = sample_before(history, previous, current, k, 0);
        let sum = partial[0]
            .plus(partial[1])
            .add_product(newest, coefficients[0]);
        current[k] = sum.restored(current[k], shift);
    }
}

/// The sample `j + 1` before lane `k` of the step: in `current` (lanes already restored),
/// `previous`, or `history` before that.
#[inline(always)]
fn sample_before<S: Sample, const N: usize>(
    history: &[S; N],
    previous: &[S; STEP],
    current: &[S; STEP],
    k: usize,
    j: usize,
) -> S {
    match (k + STEP).checked_sub(j + 1) {
        Some(at) if at >= STEP => current[at - STEP],
        Some(at) => previous[at],
        None => history[N + k - 1 - j],
    }
}

/// Restores sample `i` alone.
fn restore_one<S: Sample, A: Sum<S>, const N: usize>(
    samples: &mut [S],
    i: usize,
    coefficients: &[i32; N],
    shift: u32,
) {
    let (past, now) = samples.split_at_mut(i);
    if let (Some(history), Some(sample)) = (past.last_chunk::<N>(), now.first_mut()) {
        let sum = history
            .iter()
            .rev()
            .zip(coefficients)
            .fold(A::seed(*sample, shift), |sum, (&s, &c)| {
                sum.add_product(s, c)
            });
        *sample = sum.restored(*sample, shift);
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
            let residual: Vec<i32> = (0..203).map(|_| next() >> 1).collect();
            let wide: Vec<i64> = residual.iter().map(|&r| i64::from(r)).collect();
            let coefficients: Vec<i64> = newest_first.iter().map(|&c| i64::from(c)).collect();
            let expected = reference(&wide, &coefficients, 11);
            for accumulator in [Accumulator::Narrow, Accumulator::Wide] {
                let mut samples = residual.clone();
                i32::restore(&mut samples, &newest_first, 11, accumulator);
                let got: Vec<i64> = samples.iter().map(|&s| i64::from(s)).collect();
                assert_eq!(got, expected, "order {order} {accumulator:?}");
            }
            let mut samples = wide.clone();
            i64::restore(&mut samples, &newest_first, 11, Accumulator::Wide);
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
