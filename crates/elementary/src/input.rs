//! The bytes ahead of a reader.
//!
//! [`Input`] presents one contiguous view starting at the reader's position: the
//! cursor's current window while nothing straddles a window boundary, and a charged
//! carry buffer once a frame does. Packets cut from the window share its storage; only
//! packets that cross windows are copied, once.
use std::sync::atomic::AtomicBool;
use triergon_core::{
    BudgetVec, Bytes, InputCursor, InputRequest, Result, SourceCapabilities, Step, WorkBudget,
};

/// Why the view is shorter than asked for.
#[derive(Debug)]
pub(crate) enum Wait {
    /// The step must return this (input request or yield).
    Step(Step<std::convert::Infallible>),
    /// The source ended: the view holds everything that is left.
    End,
}

impl Wait {
    pub(crate) fn from_step<T>(step: Step<T>) -> Option<Wait> {
        match step {
            Step::Ready(_) => None,
            Step::NeedInput(request) => Some(Wait::Step(Step::NeedInput(request))),
            Step::Yield => Some(Wait::Step(Step::Yield)),
            Step::End(_) => Some(Wait::End),
            Step::Backpressure(_) => Some(Wait::Step(Step::Yield)),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Input {
    cursor: InputCursor,
    carry: BudgetVec<u8>,
    /// Consumed prefix of `carry`.
    head: usize,
    /// Largest view a caller may ask for.
    maximum: usize,
    /// The source has ended: the view holds every byte left.
    ended: bool,
}

impl Input {
    pub(crate) fn new(cursor: InputCursor, maximum: usize) -> Input {
        let carry = BudgetVec::new(cursor.budget());
        Input {
            cursor,
            carry,
            head: 0,
            maximum,
            ended: false,
        }
    }

    pub(crate) fn cursor(&self) -> &InputCursor {
        &self.cursor
    }

    pub(crate) fn cursor_mut(&mut self) -> &mut InputCursor {
        &mut self.cursor
    }

    pub(crate) fn capabilities(&self) -> SourceCapabilities {
        self.cursor.capabilities()
    }

    fn carried(&self) -> &[u8] {
        &self.carry[self.head..]
    }

    /// The unconsumed bytes at [`Input::position`].
    #[inline]
    pub(crate) fn view(&self) -> &[u8] {
        if self.head < self.carry.len() {
            self.carried()
        } else {
            self.cursor.buffered()
        }
    }

    /// Whether the view holds every byte left in the source.
    pub(crate) fn ended(&self) -> bool {
        self.ended
    }

    /// Source offset of the first byte of the view.
    pub(crate) fn position(&self) -> u64 {
        self.cursor.position() - (self.carry.len() - self.head) as u64
    }

    /// Bytes left in the source after the view's start, when its length is known.
    pub(crate) fn remaining_in_source(&self) -> Option<u64> {
        let length = self.capabilities().length?;
        Some(length.saturating_sub(self.position()))
    }

    /// Make the view at least `len` bytes long.
    pub(crate) fn fill(
        &mut self,
        len: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Wait>> {
        if len > self.maximum {
            return Err(triergon_core::Error::Limit {
                requested: len,
                maximum: self.maximum,
            });
        }
        loop {
            let carried = self.carry.len() - self.head;
            if carried == 0 && self.cursor.buffered().len() >= len {
                return Ok(None);
            }
            if carried >= len {
                return Ok(None);
            }
            self.compact();
            self.reserve(len - carried)?;
            let step = self
                .cursor
                .read_append(&mut self.carry, len - carried, work, cancel)?;
            if let Some(wait) = Wait::from_step(step) {
                self.ended |= matches!(wait, Wait::End);
                return Ok(Some(wait));
            }
        }
    }

    /// Drop the consumed prefix of the carry once it is at least half of it.
    fn compact(&mut self) {
        if self.head == 0 {
            return;
        }
        let len = self.carry.len();
        if self.head == len {
            self.carry.clear();
            self.head = 0;
        } else if self.head >= len / 2 {
            self.carry.copy_within(self.head.., 0);
            self.carry.truncate(len - self.head);
            self.head = 0;
        }
    }

    /// Grow the carry geometrically so a long frame does not reallocate per window.
    fn reserve(&mut self, additional: usize) -> Result<()> {
        let target = self.carry.len().saturating_add(additional);
        if target <= self.carry.capacity() {
            return Ok(());
        }
        let wanted = target
            .max(self.carry.capacity().saturating_mul(2))
            .min(self.maximum.saturating_add(self.head).max(target));
        self.carry.try_reserve_exact(wanted - self.carry.len())
    }

    /// Consume the first `len` bytes of the view as shared storage. Call after
    /// [`Input::fill`] reported at least `len` bytes.
    pub(crate) fn take(
        &mut self,
        len: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Bytes>> {
        if len == 0 {
            return Bytes::copy_from_slice(self.cursor.budget(), &[]).map(Some);
        }
        loop {
            let carried = self.carry.len() - self.head;
            if carried == 0 && work.remaining() >= len as u64 {
                return match self.cursor.read_span(len, work, cancel)? {
                    Step::Ready(bytes) if bytes.len() == len => Ok(Some(bytes)),
                    _ => Err(triergon_core::Error::Invalid("input view shrank")),
                };
            }
            if carried >= len {
                if work.take(len as u64) == 0 {
                    return Ok(None);
                }
                let bytes = Bytes::copy_from_slice(self.cursor.budget(), &self.carried()[..len])?;
                self.head += len;
                return Ok(Some(bytes));
            }
            // The step's work cannot cover a zero-copy span: gather into the carry,
            // which resumes across steps.
            if self.fill(len, work, cancel)?.is_some() || work.remaining() == 0 {
                return Ok(None);
            }
            if self.carry.len() - self.head == 0 {
                self.compact();
                self.reserve(len)?;
                let step = self
                    .cursor
                    .read_append(&mut self.carry, len, work, cancel)?;
                if Wait::from_step(step).is_some() {
                    return Ok(None);
                }
            }
        }
    }

    /// Consume up to `len` bytes of the view without keeping them; returns how many.
    pub(crate) fn skip(
        &mut self,
        len: u64,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<std::result::Result<u64, Wait>> {
        let carried = (self.carry.len() - self.head) as u64;
        if carried > 0 {
            let count = work.take(len.min(carried));
            if count == 0 {
                return Ok(Err(Wait::Step(Step::Yield)));
            }
            self.head += count as usize;
            return Ok(Ok(count));
        }
        let buffered = self.cursor.buffered().len() as u64;
        if buffered == 0 && len > self.maximum as u64 && self.capabilities().random_access {
            // A long skip on a seekable source jumps instead of reading.
            let target = self.cursor.position() + len;
            if self
                .capabilities()
                .length
                .is_none_or(|length| target <= length)
            {
                self.cursor.request_range(target)?;
                return Ok(Ok(len));
            }
        }
        let step =
            self.cursor
                .read_discard(usize::try_from(len).unwrap_or(usize::MAX), work, cancel)?;
        Ok(match step {
            Step::Ready(count) => Ok(count as u64),
            other => Err(Wait::from_step(other).unwrap_or(Wait::Step(Step::Yield))),
        })
    }

    /// Restart at `offset` with a new input generation.
    pub(crate) fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        let request = self.cursor.reset(offset)?;
        self.carry.clear();
        self.head = 0;
        self.ended = false;
        Ok(request)
    }
}
