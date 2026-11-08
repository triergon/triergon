//! Budgeted text buffers that grow by doubling.

use triergon_core::{Budget, BudgetVec, Result};

/// A byte buffer charged to a budget. It keeps its capacity when cleared, so a
/// decoder reaches its steady size after a few captions.
#[derive(Debug)]
pub(crate) struct Text(BudgetVec<u8>);

impl Text {
    pub(crate) fn new(pool: &Budget) -> Self {
        Self(BudgetVec::new(pool))
    }

    pub(crate) fn put(&mut self, bytes: &[u8]) -> Result<()> {
        let needed = self.0.len() + bytes.len();
        if needed > self.0.capacity() {
            let target = needed.max(self.0.capacity() * 2).max(64);
            self.0.try_reserve_exact(target - self.0.len())?;
        }
        self.0.try_extend_from_slice(bytes)
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Text {
    /// Drops the first `count` bytes, keeping the rest.
    pub(crate) fn drop_front(&mut self, count: usize) -> Result<()> {
        let len = self.0.len();
        self.0.as_mut_slice().copy_within(count.min(len)..len, 0);
        self.0.try_resize(len - count.min(len), 0)
    }
}
