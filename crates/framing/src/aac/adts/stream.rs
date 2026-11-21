//! Consecutive ADTS frames read from a stream. Requests, windows, the end of the
//! source and its failures are those of the core `InputCursor`.
mod phases;

use super::carrier::AacAdtsCarrier;
use super::{AacAdtsFrame, AacAdtsHeader};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, Step, WorkBudget,
};

/// Bounds of an [`AacAdtsStreamReader`]. None may be zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAdtsStreamLimits {
    /// Most bytes one frame may hold, header included; a header that declares more
    /// is [`Error::Limit`].
    pub maximum_frame_bytes: usize,
    /// Most frames the reader returns between resets; a further one is
    /// [`Error::Limit`].
    pub maximum_frames: usize,
    /// Most bytes one input window may carry.
    pub maximum_input_window: usize,
}

#[derive(Clone, Copy)]
enum Phase {
    Start,
    Prefix,
    Validate,
    ReserveHeader,
    CopyHeader,
    Body,
    ScanCheck,
    EmitPrefix,
    Grow,
    Freeze,
    End,
}

/// Reads a stream of consecutive ADTS frames, each one AAC LC raw data block, from
/// input of any chunking.
///
/// The stream must be aligned: the first frame starts at the first byte read and
/// each further one where the last ended. A header that does not begin such a frame
/// is an error; there is no scan for the next syncword. The payload is not
/// validated, the CRC is not verified, and neither timing nor a change of
/// configuration is derived. Each frame keeps its source offset, the
/// [`generation`](Self::generation) of the input it was read from, and its header,
/// CRC and payload whole, in an immutable pool allocation of its own.
///
/// Input is requested and answered as for an [`InputCursor`].
pub struct AacAdtsStreamReader {
    pool: Budget,
    cursor: Option<InputCursor>,
    parked: InputRequest,
    capabilities: SourceCapabilities,
    limits: AacAdtsStreamLimits,
    phase: Phase,
    prefix: [u8; 7],
    used: usize,
    copied: usize,
    frame_start: u64,
    frame_bytes: usize,
    header: Option<AacAdtsHeader>,
    buffer: Option<BudgetVec<u8>>,
    growth_target: usize,
    growth_copy_credit: usize,
    completed: usize,
    failure: Option<Error>,
    carrier: bool,
    header_offset: usize,
    eof_fragment: bool,
    selected_error: Option<Error>,
    split_prefix: bool,
}
impl std::fmt::Debug for AacAdtsStreamReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AacAdtsStreamReader")
            .field("source", &self.parked.source)
            .field("position", &self.position())
            .field("capabilities", &self.capabilities)
            .field("limits", &self.limits)
            .field("completed", &self.completed)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}
enum StreamOutput {
    Selected(AacAdtsFrame),
    Carrier(AacAdtsCarrier),
}
impl AacAdtsStreamReader {
    /// A reader of `source` from offset zero, with windows charged to `pool`.
    ///
    /// # Errors
    ///
    /// When a limit is zero.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: AacAdtsStreamLimits,
    ) -> Result<Self> {
        Self::create(pool, source, capabilities, limits, false, false)
    }
    pub(super) fn new_carrier(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: AacAdtsStreamLimits,
    ) -> Result<Self> {
        Self::create(pool, source, capabilities, limits, true, false)
    }
    pub(super) fn new_parser_packets(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: AacAdtsStreamLimits,
    ) -> Result<Self> {
        Self::create(pool, source, capabilities, limits, true, true)
    }
    fn create(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: AacAdtsStreamLimits,
        carrier: bool,
        split_prefix: bool,
    ) -> Result<Self> {
        if limits.maximum_frame_bytes == 0
            || limits.maximum_frames == 0
            || limits.maximum_input_window == 0
        {
            return Err(Error::Invalid("zero ADTS stream policy"));
        }
        let cursor = InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?;
        let parked = cursor.request();
        Ok(Self {
            pool: pool.clone(),
            cursor: Some(cursor),
            parked,
            capabilities,
            limits,
            phase: Phase::Start,
            prefix: [0; 7],
            used: 0,
            copied: 0,
            frame_start: 0,
            frame_bytes: 0,
            header: None,
            buffer: None,
            growth_target: 0,
            growth_copy_credit: 0,
            completed: 0,
            failure: None,
            carrier,
            header_offset: 0,
            eof_fragment: false,
            selected_error: None,
            split_prefix,
        })
    }
    /// Capacity of the mutable frame buffer, excluding buffered source bytes.
    #[doc(hidden)]
    pub fn input_wait_buffer_capacity(&self) -> usize {
        self.buffer.as_ref().map_or(0, BudgetVec::capacity)
    }

    /// The request for the bytes the reader needs next, at
    /// [`position`](Self::position). A reader that has failed keeps its last one.
    pub fn request(&self) -> InputRequest {
        self.cursor
            .as_ref()
            .map_or_else(|| self.parked.clone(), InputCursor::request)
    }
    /// The token an answer to the current request must carry.
    pub fn token(&self) -> InputToken {
        self.request().token
    }
    /// [`InputToken::generation`] of the current token; it changes with every
    /// [`reset`](Self::reset).
    pub fn generation(&self) -> u64 {
        self.token().generation()
    }
    /// Source offset of the next byte the reader reads.
    pub fn position(&self) -> u64 {
        self.cursor
            .as_ref()
            .map_or(self.parked.offset, InputCursor::position)
    }
    fn cursor_mut(&mut self) -> Result<&mut InputCursor> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        self.cursor
            .as_mut()
            .ok_or(Error::Invalid("missing ADTS input cursor"))
    }
    /// Answers the current request with `window`, whose bytes the reader shares
    /// until it has copied them into a frame.
    ///
    /// # Errors
    ///
    /// When the window is refused as by [`InputCursor::provide`], which changes
    /// nothing, or the reader has failed.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.cursor_mut()?.provide(window)
    }
    /// [`provide`](Self::provide) for a window the caller gives away.
    ///
    /// # Errors
    ///
    /// As [`provide`](Self::provide); a refused window is dropped.
    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        self.cursor_mut()?.provide_owned(window)
    }
    /// [`provide`](Self::provide) for at most [`InputCursor::INLINE_CAPACITY`] bytes,
    /// copied into the reader without an allocation.
    ///
    /// # Errors
    ///
    /// As [`provide`](Self::provide), and when `bytes` is longer than that.
    pub fn provide_inline(
        &mut self,
        source: SourceId,
        token: InputToken,
        offset: u64,
        bytes: &[u8],
    ) -> Result<()> {
        self.cursor_mut()?
            .provide_inline(source, token, offset, bytes)
    }
    /// Answers the current request with the end of the source, which lies after
    /// the bytes already provided.
    ///
    /// # Errors
    ///
    /// When the source ends before its known length: the reader fails for good with
    /// [`Error::Truncated`]. A token or source other than the current one, or a
    /// reader that has failed, changes nothing.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        let result = self.cursor_mut()?.finish(source, token);
        if let Some(c) = &self.cursor {
            self.capabilities = c.capabilities();
        }
        if matches!(result, Err(Error::Truncated)) {
            self.stop(Error::Truncated);
        }
        result
    }
    /// Answers the current request with a failure. Buffered bytes and an unfinished
    /// frame are dropped, and every later step returns [`Error::Transport`] or
    /// [`Error::Cancelled`].
    ///
    /// # Errors
    ///
    /// When the token or source is not the current one or the reader has failed
    /// already; nothing changes.
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.cursor_mut()?.fail(source, token, failure)?;
        self.stop(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }
    /// Starts reading the same source again at `offset`, under another token
    /// generation, and returns the request for the first bytes. Buffered bytes, an
    /// unfinished frame, the end of the source, a failure and the frame count are
    /// cleared together; a source length learned from [`finish`](Self::finish) is
    /// kept.
    ///
    /// # Errors
    ///
    /// When `offset` is not zero and the source has no random access, or `offset`
    /// lies past the known length. The reader stays as it was.
    pub fn reset(&mut self, offset: u64) -> Result<InputRequest> {
        if !self.capabilities.random_access && offset != 0 {
            return Err(Error::Unsupported("random-access input"));
        }
        if self.capabilities.length.is_some_and(|n| offset > n) {
            return Err(Error::Invalid("range exceeds source length"));
        }
        let mut next = InputCursor::new(
            &self.pool,
            self.parked.source,
            self.capabilities,
            self.limits.maximum_input_window,
        )?;
        if offset != 0 {
            next.request_range(offset)?;
        }
        self.parked = next.request();
        self.cursor = Some(next);
        self.phase = Phase::Start;
        self.used = 0;
        self.copied = 0;
        self.frame_start = offset;
        self.frame_bytes = 0;
        self.header = None;
        self.buffer = None;
        self.growth_target = 0;
        self.growth_copy_credit = 0;
        self.completed = 0;
        self.failure = None;
        self.header_offset = 0;
        self.eof_fragment = false;
        self.selected_error = None;
        Ok(self.request())
    }
    /// Frames returned since the reader was made or last reset.
    pub fn completed_frames(&self) -> usize {
        self.completed
    }
    fn stop(&mut self, error: Error) {
        // Dropping the actual cursor releases a buffered input owner without
        // inventing a transport/cancel event to represent a syntax failure.
        if let Some(c) = self.cursor.take() {
            self.parked = c.request();
            self.capabilities = c.capabilities();
        }
        self.buffer = None;
        self.header = None;
        self.failure = Some(error);
    }
    /// Reads for as long as `work` lasts.
    ///
    /// [`Step::Ready`] carries the next frame. [`Step::NeedInput`] carries the
    /// request to answer with [`provide`](Self::provide), [`finish`](Self::finish)
    /// or [`fail`](Self::fail). [`Step::Yield`] asks for another call with more
    /// work. [`Step::End`] reports the end of the source at a frame boundary.
    ///
    /// # Errors
    ///
    /// When the source ends inside a frame, or a frame passes its known length;
    /// when the syncword is not `0xfff`, the layer is not 0, the sampling frequency
    /// index names no frequency or the frame length is shorter than the header and
    /// CRC; when the profile is not AAC LC or the frame holds several raw data
    /// blocks; when a frame exceeds `maximum_frame_bytes` or the frames exceed
    /// `maximum_frames`; when `cancel` is set or the source failed. The reader then
    /// returns the same error from every later call until [`reset`](Self::reset).
    /// [`Error::Backpressure`] and [`Error::Allocation`] keep its state, to be
    /// stepped again.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<AacAdtsFrame>> {
        Ok(match self.step_output(work, cancel)? {
            Step::Ready(StreamOutput::Selected(o)) => Step::Ready(o),
            Step::Ready(StreamOutput::Carrier(_)) => {
                return Err(Error::Invalid("carrier in selected ADTS reader"));
            }
            Step::NeedInput(r) => Step::NeedInput(r),
            Step::Yield => Step::Yield,
            Step::End(e) => Step::End(e),
        })
    }
    pub(super) fn step_carrier(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<AacAdtsCarrier>> {
        Ok(match self.step_output(work, cancel)? {
            Step::Ready(StreamOutput::Carrier(o)) => Step::Ready(o),
            Step::Ready(StreamOutput::Selected(_)) => {
                return Err(Error::Invalid("selected frame in ADTS carrier reader"));
            }
            Step::NeedInput(r) => Step::NeedInput(r),
            Step::Yield => Step::Yield,
            Step::End(e) => Step::End(e),
        })
    }
    fn step_output(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<StreamOutput>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.advance(work, cancel);
        if let Err(e) = result
            && !matches!(e, Error::Backpressure { .. } | Error::Allocation)
        {
            self.stop(e);
        }
        result
    }
    fn plausible_envelope(&self, h: AacAdtsHeader) -> bool {
        h.syncword() == 0xfff
            && (self.split_prefix || h.layer() == 0)
            && h.coded_frequency_hz().is_some()
            && h.frame_length() >= 7
    }
    fn fragment_prefix(&mut self) -> Result<()> {
        if self.completed >= self.limits.maximum_frames {
            return Err(Error::Limit {
                requested: self.completed.checked_add(1).ok_or(Error::Overflow)?,
                maximum: self.limits.maximum_frames,
            });
        }
        self.eof_fragment = true;
        self.selected_error = Some(Error::Truncated);
        self.frame_bytes = self.used;
        self.phase = Phase::ReserveHeader;
        Ok(())
    }
    // A zero-work read distinguishes an actual shared-cursor End from starvation
    // and buffered bytes without consuming a byte beyond the configured bound.
    fn probe_bound(&mut self, cancel: &AtomicBool) -> Result<Step<usize>> {
        self.cursor_mut()?
            .read_into(&mut [0; 1], &mut WorkBudget::new(0), cancel)
    }
    fn packet_limit(&self) -> Error {
        self.limits
            .maximum_frame_bytes
            .checked_add(1)
            .map_or(Error::Overflow, |requested| Error::Limit {
                requested,
                maximum: self.limits.maximum_frame_bytes,
            })
    }
    fn minimum(&self, additional: usize) -> Result<usize> {
        let source = self
            .cursor
            .as_ref()
            .and_then(InputCursor::buffered_owner)
            .map_or(0, Bytes::retained_storage_bytes);
        let capacity = self.buffer.as_ref().map_or(0, BudgetVec::capacity);
        Budget::control_storage_bytes()
            .checked_add(source)
            .and_then(|n| n.checked_add(capacity))
            .and_then(|n| n.checked_add(additional))
            .ok_or(Error::Overflow)
    }
    fn check_minimum(&self, additional: usize) -> Result<()> {
        let requested = self.minimum(additional)?;
        if requested > self.pool.limit() {
            Err(Error::Limit {
                requested,
                maximum: self.pool.limit(),
            })
        } else {
            Ok(())
        }
    }
}
