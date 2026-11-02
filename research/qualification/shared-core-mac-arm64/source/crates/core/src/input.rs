//! Bounded input ownership and explicit entity lifecycle. This module does not
//! perform I/O or interpret a container's logical packet/segment boundaries.
use crate::{Budget, Bytes, Error, Result, WorkBudget};
use std::sync::atomic::{AtomicBool, Ordering};

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
    pool: Budget,
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
        self.pool.same_pool(&other.pool)
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

#[derive(Debug)]
pub enum Step<T> {
    Ready(T),
    NeedInput(InputRequest),
    Yield,
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
    consumed: usize,
    end: Option<u64>,
    failure: Option<Error>,
}

impl InputCursor {
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
            pool: pool.clone(),
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
            consumed: 0,
            end: None,
            failure: None,
        })
    }
    pub fn budget(&self) -> &Budget {
        &self.pool
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
    fn verify(&self, source: SourceId, token: &InputToken) -> Result<()> {
        if !self.pool.same_pool(&token.pool) {
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
        self.window
            .as_ref()
            .map_or(0, |window| window.len() - self.consumed)
    }
    /// Accepts a prefix at the current requested offset. Invalid/stale responses
    /// leave the cursor unchanged, and callers retain their original window.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.verify(window.source, &window.token)?;
        self.healthy()?;
        if !window.bytes.belongs_to(&self.pool) {
            return Err(Error::ForeignPool);
        }
        if window.offset != self.position || self.remaining() != 0 || self.end.is_some() {
            return Err(Error::Invalid("input window is not currently requested"));
        }
        if window.bytes.len() > self.maximum_window {
            return Err(Error::Limit {
                requested: window.bytes.len(),
                maximum: self.maximum_window,
            });
        }
        let end = window
            .offset
            .checked_add(u64::try_from(window.bytes.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if self.capabilities.length.is_some_and(|length| end > length) {
            return Err(Error::Invalid("input window exceeds source length"));
        }
        self.window = (!window.bytes.is_empty()).then(|| window.bytes.clone());
        self.consumed = 0;
        Ok(())
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
            self.window = None;
            self.consumed = 0;
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
        self.window = None;
        self.consumed = 0;
        Ok(())
    }
    fn check_cancel(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.healthy()?;
        if cancel.load(Ordering::Relaxed) {
            self.failure = Some(Error::Cancelled);
            self.window = None;
            self.consumed = 0;
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
    fn advance(&mut self, count: usize) {
        // Every accepted window's end was checked before it entered the cursor.
        self.position += count as u64;
        self.consumed += count;
        if self.remaining() == 0 {
            self.window = None;
            self.consumed = 0;
        }
    }
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
        if self.remaining() == 0 {
            return Ok(self.wait());
        }
        let maximum =
            u64::try_from(output.len().min(self.remaining())).map_err(|_| Error::Overflow)?;
        let count = work.take(maximum) as usize; // bounded by the original slice length
        if count == 0 {
            return Ok(Step::Yield);
        }
        let window = self
            .window
            .as_ref()
            .ok_or(Error::Invalid("missing input window"))?;
        output[..count].copy_from_slice(&window[self.consumed..self.consumed + count]);
        self.advance(count);
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
        let count = work.take(maximum) as usize; // bounded by the original window length
        if count == 0 {
            return Ok(Step::Yield);
        }
        let window = self
            .window
            .as_ref()
            .ok_or(Error::Invalid("missing input window"))?;
        let bytes = window.slice(self.consumed..self.consumed + count)?;
        self.advance(count);
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
        self.window = None;
        self.consumed = 0;
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
