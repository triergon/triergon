//! Buffer arithmetic shared by the transport models: where FFmpeg's reader holds its
//! 32768-byte buffer in the source as read callbacks of a bounded size fill it, and where a
//! refill can start when the reader returns to the sample entry.
use crate::{
    reference_trace::mov::raw_video_transport::EntryRevisit, table::recovery::DescriptionContext,
};
use triergon_core::{Error, Result};

/// Capacity of the reference wrapper's original allocation.
pub(super) const CAPACITY: u64 = 32768;

#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Window {
    pub(super) cursor: u64,
    begin: u64,
    pub(super) end: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Revisit {
    pub(super) kind: EntryRevisit,
    pub(super) first_anchor: u64,
    pub(super) last_anchor: u64,
}

impl Revisit {
    pub(super) fn buffered(target: u64) -> Self {
        Self {
            kind: EntryRevisit::Buffered,
            first_anchor: target,
            last_anchor: target,
        }
    }

    pub(super) fn window_at(self, seed: Window, anchor: u64) -> Window {
        if self.kind == EntryRevisit::Buffered {
            seed
        } else {
            Window::at(anchor)
        }
    }
}

impl Window {
    pub(super) fn at(target: u64) -> Self {
        Self {
            cursor: target,
            begin: target,
            end: target,
        }
    }

    pub(super) fn contains(self, target: u64) -> bool {
        (self.begin..=self.end).contains(&target)
    }

    pub(super) fn matches(self, source_position: u64, ptr_offset: u64, end_offset: u64) -> bool {
        self.end == source_position
            && self.cursor.checked_sub(self.begin) == Some(ptr_offset)
            && self.end.checked_sub(self.begin) == Some(end_offset)
    }

    /// Each model admits only positive callback sizes when validating its context.
    pub(super) fn consume(
        &mut self,
        target: u64,
        context: DescriptionContext,
        bounds_error: &'static str,
    ) -> Result<()> {
        if target < self.cursor || target > context.physical_length {
            return Err(Error::Unsupported(bounds_error));
        }
        if target > self.end {
            let quantum = u64::try_from(context.custom_read_maximum)
                .map_err(|_| Error::Overflow)?
                .min(CAPACITY);
            let needed = target.checked_sub(self.end).ok_or(Error::Overflow)?;
            let fills = needed.checked_add(quantum - 1).ok_or(Error::Overflow)? / quantum;
            self.begin = self
                .end
                .checked_add((fills - 1).checked_mul(quantum).ok_or(Error::Overflow)?)
                .ok_or(Error::Overflow)?;
            self.end = self
                .end
                .checked_add(fills.checked_mul(quantum).ok_or(Error::Overflow)?)
                .ok_or(Error::Overflow)?
                .min(context.physical_length);
        }
        self.cursor = target;
        Ok(())
    }

    pub(super) fn revisit_set(
        &mut self,
        target: u64,
        forward_error: &'static str,
    ) -> Result<Revisit> {
        if self.contains(target) {
            self.cursor = target;
            return Ok(Revisit::buffered(target));
        }
        let filled = self.end.checked_sub(self.begin).ok_or(Error::Overflow)?;
        let gap = self
            .begin
            .checked_sub(target)
            .ok_or(Error::Unsupported(forward_error))?;
        // A revisit may originate anywhere in the preceding filled window.
        // Keep every source-consistent anchor when the target reaches into it.
        let (kind, first_anchor) = if gap < filled {
            (
                EntryRevisit::OneWindowCallbackSet,
                self.begin.saturating_sub(filled),
            )
        } else {
            (EntryRevisit::DirectCallback, target)
        };
        Ok(Revisit {
            kind,
            first_anchor,
            last_anchor: target,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{EntryRevisit, Window};
    use triergon_core::Error;

    #[test]
    fn revisit_anchors_at_the_edges_of_a_filled_window() {
        let seed = Window {
            begin: 16,
            cursor: 19,
            end: 24,
        };
        for (target, kind, first) in [
            (8, EntryRevisit::DirectCallback, 8),
            (9, EntryRevisit::OneWindowCallbackSet, 8),
            (15, EntryRevisit::OneWindowCallbackSet, 8),
            (16, EntryRevisit::Buffered, 16),
            (24, EntryRevisit::Buffered, 24),
        ] {
            let mut window = seed;
            let revisit = window
                .revisit_set(target, "forward revisit")
                .expect("valid revisit");
            assert_eq!(
                (revisit.kind, revisit.first_anchor, revisit.last_anchor),
                (kind, first, target)
            );
            for anchor in first..=target {
                let candidate = revisit.window_at(window, anchor);
                if kind == EntryRevisit::Buffered {
                    assert!(candidate.matches(24, target - 16, 8));
                } else {
                    assert!(candidate.matches(anchor, 0, 0));
                }
            }
        }
        let mut window = seed;
        assert!(matches!(
            window.revisit_set(25, "forward revisit"),
            Err(Error::Unsupported("forward revisit"))
        ));
        assert!(window.matches(24, 3, 8));
    }
}
