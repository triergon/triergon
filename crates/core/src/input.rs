//! Bounded input ownership and explicit entity lifecycle. This module does not
//! perform I/O or interpret a container's logical packet/segment boundaries.
use crate::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};
use std::sync::atomic::{AtomicBool, Ordering};

const INLINE_INPUT_BYTES: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceCapabilities {
    pub length: Option<u64>,
    pub random_access: bool,
}

/// A response is valid only for this pool, parser generation and range request.
/// Codec configuration and transport clock epochs belong to the format layer.
#[derive(Clone, Debug)]
pub struct InputToken {
    pool: crate::budget::PoolId,
    generation: u64,
    request: u64,
}
impl InputToken {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn request(&self) -> u64 {
        self.request
    }
}
impl PartialEq for InputToken {
    fn eq(&self, other: &Self) -> bool {
        self.pool == other.pool
            && self.generation == other.generation
            && self.request == other.request
    }
}
impl Eq for InputToken {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLocation {
    pub source: SourceId,
    pub offset: u64,
}

#[derive(Clone, Debug)]
pub struct InputRequest {
    pub source: SourceId,
    pub token: InputToken,
    pub offset: u64,
    /// A caller may supply fewer bytes, including one byte. Zero available bytes
    /// never establish EOF; completion is a separately validated message.
    pub maximum: usize,
}

#[derive(Clone, Debug)]
pub struct InputWindow {
    pub source: SourceId,
    pub token: InputToken,
    pub offset: u64,
    bytes: Bytes,
}
impl InputWindow {
    pub fn new(source: SourceId, token: InputToken, offset: u64, bytes: Bytes) -> Result<Self> {
        offset
            .checked_add(u64::try_from(bytes.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok(Self {
            source,
            token,
            offset,
            bytes,
        })
    }
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFailure {
    Transport,
    Cancelled,
}

/// Retained output storage is exhausted: release output, then step again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backpressure {
    pub requested: usize,
    pub available: usize,
}

/// The outcome of one bounded unit of resumable work.
///
/// `B` is the backpressure payload. Operations that retain no output storage
/// (input cursors, most framers) use the default uninhabited `Infallible`, so
/// their callers match without a backpressure arm; readers that own output
/// storage return `Step<T, Backpressure>`.
#[derive(Debug)]
pub enum Step<T, B = std::convert::Infallible> {
    Ready(T),
    NeedInput(InputRequest),
    Yield,
    Backpressure(B),
    /// End of this source entity. A playlist/transport parser decides whether
    /// this is also its logical EOF; an entity boundary need not flush a PES.
    End(SourceLocation),
}

#[derive(Debug)]
pub struct InputCursor {
    pool: Budget,
    source: SourceId,
    capabilities: SourceCapabilities,
    token: InputToken,
    position: u64,
    maximum_window: usize,
    window: Option<Bytes>,
    inline: [u8; INLINE_INPUT_BYTES],
    inline_len: usize,
    consumed: usize,
    end: Option<u64>,
    failure: Option<Error>,
}

impl InputCursor {
    /// Maximum fixed cursor-resident response, without a heap owner.
    pub const INLINE_CAPACITY: usize = INLINE_INPUT_BYTES;

    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        maximum_window: usize,
    ) -> Result<Self> {
        if maximum_window == 0 {
            return Err(Error::Invalid("zero input window policy"));
        }
        let token = InputToken {
            pool: pool.identity(),
            generation: pool.next_generation()?,
            request: 0,
        };
        Ok(Self {
            pool: pool.clone(),
            source,
            capabilities,
            token,
            position: 0,
            maximum_window,
            window: None,
            inline: [0; INLINE_INPUT_BYTES],
            inline_len: 0,
            consumed: 0,
            end: None,
            failure: None,
        })
    }
    pub fn budget(&self) -> &Budget {
        &self.pool
    }
    /// Retains the actual input-window owner for allocation deduplication.
    /// This does not advance the cursor or allocate/copy bytes. The handle
    /// keeps the entire owner charged after consumption, reset or cancellation.
    /// A fixed inline response has no heap owner and returns `None` until a
    /// span read promotes it into charged shared storage.
    pub fn retained_window(&self) -> Option<Bytes> {
        self.buffered_owner().cloned()
    }
    /// The current shared input window, including its consumed prefix.
    pub fn buffered_owner(&self) -> Option<&Bytes> {
        self.window.as_ref()
    }
    pub fn source(&self) -> SourceId {
        self.source
    }
    pub fn capabilities(&self) -> SourceCapabilities {
        self.capabilities
    }
    pub fn position(&self) -> u64 {
        self.position
    }
    pub fn generation(&self) -> u64 {
        self.token.generation
    }
    pub fn token(&self) -> InputToken {
        self.token.clone()
    }
    pub fn request(&self) -> InputRequest {
        let maximum = self
            .capabilities
            .length
            .map_or(self.maximum_window, |length| {
                usize::try_from(length.saturating_sub(self.position))
                    .unwrap_or(usize::MAX)
                    .min(self.maximum_window)
            });
        InputRequest {
            source: self.source,
            token: self.token(),
            offset: self.position,
            maximum,
        }
    }
    /// Returns the next request if the cursor has consumed its buffered input
    /// and the source is still active. Checks for cancellation and sticky
    /// failure before requesting bytes.
    #[inline]
    pub fn pending_request(&mut self, cancel: &AtomicBool) -> Result<Option<InputRequest>> {
        if self.inline_len != 0 || self.window.is_some() || self.end.is_some() {
            return Ok(None);
        }
        self.check_cancel(cancel)?;
        Ok(Some(self.request()))
    }
    fn verify(&self, source: SourceId, token: &InputToken) -> Result<()> {
        if self.pool.identity() != token.pool {
            return Err(Error::ForeignPool);
        }
        if token.generation != self.token.generation {
            return Err(Error::StaleGeneration {
                expected: self.token.generation,
                received: token.generation,
            });
        }
        if token.request != self.token.request {
            return Err(Error::StaleRequest {
                expected: self.token.request,
                received: token.request,
            });
        }
        if source != self.source {
            return Err(Error::SourceMismatch);
        }
        Ok(())
    }
    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }
    fn remaining(&self) -> usize {
        self.window.as_ref().map_or(self.inline_len, Bytes::len) - self.consumed
    }
    /// The unconsumed bytes already supplied at the current position. Looking
    /// consumes no byte or work and creates no owner.
    #[inline]
    pub fn buffered(&self) -> &[u8] {
        let bytes = match &self.window {
            Some(window) => window.as_ref(),
            None => &self.inline[..self.inline_len],
        };
        &bytes[self.consumed..]
    }
    fn validate_window(&self, offset: u64, length: usize, maximum: usize) -> Result<()> {
        // A complete read clears either representation, including its consumed count.
        if offset != self.position
            || self.inline_len != 0
            || self.window.is_some()
            || self.end.is_some()
        {
            return Err(Error::Invalid("input window is not currently requested"));
        }
        if length > maximum {
            return Err(Error::Limit {
                requested: length,
                maximum,
            });
        }
        let end = offset
            .checked_add(u64::try_from(length).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if self.capabilities.length.is_some_and(|length| end > length) {
            return Err(Error::Invalid("input window exceeds source length"));
        }
        Ok(())
    }
    fn validate_provision(&self, window: &InputWindow) -> Result<()> {
        self.verify(window.source, &window.token)?;
        self.healthy()?;
        if !window.bytes.belongs_to(&self.pool) {
            return Err(Error::ForeignPool);
        }
        self.validate_window(window.offset, window.bytes.len(), self.maximum_window)
    }
    /// Accepts a prefix at the current requested offset. Invalid/stale responses
    /// leave the cursor unchanged, and callers retain their original window.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.validate_provision(window)?;
        self.window = (!window.bytes.is_empty()).then(|| window.bytes.clone());
        self.inline_len = 0;
        self.consumed = 0;
        Ok(())
    }
    /// Consumes an owned response after the same validation as `provide`.
    /// Rejection leaves the cursor unchanged and drops the submitted response.
    /// No allocation or reference-count round trip is needed to accept it.
    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        self.validate_provision(&window)?;
        self.window = (!window.bytes.is_empty()).then_some(window.bytes);
        self.inline_len = 0;
        self.consumed = 0;
        Ok(())
    }
    /// Copies at most twelve bytes into fixed cursor storage. Source, token,
    /// offset, entity and window limits match owned provision; an empty response
    /// remains starvation. Processing still consumes the caller's byte work.
    ///
    /// Inline bytes have no heap owner. `read_span` first promotes the complete
    /// response into charged immutable storage, preserving hidden prefixes and
    /// suffixes; allocation pressure leaves this response available for retry.
    #[inline]
    pub fn provide_inline(
        &mut self,
        source: SourceId,
        token: InputToken,
        offset: u64,
        bytes: &[u8],
    ) -> Result<()> {
        self.verify(source, &token)?;
        self.healthy()?;
        self.validate_window(
            offset,
            bytes.len(),
            self.maximum_window.min(Self::INLINE_CAPACITY),
        )?;
        if let [byte] = bytes {
            self.inline[0] = *byte;
        } else {
            self.inline[..bytes.len()].copy_from_slice(bytes);
        }
        self.inline_len = bytes.len();
        Ok(())
    }
    fn discard_window(&mut self) {
        self.window = None;
        self.inline_len = 0;
        self.consumed = 0;
    }
    /// Marks a known entity boundary after all supplied bytes, not just bytes
    /// already consumed. A short known-length source is a sticky truncation.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.verify(source, &token)?;
        self.healthy()?;
        let end = self
            .position
            .checked_add(u64::try_from(self.remaining()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if self.capabilities.length.is_some_and(|length| length != end) {
            self.failure = Some(Error::Truncated);
            self.discard_window();
            return Err(Error::Truncated);
        }
        self.capabilities.length = Some(end);
        self.end = Some(end);
        Ok(())
    }
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.verify(source, &token)?;
        self.healthy()?;
        self.failure = Some(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        self.discard_window();
        Ok(())
    }
    fn check_cancel(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.healthy()?;
        if cancel.load(Ordering::Relaxed) {
            self.failure = Some(Error::Cancelled);
            self.discard_window();
            return Err(Error::Cancelled);
        }
        Ok(())
    }
    fn wait<T>(&self) -> Step<T> {
        if self.end == Some(self.position) {
            Step::End(SourceLocation {
                source: self.source,
                offset: self.position,
            })
        } else {
            Step::NeedInput(self.request())
        }
    }
    fn advance(&mut self, count: usize, complete_window: bool) {
        // Every accepted window's end was checked before it entered the cursor.
        self.position += count as u64;
        self.consumed += count;
        if complete_window {
            self.discard_window();
        }
    }
    #[inline(always)]
    pub fn read_into(
        &mut self,
        output: &mut [u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<usize>> {
        self.check_cancel(cancel)?;
        if output.is_empty() {
            return Ok(Step::Ready(0));
        }
        if self.inline_len == 1 {
            if !work.try_charge(1) {
                return Ok(Step::Yield);
            }
            output[0] = self.inline[0];
            // The offered window already checked the end of this byte.
            self.position += 1;
            self.inline_len = 0;
            return Ok(Step::Ready(1));
        }
        let buffered = self.buffered();
        if buffered.is_empty() {
            return Ok(self.wait());
        }
        let maximum =
            u64::try_from(output.len().min(buffered.len())).map_err(|_| Error::Overflow)?;
        let count = work.take(maximum) as usize; // bounded by the original slice length
        if count == 0 {
            return Ok(Step::Yield);
        }
        output[..count].copy_from_slice(&buffered[..count]);
        let complete = count == buffered.len();
        self.advance(count, complete);
        Ok(Step::Ready(count))
    }
    /// Consume buffered bytes in place.
    /// Each byte costs one work unit. Zero maximum, starvation, explicit end,
    /// sticky failure and cancellation follow `read_into` ordering. Completing
    /// a window releases the cursor's reference. Independent views remain valid.
    #[inline]
    pub fn read_discard(
        &mut self,
        maximum: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<usize>> {
        self.check_cancel(cancel)?;
        if maximum == 0 {
            return Ok(Step::Ready(0));
        }
        let remaining = self.remaining();
        if remaining == 0 {
            return Ok(self.wait());
        }
        let maximum = u64::try_from(maximum.min(remaining)).map_err(|_| Error::Overflow)?;
        let count = work.take(maximum) as usize; // bounded by the buffered window
        if count == 0 {
            return Ok(Step::Yield);
        }
        self.advance(count, count == remaining);
        Ok(Step::Ready(count))
    }
    /// Copy buffered input straight into an initialized, capacity-accounted byte
    /// builder. The destination may use its own pool, like a read_into buffer.
    /// Reservation failure preserves input, work and destination contents. No
    /// temporary Bytes owner, scratch buffer or uninitialized tail is created.
    #[inline(always)]
    pub fn read_append(
        &mut self,
        output: &mut BudgetVec<u8>,
        maximum: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<usize>> {
        self.check_cancel(cancel)?;
        if maximum == 0 {
            return Ok(Step::Ready(0));
        }
        if self.inline_len == 1 {
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            output.try_extend_from_slice(&self.inline[..1])?;
            work.take(1);
            self.position += 1;
            self.inline_len = 0;
            return Ok(Step::Ready(1));
        }
        let buffered = self.buffered();
        if buffered.is_empty() {
            return Ok(self.wait());
        }
        let maximum = u64::try_from(maximum.min(buffered.len())).map_err(|_| Error::Overflow)?;
        let count = work.remaining().min(maximum) as usize; // bounded by the buffered slice
        if count == 0 {
            return Ok(Step::Yield);
        }
        // BudgetVec reserves before extending; neither source nor work has moved.
        output.try_extend_from_slice(&buffered[..count])?;
        let complete = count == buffered.len();
        work.take(count as u64);
        self.advance(count, complete);
        Ok(Step::Ready(count))
    }
    pub fn read_span(
        &mut self,
        maximum: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Bytes>> {
        self.check_cancel(cancel)?;
        if maximum == 0 {
            return Err(Error::Invalid("zero span read"));
        }
        if self.remaining() == 0 {
            return Ok(self.wait());
        }
        let maximum = u64::try_from(maximum.min(self.remaining())).map_err(|_| Error::Overflow)?;
        if work.remaining() == 0 {
            return Ok(Step::Yield);
        }
        if self.window.is_none() {
            // Reserve/copy the entire supplied window before consuming work or
            // changing its representation. On failure all inline bytes and the
            // existing consumed prefix remain available unchanged.
            let window = Bytes::copy_from_slice(&self.pool, &self.inline[..self.inline_len])?;
            self.window = Some(window);
            self.inline_len = 0;
        }
        let count = work.take(maximum) as usize; // bounded by the original window length
        let complete = count == self.remaining();
        let range = self.consumed..self.consumed + count;
        let bytes = if complete {
            self.window
                .take()
                .ok_or(Error::Invalid("missing input window"))?
                .into_slice(range)?
        } else {
            self.window
                .as_ref()
                .ok_or(Error::Invalid("missing input window"))?
                .slice(range)?
        };
        self.advance(count, complete);
        Ok(Step::Ready(bytes))
    }
    fn validate_offset(&self, offset: u64) -> Result<()> {
        if !self.capabilities.random_access && offset != 0 {
            return Err(Error::Unsupported("random-access input"));
        }
        if self
            .capabilities
            .length
            .is_some_and(|length| offset > length)
        {
            return Err(Error::Invalid("range exceeds source length"));
        }
        Ok(())
    }
    fn discard_at(&mut self, offset: u64) {
        self.position = offset;
        self.discard_window();
        self.end = None;
        self.failure = None;
    }
    /// Internal metadata/range jump: invalidate I/O requests, preserving the
    /// parser generation. Does not recover an existing source failure.
    pub fn request_range(&mut self, offset: u64) -> Result<InputRequest> {
        self.healthy()?;
        if !self.capabilities.random_access {
            return Err(Error::Unsupported("random-access input"));
        }
        self.validate_offset(offset)?;
        let request = self.token.request.checked_add(1).ok_or(Error::Overflow)?;
        self.discard_at(offset);
        self.token.request = request;
        Ok(self.request())
    }
    /// Explicit logical seek/reset and recovery. The owning parser must reset
    /// its pending syntax/configuration state in the same transition.
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        self.validate_offset(offset)?;
        let generation = self.pool.next_generation()?;
        self.discard_at(offset);
        self.token.generation = generation;
        self.token.request = 0;
        Ok(self.request())
    }
    /// Continue a logical stream after consuming one completed source entity.
    /// Format-owned PES, segment and clock epochs are deliberately unaffected.
    pub fn continue_source(
        &mut self,
        source: SourceId,
        capabilities: SourceCapabilities,
    ) -> Result<InputRequest> {
        self.healthy()?;
        if self.end != Some(self.position) || self.remaining() != 0 {
            return Err(Error::Invalid("source entity is not fully consumed"));
        }
        let request = self.token.request.checked_add(1).ok_or(Error::Overflow)?;
        self.discard_at(0);
        self.source = source;
        self.capabilities = capabilities;
        self.token.request = request;
        Ok(self.request())
    }
}
