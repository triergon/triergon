//! Work and size bounds, over core's [`WorkBudget`].

use triergon_core::{Error, Result, WorkBudget};

/// The largest bitmap libass allocates: its stride (the width rounded up to
/// 16) times its height stays within `i32::MAX - 16` bytes. A larger bitmap is
/// not drawn, in libass and here.
pub const MAX_BITMAP_BYTES: usize = i32::MAX as usize - crate::bitmap::ALIGNMENT;

/// Bounds on the work and sizes of one operation. Exceeding one is
/// [`Error::Policy`]; storage is charged separately to the [`Budget`] each value
/// is created with.
///
/// [`Budget`]: triergon_core::Budget
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Work units of one conversion, stroke, rasterisation, composition or blur:
    /// subdivision steps, emitted points, polyline segments, tiles and filter
    /// rows.
    pub work: u64,
    /// Points of one stroked outline.
    pub outline_points: usize,
    /// Bytes (stride × height) of one bitmap. The default is
    /// [`MAX_BITMAP_BYTES`], libass's own bound, so that only the budget limits
    /// what libass would draw; a lower value refuses larger bitmaps with
    /// `Error::Policy` instead.
    pub bitmap_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            work: 1 << 28,
            outline_points: 1 << 22,
            bitmap_bytes: MAX_BITMAP_BYTES,
        }
    }
}

impl Limits {
    /// The work budget of one operation.
    pub(crate) fn work(&self) -> WorkBudget {
        WorkBudget::new(self.work)
    }
}

/// Charges `units` before the work is done; running out is `Error::Policy`.
#[inline]
pub(crate) fn charge(work: &mut WorkBudget, units: u64) -> Result<()> {
    if work.try_charge(units) {
        Ok(())
    } else {
        Err(Error::Policy("raster work"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Limits, charge};
    use triergon_core::Error;

    #[test]
    fn work_beyond_the_limit_is_a_policy_error() {
        let limits = Limits {
            work: 3,
            ..Limits::default()
        };
        let mut work = limits.work();
        assert_eq!(charge(&mut work, 3), Ok(()));
        assert_eq!(charge(&mut work, 1), Err(Error::Policy("raster work")));
    }
}
