//! Exact signed time arithmetic; unknown timestamps are represented by `Option`.
use crate::{Error, Result};
use std::num::NonZeroU32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeBase {
    pub numerator: NonZeroU32,
    pub denominator: NonZeroU32,
}

impl TimeBase {
    pub fn new(numerator: u32, denominator: u32) -> Result<Self> {
        Ok(Self {
            numerator: NonZeroU32::new(numerator).ok_or(Error::Invalid("zero timebase"))?,
            denominator: NonZeroU32::new(denominator).ok_or(Error::Invalid("zero timebase"))?,
        })
    }

    /// Rescale with truncation toward zero. No floating point or sentinel timestamps.
    pub fn rescale(self, ticks: i64, to: Self) -> Result<i64> {
        let numerator =
            i128::from(ticks) * i128::from(self.numerator.get()) * i128::from(to.denominator.get());
        let denominator = i128::from(self.denominator.get()) * i128::from(to.numerator.get());
        i64::try_from(numerator / denominator).map_err(|_| Error::Overflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negative_and_extreme_time_is_not_saturated() {
        let ms = TimeBase::new(1, 1000).unwrap();
        let ns = TimeBase::new(1, 1_000_000_000).unwrap();
        assert_eq!(ms.rescale(-21, ns).unwrap(), -21_000_000);
        assert!(matches!(ms.rescale(i64::MAX, ns), Err(Error::Overflow)));
        assert_eq!(ns.rescale(-1, ms).unwrap(), 0);
        assert!(TimeBase::new(0, 1).is_err());
    }
}
