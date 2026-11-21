//! Selected aligned ADTS stream envelopes; source lifecycle is the shared core.
mod phases;

use super::carrier::AacAdtsCarrier;
use super::{AacAdtsFrame, AacAdtsHeader};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, Step, WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAdtsStreamLimits {
    pub maximum_frame_bytes: usize,
    pub maximum_frames: usize,
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

/// Strict aligned, selected single-block ADTS envelopes. No recovery scan,
/// encoded-audio validation, CRC verification or clock/config epoch is inferred.
/// Input generation is the real shared-cursor epoch; every output keeps its
/// physical source offset, full header/CRC/payload and charged immutable owner.
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
enum StreamOutput {
    Selected(AacAdtsFrame),
    Carrier(AacAdtsCarrier),
}
impl AacAdtsStreamReader {
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

    pub fn request(&self) -> InputRequest {
        self.cursor
            .as_ref()
            .map_or_else(|| self.parked.clone(), InputCursor::request)
    }
    pub fn token(&self) -> InputToken {
        self.request().token
    }
    pub fn generation(&self) -> u64 {
        self.token().generation()
    }
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
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.cursor_mut()?.provide(window)
    }
    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        self.cursor_mut()?.provide_owned(window)
    }
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
    /// An explicit read epoch for the same physical source. Invalid resets do
    /// not change state. Source length learned from real finish is retained;
    /// buffered syntax/EOF/failure and frame count are reset together.
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
            .and_then(InputCursor::retained_window)
            .map_or(0, |b| b.retained_storage_bytes());
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
