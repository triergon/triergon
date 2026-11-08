//! The written subtitles waiting for output.

use triergon_core::{Budget, BudgetVec, Result};

/// Subtitles the decoder holds; a page beyond them is lost.
pub(super) const CAPACITY: usize = 25;

#[derive(Debug)]
pub(super) struct Slot {
    pub(super) pts: Option<i64>,
    /// The ASS event line, empty for an empty subtitle.
    pub(super) text: BudgetVec<u8>,
}

/// A ring of [`CAPACITY`] reused slots, oldest first.
#[derive(Debug)]
pub(super) struct Queue {
    slots: BudgetVec<Slot>,
    head: usize,
    len: usize,
}

impl Queue {
    pub(super) fn new(pool: &Budget) -> Result<Self> {
        let mut slots = BudgetVec::new(pool);
        slots.try_reserve_exact(CAPACITY)?;
        for _ in 0..CAPACITY {
            let slot = Slot {
                pts: None,
                text: BudgetVec::new(pool),
            };
            slots.try_push(slot).map_err(|e| e.error)?;
        }
        Ok(Self {
            slots,
            head: 0,
            len: 0,
        })
    }

    /// The cleared slot after the newest, or `None` when full; it joins the
    /// queue with [`push_back`](Self::push_back).
    pub(super) fn back(&mut self) -> Option<&mut Slot> {
        if self.len == CAPACITY {
            return None;
        }
        let slot = &mut self.slots.as_mut_slice()[(self.head + self.len) % CAPACITY];
        slot.text.clear();
        Some(slot)
    }

    pub(super) fn push_back(&mut self) {
        self.len += 1;
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Keeps the oldest `len` subtitles, dropping the newer ones.
    pub(super) fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }

    pub(super) fn pop_front(&mut self) -> Option<&Slot> {
        if self.len == 0 {
            return None;
        }
        let index = self.head;
        self.head = (self.head + 1) % CAPACITY;
        self.len -= 1;
        self.slots.as_slice().get(index)
    }
}
