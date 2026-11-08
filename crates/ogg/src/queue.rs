//! A first-in, first-out queue in charged storage.
use triergon_core::{Budget, BudgetVec, Error, Result};

/// A first-in, first-out queue whose slots are charged to a pool. Taken slots are reused
/// once the queue drains, or compacted before it grows.
#[derive(Debug)]
pub(crate) struct Queue<T> {
    slots: BudgetVec<Option<T>>,
    /// The first slot still queued.
    head: usize,
}

impl<T> Queue<T> {
    pub(crate) fn new(pool: &Budget) -> Self {
        Queue {
            slots: BudgetVec::new(pool),
            head: 0,
        }
    }

    /// Room for `additional` more values without growing. A refusal of the pool changes
    /// nothing.
    pub(crate) fn reserve(&mut self, additional: usize) -> Result<()> {
        if self.slots.capacity() - self.slots.len() >= additional {
            return Ok(());
        }
        self.retain(|_| true);
        let needed = self
            .slots
            .len()
            .checked_add(additional)
            .ok_or(Error::Overflow)?;
        if needed <= self.slots.capacity() {
            return Ok(());
        }
        let doubled = needed.max(self.slots.capacity().saturating_mul(2));
        self.slots
            .try_reserve_exact(doubled - self.slots.len())
            .or_else(|_| self.slots.try_reserve_exact(additional))
    }

    pub(crate) fn push(&mut self, value: T) -> Result<()> {
        self.reserve(1)?;
        self.slots
            .try_push(Some(value))
            .map_err(|refused| refused.error)
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        let value = self.slots.get_mut(self.head)?.take();
        self.head += 1;
        if self.head == self.slots.len() {
            self.clear();
        }
        value
    }

    /// Keep the queued values `keep` accepts, in order.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        let mut kept = 0;
        for index in self.head..self.slots.len() {
            if let Some(value) = self.slots[index].take().filter(&mut keep) {
                self.slots[kept] = Some(value);
                kept += 1;
            }
        }
        self.slots.truncate(kept);
        self.head = 0;
    }

    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.head = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_leave_in_order_and_storage_is_reused() {
        let pool = Budget::new(1 << 16).unwrap();
        let mut queue = Queue::new(&pool);
        for value in 0..5 {
            queue.push(value).unwrap();
        }
        assert_eq!((queue.pop(), queue.pop()), (Some(0), Some(1)));
        queue.retain(|value| value % 2 == 0);
        queue.push(5).unwrap();
        let capacity = queue.slots.capacity();
        let drained: Vec<_> = std::iter::from_fn(|| queue.pop()).collect();
        assert_eq!(drained, [2, 4, 5]);
        queue.reserve(capacity).unwrap();
        assert_eq!(queue.slots.capacity(), capacity);
    }

    #[test]
    fn a_refused_growth_keeps_the_queue() {
        let pool = Budget::new(Budget::control_storage_bytes() + 64).unwrap();
        let mut queue = Queue::new(&pool);
        queue.push(1u64).unwrap();
        assert!(queue.reserve(1 << 10).is_err());
        assert_eq!((queue.pop(), queue.pop()), (Some(1), None));
    }
}
