//! A queue of caption frames: entries over one byte arena that is compacted as
//! frames leave.

use super::CaptionFrame;
use crate::cc::text::Text;
use triergon_core::{Budget, BudgetVec, Result};

/// One queued frame: its times and its bytes in the arena.
#[derive(Clone, Copy, Debug)]
pub(super) struct Entry {
    pub(super) pts: Option<i64>,
    pub(super) duration: Option<i64>,
    start: usize,
    end: usize,
}

/// Frames in order. Frames leave from the front, or from anywhere for the
/// reorder buffer.
#[derive(Debug)]
pub(super) struct FrameQueue {
    entries: BudgetVec<Entry>,
    /// Entries before `head` have left.
    head: usize,
    /// The bytes of the live entries.
    live: usize,
    data: Text,
    spare: Text,
}

impl FrameQueue {
    pub(super) fn new(pool: &Budget) -> Self {
        Self {
            entries: BudgetVec::new(pool),
            head: 0,
            live: 0,
            data: Text::new(pool),
            spare: Text::new(pool),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len() - self.head
    }

    /// Queues a frame; a frame without caption bytes is left out.
    pub(super) fn push(
        &mut self,
        pts: Option<i64>,
        duration: Option<i64>,
        bytes: &[u8],
    ) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.compact()?;
        let start = self.data.len();
        self.data.put(bytes)?;
        if self.entries.len() == self.entries.capacity() {
            self.entries.try_reserve_exact(self.entries.len().max(16))?;
        }
        self.entries
            .try_push(Entry {
                pts,
                duration,
                start,
                end: self.data.len(),
            })
            .map_err(|rejected| rejected.error)?;
        self.live += bytes.len();
        Ok(())
    }

    /// The live entries, front first.
    pub(super) fn entries(&self) -> &[Entry] {
        &self.entries.as_slice()[self.head..]
    }

    /// Takes the front frame; it stays readable until the queue next changes.
    pub(super) fn pop_front(&mut self) -> Result<Option<CaptionFrame<'_>>> {
        self.compact()?;
        let Some(&entry) = self.entries.as_slice().get(self.head) else {
            return Ok(None);
        };
        self.head += 1;
        self.live -= entry.end - entry.start;
        Ok(Some(CaptionFrame {
            pts: entry.pts,
            duration: entry.duration,
            data: &self.data.as_bytes()[entry.start..entry.end],
        }))
    }

    /// Removes the live entry at `index` (from the front), keeping the order of
    /// the others, and hands it and its bytes to `take`.
    pub(super) fn remove(
        &mut self,
        index: usize,
        take: impl FnOnce(Entry, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let at = self.head + index;
        let entry = self.entries.as_slice()[at];
        take(entry, &self.data.as_bytes()[entry.start..entry.end])?;
        self.entries.as_mut_slice()[at..].rotate_left(1);
        self.entries.pop();
        self.live -= entry.end - entry.start;
        Ok(())
    }

    /// Drops the bytes of frames that have left once they outweigh the rest.
    fn compact(&mut self) -> Result<()> {
        if self.head == self.entries.len() {
            self.entries.clear();
            self.head = 0;
            self.data.clear();
            return Ok(());
        }
        if self.data.len() < 4096 || self.data.len() < 2 * self.live {
            return Ok(());
        }
        self.spare.clear();
        let head = self.head;
        let mut kept = 0;
        for i in head..self.entries.len() {
            let mut entry = self.entries.as_slice()[i];
            let start = self.spare.len();
            self.spare
                .put(&self.data.as_bytes()[entry.start..entry.end])?;
            entry.start = start;
            entry.end = self.spare.len();
            self.entries.as_mut_slice()[kept] = entry;
            kept += 1;
        }
        while self.entries.len() > kept {
            self.entries.pop();
        }
        self.head = 0;
        std::mem::swap(&mut self.data, &mut self.spare);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(queue: &FrameQueue) -> usize {
        queue.entries().iter().map(|e| e.end - e.start).sum()
    }

    #[test]
    fn the_live_bytes_are_kept_as_frames_come_and_go() {
        let pool = Budget::new(1 << 24).expect("pool");
        let mut queue = FrameQueue::new(&pool);
        for n in 0..5000usize {
            queue
                .push(Some(n as i64), None, &vec![n as u8; 1 + n % 7])
                .expect("push");
            match n % 5 {
                0 => {
                    queue.pop_front().expect("pop");
                }
                1 => queue
                    .remove(queue.len() / 2, |_, _| Ok(()))
                    .expect("remove"),
                _ => {}
            }
            assert_eq!(queue.live, live(&queue));
        }
        while queue.pop_front().expect("pop").is_some() {}
        assert_eq!(queue.live, 0);
    }
}
