//! The reader of Annex B byte streams: NAL units between start codes.

use super::{AnnexBPrefix, NalFacts, NalUnit, ReaderLimits};
use crate::nal::{Codec, NalHeader};
use crate::{freeze_whole, retryable};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
    nonzero_prefix_len,
};

/// The prefix fields before a start code is read.
const UNREAD: AnnexBPrefix = AnnexBPrefix {
    offset: 0,
    leading_zero_bytes: 0,
    start_code_bytes: 0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Zero bytes before the first start code.
    Leading,
    Reserve,
    Payload,
    /// Zeros that turned out to be payload, then `pending_byte`, are appended.
    Append,
    Finish,
    End,
}

/// Reads the NAL units of an Annex B source. `prefix` is the start code of the unit
/// being read from `Reserve` on; `next` is the start code that ended it, read in
/// `Payload` and taken over at `Finish`.
#[derive(Debug)]
pub(crate) struct AnnexBNalReader {
    cursor: InputCursor,
    codec: Codec,
    limits: ReaderLimits,
    // The buffer grows with the unit instead of reserving `maximum_nal_bytes`.
    compact: bool,
    phase: Phase,
    buffer: BudgetVec<u8>,
    prefix: AnnexBPrefix,
    next: AnnexBPrefix,
    zeroes: usize,
    pending_byte: Option<u8>,
    append_finishes: bool,
    trailing_zero_bytes: usize,
    end_after_nal: bool,
    completed: usize,
    failure: Option<Error>,
}

impl AnnexBNalReader {
    /// A reader at offset zero of `source`; `compact` grows each unit's buffer as its
    /// bytes arrive. Fails as [`NalReader::new`](super::NalReader::new) does.
    pub(crate) fn new(
        pool: &Budget,
        codec: Codec,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: ReaderLimits,
        compact: bool,
    ) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            cursor: InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?,
            codec,
            limits,
            compact,
            phase: Phase::Leading,
            buffer: BudgetVec::new(pool),
            prefix: UNREAD,
            next: UNREAD,
            zeroes: 0,
            pending_byte: None,
            append_finishes: false,
            trailing_zero_bytes: 0,
            end_after_nal: false,
            completed: 0,
            failure: None,
        })
    }

    pub(crate) fn resume(&mut self, offset: u64, completed: usize) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !matches!(self.phase, Phase::Leading)
            || self.cursor.position() != 0
            || self.cursor.buffered_owner().is_some()
            || self.zeroes != 0
            || self.completed != 0
            || completed >= self.limits.maximum_nal_units
        {
            return Err(Error::SourceMismatch);
        }
        let mut cursor = InputCursor::new(
            self.cursor.budget(),
            self.cursor.source(),
            SourceCapabilities {
                random_access: true,
                ..self.cursor.capabilities()
            },
            self.limits.maximum_input_window,
        )?;
        cursor.request_range(offset)?;
        self.cursor = cursor;
        self.completed = completed;
        Ok(())
    }

    pub(crate) fn request(&self) -> InputRequest {
        self.cursor.request()
    }
    pub(crate) fn token(&self) -> InputToken {
        self.cursor.token()
    }
    pub(crate) fn position(&self) -> u64 {
        self.cursor.position()
    }
    /// True once the source ended after the last NAL unit read: that unit is
    /// the final one.
    pub(crate) const fn ended(&self) -> bool {
        self.end_after_nal
    }
    pub(crate) const fn limits(&self) -> ReaderLimits {
        self.limits
    }
    /// The error that ended the reader, if any.
    pub(crate) const fn failure(&self) -> Option<Error> {
        self.failure
    }
    pub(crate) fn source(&self) -> SourceId {
        self.cursor.source()
    }
    pub(crate) fn budget(&self) -> &Budget {
        self.cursor.budget()
    }

    pub(crate) fn input_wait_buffer_capacity(&self) -> Result<usize> {
        if self.cursor.buffered_owner().is_some() {
            return Err(Error::Invalid("NAL reader has buffered input"));
        }
        Ok(self.buffer.capacity())
    }

    pub(crate) fn provide(&mut self, window: &InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide(window)
    }

    pub(crate) fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide_owned(window)
    }

    /// Takes `bytes` as the answer to `request` when they lie wholly inside the unit
    /// being read; see [`RawAnnexBReader::append_payload`](crate::nal::RawAnnexBReader::append_payload).
    pub(crate) fn append_payload(
        &mut self,
        request: &InputRequest,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.compact
            || self.phase != Phase::Payload
            || self.zeroes != 0
            || bytes.len() < 32
            || bytes.len() > 4096
            || bytes.last() == Some(&0)
        {
            return Ok(false);
        }
        let count = bytes.len() as u64;
        let scan = count / 8 + count % 8;
        if work.remaining() < count + scan {
            return Ok(false);
        }
        work.take(scan);
        if contains_zero_pair(bytes) {
            return Ok(false);
        }
        let maximum = self.limits.maximum_nal_bytes;
        let buffer = &mut self.buffer;
        match self
            .cursor
            .consume_response(request, bytes.len(), work, cancel, || {
                reserve_nal_bytes(buffer, maximum, bytes.len())?;
                buffer.try_extend_from_slice(bytes)
            }) {
            Err(Error::Backpressure { .. } | Error::Allocation | Error::Limit { .. }) => Ok(false),
            result => result,
        }
    }

    pub(crate) fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.cursor.finish(source, token);
        if matches!(result, Err(Error::Truncated)) {
            self.stop(Error::Truncated);
        }
        result
    }

    pub(crate) fn fail(
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

    pub(crate) fn stop(&mut self, error: Error) {
        if error == Error::Cancelled {
            let _ = self.cursor.fail(
                self.cursor.source(),
                self.cursor.token(),
                SourceFailure::Cancelled,
            );
        }
        self.failure = Some(error);
        self.buffer = BudgetVec::new(self.cursor.budget());
        self.pending_byte = None;
    }

    /// Reads towards the next NAL unit, as [`NalReader::step`](super::NalReader::step)
    /// does.
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<NalUnit<AnnexBPrefix>>> {
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

    fn check_padding(&self, count: usize) -> Result<()> {
        if count > self.limits.maximum_padding_bytes {
            return Err(Error::Limit {
                requested: count,
                maximum: self.limits.maximum_padding_bytes,
            });
        }
        Ok(())
    }

    fn add_zero(&mut self) -> Result<()> {
        let count = self.zeroes.checked_add(1).ok_or(Error::Overflow)?;
        let maximum = self
            .limits
            .maximum_padding_bytes
            .checked_add(3)
            .ok_or(Error::Overflow)?;
        if count > maximum {
            return Err(Error::Limit {
                requested: count,
                maximum,
            });
        }
        self.zeroes = count;
        Ok(())
    }

    fn append_byte(&mut self, byte: u8) -> Result<()> {
        let count = self.buffer.len().checked_add(1).ok_or(Error::Overflow)?;
        if count > self.limits.maximum_nal_bytes {
            return Err(Error::Limit {
                requested: count,
                maximum: self.limits.maximum_nal_bytes,
            });
        }
        self.reserve_compact_payload(1)?;
        self.buffer
            .try_push(byte)
            .map_err(|rejected| rejected.error)
    }

    fn reserve_compact_payload(&mut self, count: usize) -> Result<()> {
        if !self.compact {
            return Ok(());
        }
        let maximum = self.limits.maximum_nal_bytes;
        reserve_nal_bytes(&mut self.buffer, maximum, count)
    }

    fn observed_prefix(&self, leading_zero_bytes: usize) -> Result<AnnexBPrefix> {
        let width = if self.zeroes >= 3 { 4_u8 } else { 3_u8 };
        let offset = self
            .cursor
            .position()
            .checked_sub(u64::from(width))
            .ok_or(Error::Overflow)?;
        Ok(AnnexBPrefix {
            offset,
            leading_zero_bytes,
            start_code_bytes: width,
        })
    }

    fn step_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<NalUnit<AnnexBPrefix>>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Leading => {
                    let mut byte = [0];
                    match self.cursor.read_into(&mut byte, work, cancel)? {
                        Step::Ready(_) if byte[0] == 0 => self.add_zero()?,
                        Step::Ready(_) if byte[0] == 1 && self.zeroes >= 2 => {
                            let leading = self.zeroes - if self.zeroes >= 3 { 3 } else { 2 };
                            self.check_padding(leading)?;
                            self.prefix = self.observed_prefix(leading)?;
                            self.zeroes = 0;
                            self.phase = Phase::Reserve;
                        }
                        Step::Ready(_) => return Err(Error::Invalid("Annex B leading bytes")),
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(location) => {
                            if self.zeroes != 0 {
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
                    let capacity = if self.compact {
                        0
                    } else {
                        self.limits.maximum_nal_bytes
                    };
                    self.buffer.try_reserve_exact(capacity)?;
                    self.phase = Phase::Payload;
                }
                Phase::Payload => {
                    if self.compact && self.zeroes == 0 {
                        let window = self.cursor.buffered();
                        let maximum = window.len().min(work.remaining().min(4096) as usize);
                        let count = nonzero_prefix_len(&window[..maximum]);
                        if count != 0 {
                            self.reserve_compact_payload(count)?;
                            match self
                                .cursor
                                .read_append(&mut self.buffer, count, work, cancel)?
                            {
                                Step::Ready(_) => continue,
                                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                                Step::Yield => return Ok(Step::Yield),
                                Step::End(_) => return Err(Error::Truncated),
                            }
                        }
                    }
                    let mut byte = [0];
                    match self.cursor.read_into(&mut byte, work, cancel)? {
                        Step::Ready(_) if byte[0] == 0 => self.add_zero()?,
                        Step::Ready(_) if byte[0] == 1 && self.zeroes >= 2 => {
                            let prefix_zeroes = if self.zeroes >= 3 { 3 } else { 2 };
                            self.trailing_zero_bytes = self.zeroes - prefix_zeroes;
                            self.check_padding(self.trailing_zero_bytes)?;
                            self.next = self.observed_prefix(0)?;
                            self.zeroes = 0;
                            if self.buffer.is_empty() {
                                return Err(Error::Invalid("empty Annex B NAL"));
                            }
                            self.phase = Phase::Finish;
                        }
                        Step::Ready(_) => {
                            // Annex B terminates a NAL at 00 00 00. Following
                            // nonzero garbage is not silently spliced into it.
                            if self.zeroes >= 3 {
                                return Err(Error::Invalid("Annex B nonzero trailing padding"));
                            }
                            self.pending_byte = Some(byte[0]);
                            self.append_finishes = false;
                            self.phase = Phase::Append;
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) => {
                            self.end_after_nal = true;
                            if self.zeroes >= 3 {
                                self.check_padding(self.zeroes)?;
                                self.trailing_zero_bytes = self.zeroes;
                                self.zeroes = 0;
                                self.phase = Phase::Finish;
                            } else {
                                // Literal Annex-B extraction process: without
                                // 00 00 00 / 00 00 01, a terminal 1-2 zeros
                                // belongs to the NAL. No RBSP inference occurs.
                                self.append_finishes = true;
                                self.phase = Phase::Append;
                            }
                        }
                    }
                }
                Phase::Append => {
                    if self.zeroes != 0 {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        self.append_byte(0)?;
                        self.zeroes -= 1;
                    } else if let Some(byte) = self.pending_byte {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        self.append_byte(byte)?;
                        self.pending_byte = None;
                    } else {
                        self.phase = if self.append_finishes {
                            Phase::Finish
                        } else {
                            Phase::Payload
                        };
                    }
                }
                Phase::Finish => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let header = NalHeader::parse(self.codec, self.buffer.as_slice())?;
                    let framing = self.prefix;
                    let payload_offset = framing
                        .offset
                        .checked_add(u64::from(framing.start_code_bytes))
                        .ok_or(Error::Overflow)?;
                    let bytes = freeze_whole(&mut self.buffer)?;
                    let facts = NalFacts {
                        source: self.cursor.source(),
                        framing,
                        payload_offset,
                        payload_length: bytes.len(),
                        trailing_zero_bytes: self.trailing_zero_bytes,
                        header,
                    };
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    self.trailing_zero_bytes = 0;
                    self.prefix = self.next;
                    self.phase = if self.end_after_nal {
                        Phase::End
                    } else {
                        Phase::Reserve
                    };
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
}

fn reserve_nal_bytes(buffer: &mut BudgetVec<u8>, maximum: usize, count: usize) -> Result<()> {
    let needed = buffer.len().checked_add(count).ok_or(Error::Overflow)?;
    if needed > maximum {
        return Err(Error::Limit {
            requested: needed,
            maximum,
        });
    }
    if needed > buffer.capacity() {
        let target = buffer
            .capacity()
            .saturating_mul(2)
            .max(64)
            .min(maximum)
            .max(needed);
        let length = buffer.len();
        buffer.try_reserve_exact(target - length).or_else(|error| {
            if target != needed && matches!(error, Error::Backpressure { .. } | Error::Limit { .. })
            {
                buffer.try_reserve_exact(needed - length)
            } else {
                Err(error)
            }
        })?;
    }
    Ok(())
}

fn contains_zero_pair(bytes: &[u8]) -> bool {
    let Some(right) = bytes.get(1..) else {
        return false;
    };
    let (left_words, left_tail) = bytes[..right.len()].as_chunks::<8>();
    let (right_words, right_tail) = right.as_chunks::<8>();
    let mut zeroes = 0;
    // The shifted slice includes pairs that straddle word boundaries.
    for (&left, &right) in left_words.iter().zip(right_words) {
        let pairs = u64::from_le_bytes(left) | u64::from_le_bytes(right);
        zeroes |= pairs.wrapping_sub(0x0101_0101_0101_0101) & !pairs & 0x8080_8080_8080_8080;
    }
    zeroes != 0 || left_tail.iter().zip(right_tail).any(|(&a, &b)| a | b == 0)
}

#[cfg(test)]
mod tests {
    use super::contains_zero_pair;

    #[test]
    fn zero_pair_scan_matches_adjacent_bytes_at_word_boundaries() {
        let mut bytes = [1; 48];
        for first in 0..bytes.len() {
            bytes[first] = 0;
            for second in first..bytes.len() {
                bytes[second] = 0;
                for end in second + 1..=bytes.len() {
                    let prefix = &bytes[..end];
                    assert_eq!(
                        contains_zero_pair(prefix),
                        prefix.windows(2).any(|pair| pair == [0, 0])
                    );
                }
                bytes[second] = 1;
                bytes[first] = 0;
            }
            bytes[first] = 1;
        }
    }
}
