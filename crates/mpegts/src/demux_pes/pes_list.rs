//! A list the observer fills for one PES and hands out with it.

use triergon_core::{Budget, BudgetVec, Error, PushError, Result};

/// A list whose capacity, for admission, payload runs and growth work, is the
/// capacity its growth has planned: exactly that of a new list. A list handed
/// back for reuse keeps its allocation, so planned growth within it allocates
/// nothing; only the allocation, never the observed capacity, differs.
#[derive(Debug)]
pub(super) struct PesList<T> {
    items: BudgetVec<T>,
    planned: usize,
    /// The planned capacity of the list last handed out: what a new list
    /// grew to for that PES.
    handed_out: usize,
}

/// Room for a payload run: in the list's own allocation, or in a replacement
/// allocated before the run commits.
#[derive(Debug)]
pub(crate) struct ListGrowth<T> {
    target: usize,
    replacement: Option<BudgetVec<T>>,
}

impl<T> PesList<T> {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            items: BudgetVec::new(budget),
            planned: 0,
            handed_out: 0,
        }
    }
    pub(super) fn len(&self) -> usize {
        self.items.len()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    #[cfg(test)]
    pub(super) fn as_slice(&self) -> &[T] {
        self.items.as_slice()
    }
    /// The planned capacity, which admission and growth see.
    pub(super) fn capacity(&self) -> usize {
        self.planned
    }
    /// The capacity of the allocation, which the pool is charged for.
    pub(super) fn allocated(&self) -> usize {
        self.items.capacity()
    }
    pub(super) fn clear(&mut self) {
        self.items.clear();
    }
    /// Plans room for `additional` more elements, allocating only beyond the
    /// current allocation, as `BudgetVec::try_reserve_exact` would for a new list.
    pub(super) fn try_reserve_exact(&mut self, additional: usize) -> Result<()> {
        let target = self.len().checked_add(additional).ok_or(Error::Overflow)?;
        if target > self.items.capacity() {
            self.items.try_reserve_exact(target - self.len())?;
        }
        self.planned = self.planned.max(target);
        Ok(())
    }
    /// Pushes within the planned capacity, or grows it as `BudgetVec::try_push`
    /// grows a new list.
    pub(super) fn try_push(&mut self, value: T) -> std::result::Result<(), PushError<T>> {
        if self.len() == self.planned {
            let preferred = self.planned.saturating_mul(2).max(4);
            if self.items.capacity() < preferred {
                if self.len() == self.items.capacity() {
                    // A new list's own growth, including its fallback under pressure.
                    self.items.try_push(value)?;
                    self.planned = self.items.capacity();
                    return Ok(());
                }
                if let Err(error) = self.items.try_reserve_exact(preferred - self.len()) {
                    return Err(PushError { error, value });
                }
            }
            self.planned = preferred;
        }
        self.items.try_push(value)
    }
    pub(super) fn try_extend_with(
        &mut self,
        count: usize,
        make: impl FnMut(usize) -> T,
    ) -> Result<()> {
        self.try_reserve_exact(count)?;
        self.items.try_extend_with(count, make)
    }
    /// Room for `target` elements, prepared before a payload run commits so
    /// that a refused allocation leaves the list as it was.
    pub(super) fn prepare_growth(&self, target: usize) -> Option<ListGrowth<T>> {
        let replacement = if target > self.items.capacity() {
            let mut replacement = BudgetVec::new(self.items.budget());
            replacement.try_reserve_exact(target).ok()?;
            Some(replacement)
        } else {
            None
        };
        Some(ListGrowth {
            target,
            replacement,
        })
    }
    pub(super) fn commit_growth(&mut self, growth: ListGrowth<T>) -> Result<()> {
        if let Some(mut replacement) = growth.replacement {
            replacement.try_append(&mut self.items)?;
            self.items = replacement;
        }
        self.planned = self.planned.max(growth.target);
        Ok(())
    }
    /// The filled list, leaving a new, empty one in its place.
    pub(super) fn take(&mut self) -> BudgetVec<T> {
        let empty = BudgetVec::new(self.items.budget());
        self.handed_out = std::mem::take(&mut self.planned);
        std::mem::replace(&mut self.items, empty)
    }
    /// Adopts `list`'s allocation for the next fill when this list holds
    /// nothing and `list` is larger, charged to the same pool and of a size
    /// comparable to what a new list grew to for the last PES. The planned
    /// capacity is unchanged;
    /// `list`'s elements are dropped either way.
    pub(super) fn reuse(&mut self, mut list: BudgetVec<T>) {
        list.clear();
        if self.items.is_empty()
            && list.capacity() > self.items.capacity()
            && list.fits_reuse_for(self.handed_out)
            && list.budget().same_pool(self.items.budget())
        {
            self.items = list;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reused_list_of_comparable_size_plans_growth_as_a_new_one_without_allocating() {
        let pool = Budget::new(64 * 1024).unwrap();
        let mut fresh = PesList::new(&pool);
        let mut reused = PesList::new(&pool);
        // The last PES planned 32 entries: a list of more than twice that is
        // dropped, one within it adopted.
        reused.try_reserve_exact(32).unwrap();
        drop(reused.take());
        let mut larger = BudgetVec::new(&pool);
        larger.try_reserve_exact(65).unwrap();
        reused.reuse(larger);
        assert_eq!(reused.allocated(), 0);
        let mut previous = BudgetVec::new(&pool);
        previous.try_reserve_exact(32).unwrap();
        previous.try_push(7_u64).unwrap();
        reused.reuse(previous);
        assert_eq!(reused.allocated(), 32);
        assert!(reused.is_empty());
        let storage = reused.as_slice().as_ptr();

        for list in [&mut fresh, &mut reused] {
            assert_eq!(list.capacity(), 0);
            list.try_push(1).unwrap();
            assert_eq!(list.capacity(), 4);
            list.try_reserve_exact(5).unwrap();
            assert_eq!(list.capacity(), 6);
            let growth = list.prepare_growth(12).unwrap();
            list.commit_growth(growth).unwrap();
            assert_eq!(list.capacity(), 12);
            list.try_extend_with(3, |index| index as u64).unwrap();
            assert_eq!(list.capacity(), 12);
            assert_eq!(list.as_slice(), [1, 0, 1, 2]);
        }
        assert_eq!(fresh.allocated(), 12);
        assert_eq!(reused.allocated(), 32);
        assert_eq!(reused.as_slice().as_ptr(), storage);

        let filled = reused.take();
        assert_eq!(filled.capacity(), 32);
        assert_eq!((reused.capacity(), reused.allocated()), (0, 0));
        // A list given to a filled one, or charged to another pool, is dropped.
        fresh.reuse(filled);
        assert_eq!(fresh.allocated(), 12);
        let other = Budget::new(4096).unwrap();
        let mut foreign = BudgetVec::new(&other);
        foreign.try_reserve_exact(64).unwrap();
        reused.reuse(foreign);
        assert_eq!(reused.allocated(), 0);
    }
}
