//! std Read/Seek supply adapter for the reader's existing core cursor. It owns
//! no parser, pool, timestamps or input cursor. A read awaiting freeze remains
//! pending, so output pressure cannot repeat I/O or drop an unread partial chunk.
//! The public blocking reader drives the retained typed jobs through this supply
//! adapter; it does not select an alternate parsing path.
use crate::step::{Required, check};
use crate::{Error, Result};
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::atomic::AtomicBool,
};
use triergon_core::{
    BudgetVec, Bytes, InputCursor, InputRequest, InputWindow, SourceFailure, Step, WorkBudget,
};

#[derive(Debug)]
pub(crate) enum FeedStep {
    Provided,
    End,
    Backpressure { requested: usize },
}
#[derive(Debug)]
enum Phase {
    Read,
    Freeze,
}
#[derive(Debug)]
struct Pending {
    request: InputRequest,
    inline_fallback: Option<usize>,
    buffer: Option<BudgetVec<u8>>,
    phase: Phase,
}
// Sealed to the explicit Cursor<Bytes> constructor below. Generic Read/Seek
// sources cannot opt into immutable views or acquire an implicit input cache.
#[derive(Debug)]
struct OwnedWindows<R> {
    read: fn(&mut R, usize) -> Result<Option<Bytes>>,
    root: fn(&R) -> &Bytes,
}
#[derive(Debug)]
pub(crate) struct InputFeeder<R> {
    reader: R,
    position: u64,
    maximum: usize,
    pending: Option<Pending>,
    owned: Option<OwnedWindows<R>>,
}
impl<R: Read> InputFeeder<R> {
    /// The ordinary blocking API begins a source at offset zero. A caller using
    /// an already-positioned seekable reader can use with_position instead.
    pub(crate) fn new(reader: R, maximum: usize) -> Result<Self> {
        Self::with_position(reader, 0, maximum)
    }
    pub(crate) fn with_position(reader: R, position: u64, maximum: usize) -> Result<Self> {
        if maximum == 0 {
            return Err(Error::Invalid("empty blocking input window"));
        }
        Ok(Self {
            reader,
            position,
            maximum,
            pending: None,
            owned: None,
        })
    }
    pub(crate) fn owned_source(&self) -> Option<&Bytes> {
        self.owned.as_ref().map(|owned| (owned.root)(&self.reader))
    }
    // Construct only a borrowed graph reference on the hot poll path. The
    // source-root callback runs solely during a real cold pressure walk.
    pub(crate) fn owned_source_storage(&self) -> Option<&dyn crate::storage::usage::Storage> {
        self.owned
            .as_ref()
            .map(|_| self as &dyn crate::storage::usage::Storage)
    }
    pub(crate) fn into_inner(self) -> R {
        let Self {
            reader, pending, ..
        } = self;
        drop(pending);
        reader
    }
    pub(crate) fn has_pending_read(&self) -> bool {
        self.pending.is_some()
    }
    pub(crate) fn discard_stale(&mut self, request: &InputRequest) {
        if self.pending.as_ref().is_some_and(|pending| {
            pending.request.source != request.source
                || pending.request.token != request.token
                || pending.request.offset != request.offset
        }) {
            self.pending = None;
        }
    }
    pub(crate) fn retained_buffer_bytes(&self) -> usize {
        self.pending
            .as_ref()
            .and_then(|pending| pending.buffer.as_ref())
            .map_or(0, BudgetVec::capacity)
    }
    #[cfg(test)]
    pub(crate) fn reader_mut(&mut self) -> &mut R {
        &mut self.reader
    }
    #[cfg(test)]
    pub(crate) fn feed_sequential(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> Result<FeedStep> {
        self.feed(input, cancel, None, None, false)
    }
    #[cfg(test)]
    pub(crate) fn feed_sequential_request(
        &mut self,
        input: &mut InputCursor,
        request: &InputRequest,
        cancel: &AtomicBool,
    ) -> Result<FeedStep> {
        self.feed(input, cancel, None, Some(request), false)
    }
    #[inline]
    pub(crate) fn feed_typed_request(
        &mut self,
        input: &mut InputCursor,
        request: &InputRequest,
        header: bool,
        cancel: &AtomicBool,
        reposition: Option<fn(&mut R, u64) -> io::Result<u64>>,
    ) -> Result<FeedStep> {
        self.feed(input, cancel, reposition, Some(request), header)
    }
    /// Bytes the parser discards cannot become a payload owner. An immutable
    /// source supplies them as a view of its charged root, with the exact same
    /// request; any other source takes the ordinary copied body path.
    pub(crate) fn feed_typed_discard_request(
        &mut self,
        input: &mut InputCursor,
        request: &InputRequest,
        cancel: &AtomicBool,
        reposition: Option<fn(&mut R, u64) -> io::Result<u64>>,
    ) -> Result<FeedStep> {
        match self.advance_with(input, cancel, reposition, Some(request), false, None, true) {
            Err(Error::Backpressure(triergon_core::Backpressure { requested, .. })) => {
                Ok(FeedStep::Backpressure { requested })
            }
            other => other,
        }
    }
    /// A bounded widened discard (see `bounded_discard_refill`); immutable only.
    pub(crate) fn feed_typed_discard_refill(
        &mut self,
        input: &mut InputCursor,
        plan: &crate::element::control::HeaderRefillRequest,
        cancel: &AtomicBool,
        reposition: Option<fn(&mut R, u64) -> io::Result<u64>>,
    ) -> Result<FeedStep> {
        if self.owned.is_none() {
            return Err(Error::Invalid(
                "widened discard requires an immutable source",
            ));
        }
        self.feed_typed_discard_request(input, plan.expanded(), cancel, reposition)
    }
    pub(crate) fn feed_typed_header_refill(
        &mut self,
        input: &mut InputCursor,
        plan: &crate::element::control::HeaderRefillRequest,
        cancel: &AtomicBool,
        reposition: Option<fn(&mut R, u64) -> io::Result<u64>>,
    ) -> Result<FeedStep> {
        match self.advance(
            input,
            cancel,
            reposition,
            Some(plan.expanded()),
            true,
            Some(plan.exact().maximum),
        ) {
            Err(Error::Backpressure(triergon_core::Backpressure { requested, .. })) => {
                Ok(FeedStep::Backpressure { requested })
            }
            other => other,
        }
    }
    fn feed(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
        reposition: Option<fn(&mut R, u64) -> io::Result<u64>>,
        requested: Option<&InputRequest>,
        inline_header: bool,
    ) -> Result<FeedStep> {
        match self.advance(input, cancel, reposition, requested, inline_header, None) {
            Err(Error::Backpressure(triergon_core::Backpressure { requested, .. })) => {
                Ok(FeedStep::Backpressure { requested })
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
        reposition: Option<fn(&mut R, u64) -> io::Result<u64>>,
        requested: Option<&InputRequest>,
        inline_header: bool,
        inline_fallback: Option<usize>,
    ) -> Result<FeedStep> {
        self.advance_with(
            input,
            cancel,
            reposition,
            requested,
            inline_header,
            inline_fallback,
            false,
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "the one feed loop behind every header, body and discard read takes its mode as arguments"
    )]
    fn advance_with(
        &mut self,
        input: &mut InputCursor,
        cancel: &AtomicBool,
        reposition: Option<fn(&mut R, u64) -> io::Result<u64>>,
        requested: Option<&InputRequest>,
        inline_header: bool,
        mut inline_fallback: Option<usize>,
        discard: bool,
    ) -> Result<FeedStep> {
        // Probe starvation without consuming a byte or fabricating EOF from a
        // declared length. An unread core window yields with zero work.
        let mut request = match input.read_into(&mut [0], &mut WorkBudget::new(0), cancel)? {
            Step::NeedInput(request) => request,
            Step::End(_) => return Ok(FeedStep::End),
            _ => return Err(Error::Invalid("blocking feed with unread input")),
        };
        if let Some(hint) = requested {
            if hint.source != request.source
                || hint.token != request.token
                || hint.offset != request.offset
                || hint.maximum > request.maximum
            {
                return Err(Error::Invalid("blocking input request identity changed"));
            }
            request.maximum = hint.maximum;
        }
        // Only a core request/generation transition invalidates a pending
        // chunk. Its actual reader position remains known for the next seek.
        self.discard_stale(&request);
        if self.pending.is_none() {
            if self.position != request.offset {
                if !input.capabilities().random_access {
                    return Err(Error::Unsupported("range on sequential blocking input"));
                }
                let seek = reposition.ok_or(Error::Unsupported("blocking source requires Seek"))?;
                match seek(&mut self.reader, request.offset) {
                    Ok(position) if position == request.offset => self.position = position,
                    Ok(_) => {
                        input.fail(request.source, request.token, SourceFailure::Transport)?;
                        return Err(Error::Invalid("Seek returned a different position"));
                    }
                    Err(error) => {
                        input.fail(request.source, request.token, SourceFailure::Transport)?;
                        return Err(error.into());
                    }
                }
            }
            if request.maximum == 0 {
                // A real one-byte probe is necessary at a known-length boundary:
                // Read(&mut []) would return zero without observing the source.
                let mut probe = [0];
                let count = match read_retry(&mut self.reader, &mut probe) {
                    Ok(count) => count,
                    Err(error) => {
                        input.fail(request.source, request.token, SourceFailure::Transport)?;
                        return Err(error.into());
                    }
                };
                if count != 0 {
                    self.position = self
                        .position
                        .checked_add(count as u64)
                        .ok_or(Error::Overflow)?;
                    input.fail(request.source, request.token, SourceFailure::Transport)?;
                    return Err(Error::Invalid("source exceeds declared length"));
                }
                input.finish(request.source, request.token)?;
                return Ok(FeedStep::End);
            }
            if let Some(exact) = inline_fallback {
                if exact == 0 || exact > InputCursor::INLINE_CAPACITY || exact > request.maximum {
                    return Err(Error::Invalid("invalid exact header fallback"));
                }
                let maximum = request.maximum.min(self.maximum);
                let required = maximum
                    .checked_add(Bytes::owner_overhead_bytes())
                    .ok_or(Error::Overflow)?;
                if input.budget().available() < required {
                    request.maximum = exact;
                    inline_fallback = None;
                }
            }
            if inline_header && request.maximum.min(self.maximum) <= InputCursor::INLINE_CAPACITY {
                return self.read_inline_header(input, request, cancel);
            }
            // Only expanded header supply uses immutable views. Exact body
            // refills keep their historical capacity-N/direct-body/work path.
            // Inline headers and the pre-read exact fallback above are literal.
            if (inline_header || discard) && self.owned.is_some() {
                return self.read_owned_header(input, request, cancel);
            }
            self.pending = Some(Pending {
                request,
                inline_fallback,
                buffer: Some(BudgetVec::new(input.budget())),
                phase: Phase::Read,
            });
        }
        let pending = self.pending.as_mut().required("missing blocking read")?;
        if matches!(pending.phase, Phase::Read) {
            let maximum = pending.request.maximum.min(self.maximum);
            let buffer = pending
                .buffer
                .as_mut()
                .required("missing blocking read buffer")?;
            let reserve = buffer.try_resize(maximum, 0);
            let fallback = pending.inline_fallback.is_some()
                && (matches!(
                    &reserve,
                    Err(Error::Backpressure { .. } | Error::Allocation)
                ) || reserve.is_ok()
                    && input.budget().available() < Bytes::owner_overhead_bytes());
            if fallback {
                // Only before Read: release the attempted arena and use the
                // exact inline prefix of the canonical hint. Freeze retry never rereads.
                let mut request = pending.request.clone();
                request.maximum = pending
                    .inline_fallback
                    .take()
                    .required("missing header fallback")?;
                self.pending = None;
                return self.read_inline_header(input, request, cancel);
            }
            reserve?;
            let count = match read_retry(&mut self.reader, buffer.as_mut_slice()) {
                Ok(count) => count,
                Err(error) => {
                    input.fail(
                        pending.request.source,
                        pending.request.token.clone(),
                        SourceFailure::Transport,
                    )?;
                    return Err(error.into());
                }
            };
            if count > maximum {
                return Err(Error::Invalid("Read returned more than requested"));
            }
            if count == 0 {
                input.finish(pending.request.source, pending.request.token.clone())?;
                self.pending = None;
                return Ok(FeedStep::End);
            }
            self.position = self
                .position
                .checked_add(count as u64)
                .ok_or(Error::Overflow)?;
            buffer.try_resize(count, 0)?;
            pending.phase = Phase::Freeze;
        }
        input.read_into(&mut [], &mut WorkBudget::new(0), cancel)?;
        let buffer = pending
            .buffer
            .take()
            .required("missing blocking read buffer")?;
        let bytes = match buffer.freeze() {
            Ok(bytes) => bytes,
            Err(error) => {
                pending.buffer = Some(error.value);
                return Err(error.error);
            }
        };
        let window = InputWindow::new(
            pending.request.source,
            pending.request.token.clone(),
            pending.request.offset,
            bytes,
        )?;
        input.provide_owned(window)?;
        self.pending = None;
        Ok(FeedStep::Provided)
    }
    fn read_owned_header(
        &mut self,
        input: &mut InputCursor,
        request: InputRequest,
        cancel: &AtomicBool,
    ) -> Result<FeedStep> {
        let owned = self.owned.as_ref().required("missing immutable source")?;
        if !(owned.root)(&self.reader).belongs_to(input.budget()) {
            return Err(Error::ForeignPool);
        }
        let maximum = request.maximum.min(self.maximum);
        let Some(bytes) = (owned.read)(&mut self.reader, maximum)? else {
            input.finish(request.source, request.token)?;
            return Ok(FeedStep::End);
        };
        self.position = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or(Error::Overflow)?;
        // Match the ordinary post-read health/cancellation boundary before
        // publication. Source position moved, parser position/work did not.
        input.read_into(&mut [], &mut WorkBudget::new(0), cancel)?;
        input.provide_owned(InputWindow::new(
            request.source,
            request.token,
            request.offset,
            bytes,
        )?)?;
        Ok(FeedStep::Provided)
    }
    fn read_inline_header(
        &mut self,
        input: &mut InputCursor,
        request: InputRequest,
        cancel: &AtomicBool,
    ) -> Result<FeedStep> {
        // Fixed canonical fallback or a format-authorized small header refill.
        // Body windows retain the charged read/freeze path.
        let maximum = request.maximum.min(self.maximum);
        let mut bytes = [0; InputCursor::INLINE_CAPACITY];
        let count = match read_retry(&mut self.reader, &mut bytes[..maximum]) {
            Ok(count) => count,
            Err(error) => {
                input.fail(request.source, request.token, SourceFailure::Transport)?;
                return Err(error.into());
            }
        };
        if count > maximum {
            return Err(Error::Invalid("Read returned more than requested"));
        }
        if count == 0 {
            input.finish(request.source, request.token)?;
            return Ok(FeedStep::End);
        }
        let Some(position) = self.position.checked_add(count as u64) else {
            return Err(Error::Overflow);
        };
        self.position = position;
        input.read_into(&mut [], &mut WorkBudget::new(0), cancel)?;
        input.provide_inline(
            request.source,
            request.token,
            request.offset,
            &bytes[..count],
        )?;
        Ok(FeedStep::Provided)
    }
}
impl<R: Read> crate::storage::usage::Storage for InputFeeder<R> {
    fn storage_slots(&self) -> Result<usize> {
        Ok(usize::from(self.owned.is_some()))
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::storage::usage::Owner>> {
        Ok(if index == 0 {
            self.owned_source()
                .map(|source| crate::storage::usage::Owner::Bytes(source.clone()))
        } else {
            None
        })
    }
}
impl<R: Read + Seek> InputFeeder<R> {
    pub(crate) fn synchronize_seekable_position(
        &mut self,
        input: &mut InputCursor,
        request: &InputRequest,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let current = input.request();
        if request.source != current.source
            || request.token != current.token
            || request.offset != current.offset
        {
            return Err(Error::Invalid("blocking replay request identity changed"));
        }
        self.discard_stale(request);
        match input.read_into(&mut [0], &mut WorkBudget::new(0), cancel)? {
            Step::Yield => return Ok(()),
            Step::NeedInput(_) | Step::End(_) => (),
            _ => return Err(Error::Invalid("unexpected replay source state")),
        }
        if self.position != request.offset {
            match self.reader.seek(SeekFrom::Start(request.offset)) {
                Ok(position) if position == request.offset => {
                    self.position = position;
                    check(cancel)?;
                }
                Ok(_) => {
                    input.fail(
                        request.source,
                        request.token.clone(),
                        SourceFailure::Transport,
                    )?;
                    return Err(Error::Invalid("Seek returned a different position"));
                }
                Err(error) => {
                    input.fail(
                        request.source,
                        request.token.clone(),
                        SourceFailure::Transport,
                    )?;
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }
}
fn read_retry<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

impl InputFeeder<io::Cursor<triergon_core::OwnedInputBytes>> {
    pub(crate) fn from_owned(
        source: io::Cursor<triergon_core::OwnedInputBytes>,
        maximum: usize,
    ) -> Result<Self> {
        let mut result = Self::new(source, maximum)?;
        result.owned = Some(OwnedWindows {
            read: |source, maximum| {
                let start = usize::try_from(source.position()).unwrap_or(usize::MAX);
                let root = source.get_ref().as_bytes();
                if start >= root.len() {
                    return Ok(None);
                }
                let count = maximum.min(root.len() - start);
                let end = start.checked_add(count).ok_or(Error::Overflow)?;
                let bytes = root.slice(start..end)?;
                source.set_position(u64::try_from(end).map_err(|_| Error::Overflow)?);
                Ok(Some(bytes))
            },
            root: |source| source.get_ref().as_bytes(),
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
