//! Selected aligned ADTS stream envelopes; source lifecycle is the shared core.
use super::carrier::{AacAdtsCarrier, AacAdtsCarrierBoundary};
use super::{AacAdtsFrame, AacAdtsHeader, inspect_aac_adts_header, validate_selected_header};
use crate::AacSource;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
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
            self.stop(Error::Truncated)
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
        if let Err(e) = result {
            if !matches!(e, Error::Backpressure { .. } | Error::Allocation) {
                self.stop(e)
            }
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
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<StreamOutput>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Start => {
                    self.frame_start = self.position();
                    self.used = 0;
                    self.copied = 0;
                    self.frame_bytes = 0;
                    self.header = None;
                    self.growth_target = 0;
                    self.growth_copy_credit = 0;
                    self.header_offset = 0;
                    self.eof_fragment = false;
                    self.selected_error = None;
                    self.phase = Phase::Prefix;
                }
                Phase::Prefix => {
                    if self.used == 7 {
                        self.phase = Phase::Validate;
                        continue;
                    }
                    if self.carrier && self.used == self.limits.maximum_frame_bytes {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        match self.probe_bound(cancel)? {
                            Step::End(_) => {
                                self.fragment_prefix()?;
                                continue;
                            }
                            Step::NeedInput(r) => return Ok(Step::NeedInput(r)),
                            Step::Yield => return Err(self.packet_limit()),
                            Step::Ready(_) => {
                                return Err(Error::Invalid("zero-work bound probe consumed input"));
                            }
                        }
                    }
                    let cursor = self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("missing ADTS input cursor"))?;
                    let end = if self.carrier {
                        self.limits.maximum_frame_bytes.min(7)
                    } else {
                        7
                    };
                    match cursor.read_into(&mut self.prefix[self.used..end], work, cancel)? {
                        Step::Ready(n) => {
                            if n == 0 {
                                return Err(Error::Invalid("zero ADTS prefix read"));
                            }
                            self.used = self.used.checked_add(n).ok_or(Error::Overflow)?;
                        }
                        Step::NeedInput(r) => return Ok(Step::NeedInput(r)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(e) => {
                            if self.used == 0 {
                                self.phase = Phase::End;
                                return Ok(Step::End(e));
                            }
                            if self.carrier {
                                self.fragment_prefix()?;
                                continue;
                            }
                            return Err(Error::Truncated);
                        }
                    }
                }
                Phase::Validate => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if self.completed >= self.limits.maximum_frames {
                        return Err(Error::Limit {
                            requested: self.completed.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.maximum_frames,
                        });
                    }
                    let h = inspect_aac_adts_header(&self.prefix)?;
                    if self.carrier {
                        self.selected_error = validate_selected_header(h).err();
                        if self.plausible_envelope(h) {
                            let n = usize::from(h.frame_length());
                            if n > self.limits.maximum_frame_bytes {
                                return Err(Error::Limit {
                                    requested: n,
                                    maximum: self.limits.maximum_frame_bytes,
                                });
                            }
                            if h.single_block_header_bytes().is_some_and(|v| n < v) {
                                self.selected_error = Some(Error::Invalid(
                                    "ADTS declared extent smaller than header",
                                ));
                            }
                            self.header = Some(h);
                            self.frame_bytes = n;
                        } else {
                            self.selected_error = self
                                .selected_error
                                .or(Some(Error::Invalid("ADTS envelope extent")));
                            self.frame_bytes = self.limits.maximum_frame_bytes;
                        }
                        self.phase = Phase::ReserveHeader;
                        continue;
                    }
                    validate_selected_header(h)?;
                    let frame_bytes = usize::from(h.frame_length());
                    let header_bytes = h
                        .single_block_header_bytes()
                        .ok_or(Error::Unsupported("ADTS block layout"))?;
                    if frame_bytes < header_bytes {
                        return Err(Error::Invalid("ADTS declared extent smaller than header"));
                    }
                    if frame_bytes > self.limits.maximum_frame_bytes {
                        return Err(Error::Limit {
                            requested: frame_bytes,
                            maximum: self.limits.maximum_frame_bytes,
                        });
                    }
                    let end = self
                        .frame_start
                        .checked_add(u64::try_from(frame_bytes).map_err(|_| Error::Overflow)?)
                        .ok_or(Error::Overflow)?;
                    if self
                        .cursor
                        .as_ref()
                        .ok_or(Error::Invalid("missing ADTS input cursor"))?
                        .capabilities()
                        .length
                        .is_some_and(|n| end > n)
                    {
                        return Err(Error::Truncated);
                    }
                    self.header = Some(h);
                    self.frame_bytes = frame_bytes;
                    self.phase = Phase::ReserveHeader;
                }
                Phase::ReserveHeader => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if self.carrier && self.completed >= self.limits.maximum_frames {
                        return Err(Error::Limit {
                            requested: self.completed.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.maximum_frames,
                        });
                    }
                    if self.buffer.is_none() {
                        self.buffer = Some(BudgetVec::new(&self.pool));
                    }
                    self.check_minimum(self.used)?;
                    self.buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing ADTS builder"))?
                        .try_reserve_exact(self.used)?;
                    self.phase = Phase::CopyHeader;
                }
                Phase::CopyHeader => {
                    if self.copied == self.used {
                        self.phase = Phase::Body;
                        continue;
                    }
                    let n = usize::try_from(work.take(
                        u64::try_from(self.used - self.copied).map_err(|_| Error::Overflow)?,
                    ))
                    .map_err(|_| Error::Overflow)?;
                    if n == 0 {
                        return Ok(Step::Yield);
                    }
                    self.buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing ADTS builder"))?
                        .try_extend_from_slice(&self.prefix[self.copied..self.copied + n])?;
                    self.copied += n;
                }
                Phase::Body => {
                    let b = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("missing ADTS builder"))?;
                    let remaining = self
                        .frame_bytes
                        .checked_sub(b.len())
                        .ok_or(Error::Overflow)?;
                    let scanning = self.carrier && self.header.is_none() && !self.eof_fragment;
                    if remaining == 0 {
                        if scanning {
                            if !work.try_charge(1) {
                                return Ok(Step::Yield);
                            }
                            match self.probe_bound(cancel)? {
                                Step::End(_) => {
                                    self.eof_fragment = true;
                                    self.phase = Phase::Freeze;
                                    continue;
                                }
                                Step::NeedInput(r) => return Ok(Step::NeedInput(r)),
                                Step::Yield => return Err(self.packet_limit()),
                                Step::Ready(_) => {
                                    return Err(Error::Invalid(
                                        "zero-work bound probe consumed input",
                                    ));
                                }
                            }
                        }
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    let room = b.capacity().checked_sub(b.len()).ok_or(Error::Overflow)?;
                    if room == 0 {
                        let needed = b.len().checked_add(1).ok_or(Error::Overflow)?;
                        let doubled = b.capacity().checked_mul(2).ok_or(Error::Overflow)?;
                        self.growth_target = needed.max(doubled).min(self.frame_bytes);
                        self.growth_copy_credit = b.len();
                        self.phase = Phase::Grow;
                        continue;
                    }
                    let cursor = self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("missing ADTS input cursor"))?;
                    let buffer = self
                        .buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing ADTS builder"))?;
                    let maximum = if scanning {
                        1
                    } else {
                        remaining.min(room).min(4096)
                    };
                    match cursor.read_append(buffer, maximum, work, cancel)? {
                        Step::Ready(n) => {
                            if n == 0 {
                                return Err(Error::Invalid("zero ADTS body read"));
                            }
                            if scanning {
                                self.prefix.copy_within(1..7, 0);
                                self.prefix[6] = *buffer
                                    .as_slice()
                                    .last()
                                    .ok_or(Error::Invalid("missing ADTS scan byte"))?;
                                self.phase = Phase::ScanCheck;
                            }
                        }
                        Step::NeedInput(r) => return Ok(Step::NeedInput(r)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) => {
                            if !self.carrier {
                                return Err(Error::Truncated);
                            }
                            self.frame_bytes = buffer.len();
                            self.eof_fragment = true;
                            self.selected_error = self.selected_error.or(Some(Error::Truncated));
                            self.phase = Phase::Freeze;
                        }
                    }
                }
                Phase::ScanCheck => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let h = inspect_aac_adts_header(&self.prefix)?;
                    if self.plausible_envelope(h) {
                        let b = self
                            .buffer
                            .as_ref()
                            .ok_or(Error::Invalid("missing ADTS scan builder"))?;
                        let start = b.len().checked_sub(7).ok_or(Error::Overflow)?;
                        let end = start
                            .checked_add(usize::from(h.frame_length()))
                            .ok_or(Error::Overflow)?;
                        if end > self.limits.maximum_frame_bytes {
                            return Err(Error::Limit {
                                requested: end,
                                maximum: self.limits.maximum_frame_bytes,
                            });
                        }
                        self.header_offset = start;
                        self.header = Some(h);
                        self.frame_bytes = end;
                        if self.split_prefix {
                            self.phase = Phase::EmitPrefix;
                            continue;
                        }
                    }
                    self.phase = Phase::Body;
                }
                Phase::EmitPrefix => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.check_minimum(Bytes::owner_overhead_bytes())?;
                    let b = self
                        .buffer
                        .take()
                        .ok_or(Error::Invalid("missing recovery prefix"))?;
                    let raw = match b.freeze() {
                        Ok(b) => b,
                        Err(e) => {
                            self.buffer = Some(e.buffer);
                            return Err(e.error);
                        }
                    };
                    let prefix = raw.slice(0..self.header_offset)?;
                    let cursor = self
                        .cursor
                        .as_ref()
                        .ok_or(Error::Invalid("missing ADTS input cursor"))?;
                    let source = AacSource {
                        source: cursor.source(),
                        generation: cursor.generation(),
                        offset: self.frame_start,
                    };
                    let inspection_end = cursor.position();
                    let error = self.selected_error;
                    self.frame_start = self
                        .frame_start
                        .checked_add(
                            u64::try_from(self.header_offset).map_err(|_| Error::Overflow)?,
                        )
                        .ok_or(Error::Overflow)?;
                    let h = self
                        .header
                        .ok_or(Error::Invalid("missing recovered header"))?;
                    self.frame_bytes = usize::from(h.frame_length());
                    self.header_offset = 0;
                    self.used = 7;
                    self.copied = 0;
                    self.selected_error = validate_selected_header(h).err();
                    if h.single_block_header_bytes()
                        .is_some_and(|n| self.frame_bytes < n)
                    {
                        self.selected_error =
                            Some(Error::Invalid("ADTS declared extent smaller than header"));
                    }
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    self.phase = Phase::ReserveHeader;
                    return Ok(Step::Ready(StreamOutput::Carrier(AacAdtsCarrier {
                        raw: prefix,
                        source,
                        header: None,
                        boundary: AacAdtsCarrierBoundary::FollowingEnvelope,
                        selected_error: error,
                        inspection_end,
                    })));
                }
                Phase::Grow => {
                    // Growth moves the real old contents. Credit that copy
                    // across small work slices once; each reserve attempt then
                    // costs one unit. A retry never repeats source/setup work.
                    let paid = usize::try_from(work.take(
                        u64::try_from(self.growth_copy_credit).map_err(|_| Error::Overflow)?,
                    ))
                    .map_err(|_| Error::Overflow)?;
                    self.growth_copy_credit -= paid;
                    if self.growth_copy_credit != 0 || !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.check_minimum(self.growth_target)?;
                    let b = self
                        .buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing ADTS builder"))?;
                    b.try_reserve_exact(
                        self.growth_target
                            .checked_sub(b.len())
                            .ok_or(Error::Overflow)?,
                    )?;
                    self.phase = Phase::Body;
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.check_minimum(Bytes::owner_overhead_bytes())?;
                    let cursor = self
                        .cursor
                        .as_ref()
                        .ok_or(Error::Invalid("missing ADTS input cursor"))?;
                    let end = self
                        .frame_start
                        .checked_add(u64::try_from(self.frame_bytes).map_err(|_| Error::Overflow)?)
                        .ok_or(Error::Overflow)?;
                    if cursor.position() != end {
                        return Err(Error::Invalid("ADTS physical source extent"));
                    }
                    let source = AacSource {
                        source: cursor.source(),
                        generation: cursor.generation(),
                        offset: self.frame_start,
                    };
                    let b = self
                        .buffer
                        .take()
                        .ok_or(Error::Invalid("missing ADTS builder"))?;
                    if b.len() != self.frame_bytes {
                        return Err(Error::Invalid("ADTS complete output extent"));
                    }
                    let raw = match b.freeze() {
                        Ok(b) => b,
                        Err(e) => {
                            self.buffer = Some(e.buffer);
                            return Err(e.error);
                        }
                    };
                    if self.carrier {
                        let boundary = if self.eof_fragment {
                            AacAdtsCarrierBoundary::ExplicitEofFragment
                        } else if self.header.is_some_and(|h| h.layer() != 0) {
                            AacAdtsCarrierBoundary::ParserLengthCandidate
                        } else {
                            AacAdtsCarrierBoundary::DeclaredEnvelope {
                                header_offset: self.header_offset,
                            }
                        };
                        self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                        self.phase = Phase::Start;
                        return Ok(Step::Ready(StreamOutput::Carrier(AacAdtsCarrier {
                            raw,
                            source,
                            header: self.header,
                            boundary,
                            selected_error: self.selected_error,
                            inspection_end: cursor.position(),
                        })));
                    }
                    let h = self
                        .header
                        .ok_or(Error::Invalid("missing ADTS stream header"))?;
                    let header_bytes = h
                        .single_block_header_bytes()
                        .ok_or(Error::Unsupported("ADTS block layout"))?;
                    let crc = if h.protection_absent() {
                        None
                    } else {
                        let bytes = raw.get(7..9).ok_or(Error::Truncated)?;
                        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
                    };
                    let payload = raw.slice(header_bytes..raw.len())?;
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    self.phase = Phase::Start;
                    return Ok(Step::Ready(StreamOutput::Selected(AacAdtsFrame {
                        raw,
                        payload,
                        source,
                        header: h,
                        crc,
                    })));
                }
                Phase::End => {
                    return Ok(Step::End(SourceLocation {
                        source: self.parked.source,
                        offset: self.position(),
                    }));
                }
            }
        }
    }
}
