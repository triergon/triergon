//! A queue of caption frames: entries over one byte arena, compacted when frames
//! are added once those that left outweigh the rest. The newest frames can be
//! kept back until they are released. A change that fails leaves the frames as
//! they were, and room reserved ahead lets frames be queued without any
//! failing.

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

/// Frames in order. Frames leave from the front, those kept back not before
/// they are released.
#[derive(Debug)]
pub(super) struct FrameQueue {
    entries: BudgetVec<Entry>,
    /// Entries before `head` have left.
    head: usize,
    /// The last this many entries are kept back.
    kept: usize,
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
            kept: 0,
            live: 0,
            data: Text::new(pool),
            spare: Text::new(pool),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len() - self.head
    }

    /// The frames kept back.
    pub(super) fn kept(&self) -> usize {
        self.kept
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
        self.reserve(1, bytes.len())?;
        self.push_reserved(pts, duration, bytes);
        Ok(())
    }

    /// Makes room for `frames` more frames with `bytes` caption bytes in all:
    /// [`push_reserved`](Self::push_reserved) then takes them without
    /// allocating, until a pop.
    #[inline]
    pub(super) fn reserve(&mut self, frames: usize, bytes: usize) -> Result<()> {
        if self.head == self.entries.len() {
            self.clear();
        } else if self.data.len() >= 4096 && self.data.len() >= 2 * self.live {
            self.compact()?;
        }
        let spare = self.entries.capacity() - self.entries.len();
        if spare < frames {
            self.entries
                .try_reserve_exact(frames.max(self.entries.len()).max(16))?;
        }
        self.data.reserve(bytes)
    }

    /// Queues a frame in the room [`reserve`](Self::reserve) made; a frame
    /// without caption bytes is left out.
    #[inline]
    pub(super) fn push_reserved(&mut self, pts: Option<i64>, duration: Option<i64>, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        debug_assert!(self.entries.len() < self.entries.capacity());
        debug_assert_eq!(self.kept, 0, "a frame after one kept back is kept too");
        let start = self.data.len();
        // Within the room reserved, neither push allocates nor fails.
        let _ = self.data.put(bytes);
        let _ = self.entries.try_push(Entry {
            pts,
            duration,
            start,
            end: self.data.len(),
        });
        self.live += bytes.len();
    }

    /// Queues a frame in the room [`reserve`](Self::reserve) made, kept back
    /// until [`release`](Self::release); a frame without caption bytes is left
    /// out.
    pub(super) fn push_kept(&mut self, pts: Option<i64>, duration: Option<i64>, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let kept = self.kept;
        self.kept = 0;
        self.push_reserved(pts, duration, bytes);
        self.kept = kept + 1;
    }

    /// Lets the frames kept back leave, those without a duration with
    /// `duration`, except the `skip`th of them, which is dropped.
    pub(super) fn release(&mut self, duration: Option<i64>, skip: Option<usize>) {
        let first = self.entries.len() - self.kept;
        if let Some(at) = skip.filter(|&at| at < self.kept) {
            let entries = &mut self.entries.as_mut_slice()[first + at..];
            let dropped = entries[0];
            entries.rotate_left(1);
            self.entries.pop();
            self.live -= dropped.end - dropped.start;
        }
        for entry in &mut self.entries.as_mut_slice()[first..] {
            entry.duration = entry.duration.or(duration);
        }
        self.kept = 0;
    }

    /// The front frame, left in the queue.
    pub(super) fn front(&self) -> Option<CaptionFrame<'_>> {
        let entry = self.leaving()?;
        Some(CaptionFrame {
            pts: entry.pts,
            duration: entry.duration,
            data: &self.data.as_bytes()[entry.start..entry.end],
        })
    }

    /// The front entry, unless it is kept back.
    fn leaving(&self) -> Option<Entry> {
        (self.len() > self.kept).then(|| self.entries[self.head])
    }

    /// Takes the front frame; it stays readable until the queue next changes.
    pub(super) fn pop_front(&mut self) -> Option<CaptionFrame<'_>> {
        let entry = self.leaving()?;
        self.head += 1;
        self.live -= entry.end - entry.start;
        Some(CaptionFrame {
            pts: entry.pts,
            duration: entry.duration,
            data: &self.data.as_bytes()[entry.start..entry.end],
        })
    }

    /// Keeps the first `len` live frames, dropping the newer ones.
    pub(super) fn truncate(&mut self, len: usize) {
        while self.len() > len {
            if let Some(entry) = self.entries.pop() {
                self.live -= entry.end - entry.start;
            }
        }
        self.kept = self.kept.min(len);
        let end = self.entries.as_slice().last().map_or(0, |entry| entry.end);
        self.data.truncate(end);
    }

    /// Drops every frame.
    fn clear(&mut self) {
        self.entries.clear();
        self.head = 0;
        self.kept = 0;
        self.live = 0;
        self.data.clear();
    }

    /// Drops the bytes of the frames that have left, moving the live frames'
    /// bytes to the front of a fresh arena.
    #[cold]
    fn compact(&mut self) -> Result<()> {
        self.spare.clear();
        let live = &self.entries.as_slice()[self.head..];
        for entry in live {
            self.spare
                .put(&self.data.as_bytes()[entry.start..entry.end])?;
        }
        let mut start = 0;
        let head = self.head;
        let entries = self.entries.as_mut_slice();
        for kept in 0..entries.len() - head {
            let entry = entries[head + kept];
            let end = start + (entry.end - entry.start);
            entries[kept] = Entry {
                start,
                end,
                ..entry
            };
            start = end;
        }
        self.entries.truncate(self.entries.len() - head);
        self.head = 0;
        std::mem::swap(&mut self.data, &mut self.spare);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(queue: &FrameQueue) -> usize {
        queue.entries.as_slice()[queue.head..]
            .iter()
            .map(|e| e.end - e.start)
            .sum()
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
                    queue.pop_front();
                }
                1 => queue.truncate(queue.len().saturating_sub(1)),
                _ => {}
            }
            assert_eq!(queue.live, live(&queue));
        }
        while queue.pop_front().is_some() {}
        assert_eq!(queue.live, 0);
    }

    #[test]
    fn frames_queued_in_room_reserved_take_no_more_memory() {
        let pool = Budget::new(1 << 24).expect("pool");
        let mut queue = FrameQueue::new(&pool);
        queue.push(Some(0), None, &[1; 5000]).expect("push");
        queue.pop_front();
        queue.reserve(20, 20 * 7).expect("room");
        let used = pool.used();
        for n in 0..20u8 {
            queue.push_reserved(Some(n.into()), None, &[n; 7]);
        }
        assert_eq!(pool.used(), used);
        assert_eq!((queue.len(), queue.live), (20, 140));
        assert_eq!(queue.live, live(&queue));
    }

    #[test]
    fn frames_kept_back_leave_when_released() {
        let pool = Budget::new(1 << 24).expect("pool");
        let mut queue = FrameQueue::new(&pool);
        queue.reserve(4, 4).expect("room");
        queue.push_reserved(Some(0), Some(5), &[0]);
        for n in 1..4u8 {
            queue.push_kept(Some(n.into()), (n == 2).then_some(7), &[n]);
        }
        queue.push_kept(Some(9), None, &[]);
        assert_eq!(queue.kept(), 3);
        assert_eq!(queue.pop_front().map(|f| f.data[0]), Some(0));
        assert!(queue.front().is_none());
        queue.release(Some(3), Some(0));
        let mut left = Vec::new();
        while let Some(f) = queue.pop_front() {
            left.push((f.pts, f.duration, f.data[0]));
        }
        assert_eq!(left, [(Some(2), Some(7), 2), (Some(3), Some(3), 3)]);
        assert_eq!(queue.live, 0);
    }
}
