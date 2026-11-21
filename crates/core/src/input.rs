//! The input side of a parser: requests for byte ranges, the windows that
//! answer them, and the cursor that reads them. Nothing here performs I/O or
//! knows a format's packet or segment boundaries.
use crate::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};
use std::sync::atomic::{AtomicBool, Ordering};

const INLINE_INPUT_BYTES: usize = 12;

/// The caller's name for one source entity: a file, or one segment of a
/// playlist. The cursor only compares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub u64);

/// What the caller knows about a source before reading it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceCapabilities {
    /// Length in bytes; `None` when unknown, which is not an empty source.
    pub length: Option<u64>,
    /// Whether the caller can serve a request at any offset. Without it a
    /// parser reads forward from offset zero only.
    pub random_access: bool,
}

/// Proof that a response answers the request it claims to.
///
/// A cursor's token changes with every range request, reset and source
/// continuation, and no two cursors share one: each takes a generation from its
/// pool, and pools have distinct identities. An equal token therefore means the
/// same cursor, source and request.
#[derive(Clone, Debug)]
pub struct InputToken {
    pool: crate::budget::PoolId,
    generation: u64,
    request: u64,
}
impl InputToken {
    /// The cursor generation, new for every cursor and after every
    /// [`InputCursor::reset`].
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// The range request within the generation, counted from zero.
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

/// A byte offset in a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLocation {
    /// The source.
    pub source: SourceId,
    /// Bytes from its start.
    pub offset: u64,
}

/// What a parser needs next: bytes of `source` starting at `offset`.
///
/// Answer with a window of 1 to `maximum` bytes at that offset, with the end of
/// the source (`finish`) or with a failure (`fail`), each carrying `token`.
#[derive(Clone, Debug)]
pub struct InputRequest {
    /// The source to read.
    pub source: SourceId,
    /// Returned with the answer.
    pub token: InputToken,
    /// Offset of the first byte wanted.
    pub offset: u64,
    /// The most bytes one answer may carry. Fewer are fine, down to one; an
    /// empty answer is accepted and means "none yet", never the end.
    pub maximum: usize,
}

/// Bytes of a source at a known offset, answering an [`InputRequest`].
#[derive(Clone, Debug)]
pub struct InputWindow {
    /// The source the bytes come from.
    pub source: SourceId,
    /// The token of the request answered.
    pub token: InputToken,
    /// Offset of the first byte.
    pub offset: u64,
    bytes: Bytes,
}
impl InputWindow {
    /// A window of `bytes` at `offset`. Fails with [`Error::Overflow`] when its
    /// end exceeds `u64::MAX`.
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
    /// The window's bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

/// Why the caller could not answer a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFailure {
    /// The transport failed; reads return [`Error::Transport`].
    Transport,
    /// The caller gave up; reads return [`Error::Cancelled`].
    Cancelled,
}

/// The pool is full of output the caller still holds: `requested` bytes were
/// needed and `available` were free. Release output, then step again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backpressure {
    /// Bytes the step needed.
    pub requested: usize,
    /// Bytes the pool had free.
    pub available: usize,
}

/// The outcome of one bounded unit of resumable work. Every variant but
/// `Ready` leaves the operation where it was, to be called again.
///
/// `B` is the backpressure payload. Operations that retain no output storage
/// (input cursors, most framers) use the default uninhabited `Infallible`, so
/// their callers match without a backpressure arm; readers that own output
/// storage return `Step<T, Backpressure>`.
#[derive(Debug)]
pub enum Step<T, B = std::convert::Infallible> {
    /// The work produced a value.
    Ready(T),
    /// More input is needed: answer the request, then call again.
    NeedInput(InputRequest),
    /// The work budget is spent: call again with a new one.
    Yield,
    /// The caller holds too much output: release some, then call again.
    Backpressure(B),
    /// The source entity ended at this location. Whether that also ends the
    /// stream is for the parser above to decide: a playlist continues with its
    /// next segment.
    End(SourceLocation),
}

/// Why a cursor stopped for good. Every read tests it, so it is kept to one
/// byte; the matching [`Error`] is built only when it is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stopped {
    Truncated,
    Transport,
    Cancelled,
}
impl From<Stopped> for Error {
    fn from(stopped: Stopped) -> Self {
        match stopped {
            Stopped::Truncated => Error::Truncated,
            Stopped::Transport => Error::Transport,
            Stopped::Cancelled => Error::Cancelled,
        }
    }
}

/// A parser's read position in one source, holding at most one answered
/// window at a time.
///
/// The cursor never reads on its own: a read that finds no buffered byte
/// returns [`Step::NeedInput`], the caller answers with
/// [`provide`](Self::provide), [`finish`](Self::finish) or [`fail`](Self::fail),
/// and the read is called again. Every read first reports a failure that was
/// recorded earlier, then checks the cancellation flag; a truncated, failed or
/// cancelled cursor stays so until [`reset`](Self::reset). Each byte read costs
/// one unit of the [`WorkBudget`].
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
    failure: Option<Stopped>,
}

impl InputCursor {
    /// The most bytes [`provide_inline`](Self::provide_inline) accepts.
    pub const INLINE_CAPACITY: usize = INLINE_INPUT_BYTES;

    /// A cursor at offset zero of `source`. Windows must be charged to `pool`
    /// and hold at most `maximum_window` bytes, which must not be zero.
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
    /// The pool windows must be charged to.
    pub fn budget(&self) -> &Budget {
        &self.pool
    }
    /// An owner of the current window's allocation, which keeps it charged
    /// after the cursor has moved on. `None` between windows and for an inline
    /// window, which has no allocation until [`read_span`](Self::read_span)
    /// gives it one.
    pub fn retained_window(&self) -> Option<Bytes> {
        self.buffered_owner().cloned()
    }
    /// The current window as it was provided, read bytes included.
    pub fn buffered_owner(&self) -> Option<&Bytes> {
        self.window.as_ref()
    }
    /// The source being read.
    pub fn source(&self) -> SourceId {
        self.source
    }
    /// The source's capabilities; `length` becomes known at
    /// [`finish`](Self::finish).
    pub fn capabilities(&self) -> SourceCapabilities {
        self.capabilities
    }
    /// Offset of the next byte a read returns.
    pub fn position(&self) -> u64 {
        self.position
    }
    /// [`InputToken::generation`] of the current token.
    pub fn generation(&self) -> u64 {
        self.token.generation
    }
    /// The token an answer must carry now.
    pub fn token(&self) -> InputToken {
        self.token.clone()
    }
    /// The request for the bytes at [`position`](Self::position): at most the
    /// window limit, and no more than a known length leaves.
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
    /// The next request, when no byte is buffered and the source has not
    /// ended; `None` otherwise. Fails as a read would on a failed or cancelled
    /// cursor.
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
        self.failure.map_or(Ok(()), |stopped| Err(stopped.into()))
    }
    fn remaining(&self) -> usize {
        self.window.as_ref().map_or(self.inline_len, Bytes::len) - self.consumed
    }
    /// The provided bytes not yet read, starting at
    /// [`position`](Self::position). Looking costs no work.
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
    /// Answers the current request with `window`, whose bytes the cursor
    /// shares. A refused window leaves the cursor unchanged: a token or source
    /// other than the current one ([`Error::StaleGeneration`],
    /// [`Error::StaleRequest`], [`Error::ForeignPool`],
    /// [`Error::SourceMismatch`]), bytes charged to another pool, more bytes
    /// than the window limit ([`Error::Limit`]), or a window that is not at
    /// [`position`](Self::position), arrives while bytes are buffered or after
    /// the end, or passes the known length ([`Error::Invalid`]).
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.validate_provision(window)?;
        self.window = (!window.bytes.is_empty()).then(|| window.bytes.clone());
        self.inline_len = 0;
        self.consumed = 0;
        Ok(())
    }
    /// [`provide`](Self::provide) for a window the caller gives away, which
    /// saves a reference count. A refused window is dropped.
    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        self.validate_provision(&window)?;
        self.window = (!window.bytes.is_empty()).then_some(window.bytes);
        self.inline_len = 0;
        self.consumed = 0;
        Ok(())
    }
    /// [`provide`](Self::provide) for at most [`INLINE_CAPACITY`](Self::INLINE_CAPACITY)
    /// bytes, copied into the cursor: no allocation and no charge.
    /// [`read_span`](Self::read_span) moves them to a charged allocation
    /// first; when that is refused the bytes stay to be read again.
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
    /// Answers the current request with the end of the source, which lies
    /// after the bytes already provided. Reads return [`Step::End`] once those
    /// are read. A source that ends before its known length is
    /// [`Error::Truncated`], now and for every later read.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.verify(source, &token)?;
        self.healthy()?;
        let end = self
            .position
            .checked_add(u64::try_from(self.remaining()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if self.capabilities.length.is_some_and(|length| length != end) {
            self.failure = Some(Stopped::Truncated);
            self.discard_window();
            return Err(Error::Truncated);
        }
        self.capabilities.length = Some(end);
        self.end = Some(end);
        Ok(())
    }
    /// Answers the current request with a failure. Buffered bytes are
    /// dropped and every later read returns the failure's error.
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.verify(source, &token)?;
        self.healthy()?;
        self.failure = Some(match failure {
            SourceFailure::Transport => Stopped::Transport,
            SourceFailure::Cancelled => Stopped::Cancelled,
        });
        self.discard_window();
        Ok(())
    }
    fn check_cancel(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.healthy()?;
        if cancel.load(Ordering::Relaxed) {
            self.failure = Some(Stopped::Cancelled);
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
    /// Copies buffered bytes to the front of `output` and returns how many:
    /// at most `output.len()`, the bytes buffered and the work left. An empty
    /// `output` is `Ready(0)`.
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
    /// [`read_into`](Self::read_into) without a destination: passes over at
    /// most `maximum` buffered bytes and returns how many.
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
    /// [`read_into`](Self::read_into) appending to `output`, at most `maximum`
    /// bytes. `output` may be charged to another pool. When it cannot grow, its
    /// error is returned and no byte or work is consumed.
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

    /// Answers `request` with `length` bytes the caller keeps and `consume`
    /// processes: the cursor checks the answer as [`provide`](Self::provide)
    /// does, runs `consume`, then moves past the bytes without ever holding
    /// them. `Ok(false)`, with nothing consumed, for an empty answer or when
    /// fewer than `length` work units are left. An error from the checks or
    /// from `consume` leaves the position unchanged.
    #[inline]
    pub fn consume_response(
        &mut self,
        request: &InputRequest,
        length: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: impl FnOnce() -> Result<()>,
    ) -> Result<bool> {
        self.verify(request.source, &request.token)?;
        self.check_cancel(cancel)?;
        self.validate_window(request.offset, length, self.maximum_window)?;
        let count = length as u64;
        if count == 0 || work.remaining() < count {
            return Ok(false);
        }
        consume()?;
        work.take(count);
        self.position += count;
        Ok(true)
    }

    /// The next buffered bytes, at most `maximum` and the work left, as a
    /// span sharing the window's allocation: no copy. A `maximum` of zero is
    /// [`Error::Invalid`].
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
    /// Moves to `offset` of the same source, dropping buffered bytes, and
    /// returns the request for the bytes there. Answers to earlier requests
    /// become [`Error::StaleRequest`]; the generation is kept. Needs random
    /// access ([`Error::Unsupported`] without), an offset within a known
    /// length, and a cursor that has not failed.
    pub fn request_range(&mut self, offset: u64) -> Result<InputRequest> {
        self.start_range(offset, false)
    }
    /// [`request_range`](Self::request_range) that keeps the current window
    /// when it has an allocation (is not inline) and holds the byte at
    /// `offset`: the bytes from there on are buffered again and need no new
    /// answer.
    pub fn request_range_reusing_window(&mut self, offset: u64) -> Result<InputRequest> {
        self.start_range(offset, true)
    }
    fn start_range(&mut self, offset: u64, reuse: bool) -> Result<InputRequest> {
        self.healthy()?;
        if !self.capabilities.random_access {
            return Err(Error::Unsupported("random-access input"));
        }
        self.validate_offset(offset)?;
        let request = self.token.request.checked_add(1).ok_or(Error::Overflow)?;
        if reuse
            && let Some(window) = &self.window
            && let Some(relative) = offset.checked_sub(self.position - self.consumed as u64)
            && relative < window.len() as u64
        {
            self.position = offset;
            self.consumed = relative as usize;
            self.end = None;
        } else {
            self.discard_at(offset);
        }
        self.token.request = request;
        Ok(self.request())
    }
    /// Restarts at `offset` with a new generation: buffered bytes, the known
    /// end and any recorded failure are dropped, and answers to every earlier
    /// request become [`Error::StaleGeneration`]. This is the seek and the
    /// recovery; the parser resets its own state with it. A non-zero offset
    /// needs random access.
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        self.validate_offset(offset)?;
        let generation = self.pool.next_generation()?;
        self.discard_at(offset);
        self.token.generation = generation;
        self.token.request = 0;
        Ok(self.request())
    }
    /// Continues at offset zero of the next source of one stream, such as the
    /// next segment of a playlist, keeping the generation. The current source
    /// must have ended and been read to its end ([`Error::Invalid`] otherwise).
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
