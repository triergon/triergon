//! Buffer arithmetic shared by the transport models: where FFmpeg's reader holds its
//! 32768-byte buffer in the source as read callbacks of a bounded size fill it, and where a
//! refill can start when the reader returns to the sample entry.
use crate::{
    boxes::{BoxHeader, BoxView},
    fields,
    reference_trace::mov::raw_video_transport::EntryRevisit,
    table::recovery::DescriptionContext,
};
use triergon_core::{Error, Result};

/// Capacity of the reference wrapper's original allocation.
pub(super) const CAPACITY: u64 = 32768;
/// Most bytes FFmpeg's reader discards by reading forward instead of seeking.
pub(super) const DISCARD_BOUND: u64 = 32768;
/// Most children walked at one level, top-level boxes included.
pub(super) const CHILD_LIMIT: u32 = 16;

const LEVEL_0: [[u8; 4]; 2] = [*b"mvhd", *b"trak"];
const LEVEL_1: [[u8; 4]; 2] = [*b"tkhd", *b"mdia"];
const LEVEL_2: [[u8; 4]; 3] = [*b"mdhd", *b"hdlr", *b"minf"];
const LEVEL_3: [[u8; 4]; 3] = [*b"vmhd", *b"dinf", *b"stbl"];
const LEVEL_4: [[u8; 4]; 6] = [*b"stsd", *b"stts", *b"stsc", *b"stsz", *b"stco", *b"stss"];
const LEVEL_5: [[u8; 4]; 1] = [*b"raw "];

pub(super) fn expected(level: u8) -> &'static [[u8; 4]] {
    match level {
        0 => &LEVEL_0,
        1 => &LEVEL_1,
        2 => &LEVEL_2,
        3 => &LEVEL_3,
        4 => &LEVEL_4,
        5 => &LEVEL_5,
        _ => &[],
    }
}
pub(super) fn next_kind(level: u8) -> [u8; 4] {
    match level {
        0 => *b"trak",
        1 => *b"mdia",
        2 => *b"minf",
        3 => *b"stbl",
        4 => *b"stsd",
        _ => *b"raw ",
    }
}
pub(super) fn small_plain(header: &BoxHeader, length: u64, refusal: &'static str) -> Result<()> {
    if header.header_size != 8
        || header.size.is_none()
        || header.end()?.is_none_or(|end| end > length)
    {
        return Err(Error::Unsupported(refusal));
    }
    Ok(())
}
pub(super) fn visual_entry(raw: &BoxView, refusal: &'static str) -> Result<()> {
    let b = raw.payload();
    if raw.header().kind != *b"raw "
        || b.len() != 78
        || b[..6] != [0; 6]
        || fields::u16_at(b, 6)? != 1
        || b[8..24] != [0; 16]
        || fields::u16_at(b, 24)? != 2
        || fields::u16_at(b, 26)? != 2
        || fields::u32_at(b, 28)? != 0x0048_0000
        || fields::u32_at(b, 32)? != 0x0048_0000
        || fields::u32_at(b, 36)? != 0
        || fields::u16_at(b, 40)? != 1
        || b[42..74] != [0; 32]
        || fields::u16_at(b, 74)? != 24
        || fields::u16_at(b, 76)? != u16::MAX
    {
        return Err(Error::Unsupported(refusal));
    }
    Ok(())
}

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
