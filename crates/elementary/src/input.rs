//! The bytes ahead of a reader.
//!
//! [`Input`] presents one contiguous view starting at the reader's position: the
//! cursor's current window while nothing straddles a window boundary, and a charged
//! carry buffer once a frame does. Packets cut from the window share its storage; only
//! packets that cross windows are copied, once. Once a carried packet is taken, the few
//! bytes left in the carry are the last bytes the cursor consumed, and the view returns
//! to the window, starting that far back in it.
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
    /// Bytes the cursor has consumed from its current window that the view still starts
    /// with; zero while the carry holds the view.
    back: usize,
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
            back: 0,
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
            self.window_view()
        }
    }

    /// The current window from `back` bytes before the cursor's position.
    #[inline]
    fn window_view(&self) -> &[u8] {
        let buffered = self.cursor.buffered();
        match self.cursor.buffered_owner() {
            Some(owner) if self.back > 0 => &owner[owner.len() - buffered.len() - self.back..],
            _ => buffered,
        }
    }

    /// After the carry gave up a packet: hand the view back to the window when the
    /// carry's remaining bytes are the window's last consumed bytes.
    fn return_to_window(&mut self) {
        let left = self.carry.len() - self.head;
        if left == 0 {
            return;
        }
        if let Some(owner) = self.cursor.buffered_owner()
            && owner.len() - self.cursor.buffered().len() >= left
        {
            self.carry.clear();
            self.head = 0;
            self.back = left;
        }
    }

    /// Whether the view holds every byte left in the source.
    pub(crate) fn ended(&self) -> bool {
        self.ended
    }

    /// Source offset of the first byte of the view.
    pub(crate) fn position(&self) -> u64 {
        self.cursor.position() - (self.carry.len() - self.head + self.back) as u64
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
            if carried == 0 && self.back + self.cursor.buffered().len() >= len {
                return Ok(None);
            }
            if carried >= len {
                return Ok(None);
            }
            self.compact();
            if self.back > 0 {
                // The view outgrows the window: carry what it already holds of it.
                self.spill_back(len)?;
                continue;
            }
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
            if carried == 0 && self.back == 0 && work.remaining() >= len as u64 {
                return match self.cursor.read_span(len, work, cancel)? {
                    Step::Ready(bytes) if bytes.len() == len => Ok(Some(bytes)),
                    _ => Err(triergon_core::Error::Invalid("input view shrank")),
                };
            }
            if carried == 0
                && self.back > 0
                && work.remaining() >= len as u64
                && self.window_view().len() >= len
            {
                return self.take_from_window(len, work, cancel).map(Some);
            }
            if carried >= len {
                if work.take(len as u64) == 0 {
                    return Ok(None);
                }
                let bytes = Bytes::copy_from_slice(self.cursor.budget(), &self.carried()[..len])?;
                self.head += len;
                self.return_to_window();
                return Ok(Some(bytes));
            }
            // The step's work cannot cover a zero-copy span: gather into the carry,
            // which resumes across steps.
            if self.fill(len, work, cancel)?.is_some() || work.remaining() == 0 {
                return Ok(None);
            }
            if self.back > 0 {
                self.spill_back(len)?;
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

    /// Move the window bytes before the cursor's position that the view starts with
    /// into the empty carry, reserving room for a view of `len` bytes.
    fn spill_back(&mut self, len: usize) -> Result<()> {
        self.compact();
        self.reserve(len.max(self.back))?;
        let owner = self
            .cursor
            .buffered_owner()
            .ok_or(triergon_core::Error::Invalid("input window lost"))?;
        let consumed = owner.len() - self.cursor.buffered().len();
        self.carry
            .try_extend_from_slice(&owner[consumed - self.back..consumed])?;
        self.back = 0;
        Ok(())
    }

    /// Slice `len` bytes of the window from `back` bytes before the cursor's position.
    fn take_from_window(
        &mut self,
        len: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Bytes> {
        let owner = self
            .cursor
            .buffered_owner()
            .ok_or(triergon_core::Error::Invalid("input window lost"))?;
        let start = owner.len() - self.cursor.buffered().len() - self.back;
        let bytes = owner.slice(start..start + len)?;
        if len > self.back {
            let ahead = len - self.back;
            match self.cursor.read_discard(ahead, work, cancel)? {
                Step::Ready(count) if count == ahead => {}
                _ => return Err(triergon_core::Error::Invalid("input view shrank")),
            }
            self.back = 0;
        } else {
            work.take(len as u64);
            self.back -= len;
        }
        Ok(bytes)
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
            self.return_to_window();
            return Ok(Ok(count));
        }
        if self.back > 0 {
            let count = work.take(len.min(self.back as u64));
            if count == 0 {
                return Ok(Err(Wait::Step(Step::Yield)));
            }
            self.back -= count as usize;
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
        self.back = 0;
        self.ended = false;
        Ok(request)
    }
}
