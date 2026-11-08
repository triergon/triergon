//! Duplicate detection for Matroska packets by their ReadOrder field.

use triergon_core::{Budget, BudgetVec, Result};

/// libass's ReadOrder bitmap. It is absent until a packet needs it, and
/// dropped whenever an identifier falls outside 0 to 2^23·10 − 1; while
/// absent, duplicates are found by scanning the held events instead, which
/// differs for events that were rejected after their identifier was marked.
#[derive(Debug)]
pub(crate) struct ReadOrder {
    words: Option<BudgetVec<u32>>,
    budget: Budget,
}

/// The largest identifier plus one that the bitmap holds (10 MiB of bits).
const CAPACITY: i32 = 10 * 1024 * 1024 * 8;

impl ReadOrder {
    pub(crate) fn new(budget: &Budget) -> Self {
        Self {
            words: None,
            budget: budget.clone(),
        }
    }

    pub(crate) const fn is_active(&self) -> bool {
        self.words.is_some()
    }

    pub(crate) fn drop_bitmap(&mut self) {
        self.words = None;
    }

    /// Marks `id`, returning whether it was marked before, or `None` (and
    /// no bitmap) when `id` is out of range.
    pub(crate) fn test_and_set(&mut self, id: i32) -> Result<Option<bool>> {
        if !(0..CAPACITY).contains(&id) {
            self.words = None;
            return Ok(None);
        }
        let index = (id >> 5) as usize;
        let words = self
            .words
            .get_or_insert_with(|| BudgetVec::new(&self.budget));
        if index >= words.len() {
            // libass's growth: ((id + 31) / 32 + 1) * 2 words.
            let len = (((id + 31) / 32 + 1) * 2) as usize;
            if let Err(error) = words.try_resize(len, 0) {
                self.words = None;
                return Err(error);
            }
        }
        let bit = 1 << (id & 31);
        let word = &mut words.as_mut_slice()[index];
        let seen = *word & bit != 0;
        *word |= bit;
        Ok(Some(seen))
    }

    pub(crate) fn clear(&mut self, id: i32) {
        if let (Ok(id), Some(words)) = (u32::try_from(id), self.words.as_mut())
            && let Some(word) = words.as_mut_slice().get_mut((id >> 5) as usize)
        {
            *word &= !(1 << (id & 31));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReadOrder;
    use triergon_core::Budget;

    #[test]
    fn identifiers_are_marked_cleared_and_bounded() {
        let mut order = ReadOrder::new(&Budget::new(1 << 24).unwrap());
        assert_eq!(order.test_and_set(5), Ok(Some(false)));
        assert_eq!(order.test_and_set(5), Ok(Some(true)));
        order.clear(5);
        assert_eq!(order.test_and_set(5), Ok(Some(false)));
        assert_eq!(order.test_and_set(-1), Ok(None));
        assert!(!order.is_active());
    }
}
