//! In-place sample ordering for layouts that cannot use the linear merge.
use triergon_core::WorkBudget;

pub(crate) struct SampleSort {
    next_parent: usize,
    end: usize,
    sift: Option<usize>,
}

impl SampleSort {
    pub(crate) fn new(len: usize) -> Self {
        Self {
            next_parent: len / 2,
            end: len,
            sift: None,
        }
    }

    pub(crate) fn poll<T, K: Ord>(
        &mut self,
        rows: &mut [T],
        key: impl Fn(&T) -> K,
        work: &mut WorkBudget,
    ) -> bool {
        loop {
            if let Some(parent) = self.sift {
                if parent >= self.end / 2 {
                    self.sift = None;
                    continue;
                }
                if !work.try_charge(1) {
                    return false;
                }
                let mut child = 2 * parent + 1;
                if child + 1 < self.end && key(&rows[child]) < key(&rows[child + 1]) {
                    child += 1;
                }
                if key(&rows[parent]) < key(&rows[child]) {
                    rows.swap(parent, child);
                    self.sift = Some(child);
                } else {
                    self.sift = None;
                }
            } else if self.next_parent != 0 {
                self.next_parent -= 1;
                self.sift = Some(self.next_parent);
            } else if self.end > 1 {
                if !work.try_charge(1) {
                    return false;
                }
                self.end -= 1;
                rows.swap(0, self.end);
                self.sift = Some(0);
            } else {
                return true;
            }
        }
    }
}
