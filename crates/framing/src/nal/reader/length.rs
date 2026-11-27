//! The reader of length-prefixed NAL units.

use super::{LengthSize, NalFacts, NalFraming, NalUnit, ReaderLimits};
use crate::nal::{Codec, NalHeader};
use crate::{freeze_whole, retryable};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Length,
    Reserve,
    Payload,
    Finish,
    End,
}

/// Reads NAL units that each follow a big-endian length field of `width` bytes.
#[derive(Debug)]
pub(super) struct LengthNalReader {
    cursor: InputCursor,
    codec: Codec,
    width: LengthSize,
    limits: ReaderLimits,
    phase: Phase,
    buffer: BudgetVec<u8>,
    prefix_offset: u64,
    length_used: u8,
    length_value: u32,
    declared_length: usize,
    completed: usize,
    failure: Option<Error>,
}

impl LengthNalReader {
    pub(super) fn new(
        pool: &Budget,
        codec: Codec,
        width: LengthSize,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: ReaderLimits,
    ) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            cursor: InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?,
            codec,
            width,
            limits,
            phase: Phase::Length,
            buffer: BudgetVec::new(pool),
            prefix_offset: 0,
            length_used: 0,
            length_value: 0,
            declared_length: 0,
            completed: 0,
            failure: None,
        })
    }

    pub(super) fn request(&self) -> InputRequest {
        self.cursor.request()
    }
    pub(super) fn token(&self) -> InputToken {
        self.cursor.token()
    }
    pub(super) fn position(&self) -> u64 {
        self.cursor.position()
    }

    pub(super) fn provide(&mut self, window: &InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide(window)
    }

    pub(super) fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide_owned(window)
    }

    pub(super) fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.cursor.finish(source, token);
        if matches!(result, Err(Error::Truncated)) {
            self.stop(Error::Truncated);
        }
        result
    }

    pub(super) fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.fail(source, token, failure)?;
        self.stop(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }

    fn stop(&mut self, error: Error) {
        if error == Error::Cancelled {
            let _ = self.cursor.fail(
                self.cursor.source(),
                self.cursor.token(),
                SourceFailure::Cancelled,
            );
        }
        self.failure = Some(error);
        self.buffer = BudgetVec::new(self.cursor.budget());
    }

    pub(super) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<NalUnit>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.stop(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.step_inner(work, cancel);
        if let Err(error) = result
            && !retryable(error)
        {
            self.stop(error);
        }
        result
    }

    fn step_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<NalUnit>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Length => {
                    if self.length_used == 0 {
                        self.prefix_offset = self.cursor.position();
                    }
                    let mut byte = [0];
                    match self.cursor.read_into(&mut byte, work, cancel)? {
                        Step::Ready(_) => {
                            self.length_value = self
                                .length_value
                                .checked_mul(256)
                                .and_then(|value| value.checked_add(u32::from(byte[0])))
                                .ok_or(Error::Overflow)?;
                            self.length_used += 1;
                            if self.length_used == self.width.bytes() {
                                self.declared()?;
                            }
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(location) => {
                            if self.length_used != 0 {
                                return Err(Error::Truncated);
                            }
                            self.phase = Phase::End;
                            return Ok(Step::End(location));
                        }
                    }
                }
                Phase::Reserve => {
                    if self.completed >= self.limits.maximum_nal_units {
                        return Err(Error::Limit {
                            requested: self.completed.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.maximum_nal_units,
                        });
                    }
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.buffer.try_reserve_exact(self.declared_length)?;
                    self.phase = Phase::Payload;
                }
                Phase::Payload => {
                    let remaining = self
                        .declared_length
                        .checked_sub(self.buffer.len())
                        .ok_or(Error::Overflow)?;
                    if remaining == 0 {
                        self.phase = Phase::Finish;
                        continue;
                    }
                    // Recheck cancellation between bounded copies even if the
                    // caller grants a very large step budget/window.
                    match self.cursor.read_span(remaining.min(4096), work, cancel)? {
                        Step::Ready(bytes) => {
                            self.buffer.try_extend_from_slice(bytes.as_ref())?;
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                    }
                }
                Phase::Finish => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let header = NalHeader::parse(self.codec, self.buffer.as_slice())?;
                    let payload_offset = self
                        .prefix_offset
                        .checked_add(u64::from(self.width.bytes()))
                        .ok_or(Error::Overflow)?;
                    let bytes = freeze_whole(&mut self.buffer)?;
                    let facts = NalFacts {
                        source: self.cursor.source(),
                        framing: NalFraming::LengthPrefixed {
                            prefix_offset: self.prefix_offset,
                            length_size: self.width,
                            declared_length: self.length_value,
                        },
                        payload_offset,
                        payload_length: bytes.len(),
                        trailing_zero_bytes: 0,
                        header,
                    };
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    self.length_used = 0;
                    self.length_value = 0;
                    self.phase = Phase::Length;
                    return Ok(Step::Ready(NalUnit { bytes, facts }));
                }
                Phase::End => {
                    return Ok(Step::End(SourceLocation {
                        source: self.cursor.source(),
                        offset: self.cursor.position(),
                    }));
                }
            }
        }
    }

    /// Checks the length field just read against the limits and the source's
    /// known length.
    fn declared(&mut self) -> Result<()> {
        if self.length_value == 0 {
            return Err(Error::Invalid("zero-length NAL"));
        }
        self.declared_length = usize::try_from(self.length_value).map_err(|_| Error::Overflow)?;
        if self.declared_length > self.limits.maximum_nal_bytes {
            return Err(Error::Limit {
                requested: self.declared_length,
                maximum: self.limits.maximum_nal_bytes,
            });
        }
        let end = self
            .cursor
            .position()
            .checked_add(u64::from(self.length_value))
            .ok_or(Error::Overflow)?;
        if self
            .cursor
            .capabilities()
            .length
            .is_some_and(|length| end > length)
        {
            return Err(Error::Truncated);
        }
        self.phase = Phase::Reserve;
        Ok(())
    }
}
