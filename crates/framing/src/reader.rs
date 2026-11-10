use crate::{Codec, NalHeader};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
    nonzero_prefix_len,
};

/// Width of an explicitly declared unsigned big-endian length field.
/// All widths 1..=4 are supported by this wire contract. Accepting width 3 here
/// does not assert that a particular ISO sample-entry/configuration permits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LengthSize(u8);

impl LengthSize {
    pub fn new(bytes: u8) -> Result<Self> {
        if !(1..=4).contains(&bytes) {
            return Err(Error::Invalid("NAL length width must be 1 through 4"));
        }
        Ok(Self(bytes))
    }

    pub const fn bytes(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    LengthPrefixed(LengthSize),
    AnnexB,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderLimits {
    pub maximum_nal_bytes: usize,
    pub maximum_padding_bytes: usize,
    pub maximum_nal_units: usize,
    pub maximum_input_window: usize,
}

impl ReaderLimits {
    pub(crate) fn validate(self) -> Result<()> {
        if self.maximum_nal_bytes == 0
            || self.maximum_nal_units == 0
            || self.maximum_input_window == 0
        {
            return Err(Error::Invalid("zero NAL reader policy"));
        }
        self.maximum_padding_bytes
            .checked_add(3)
            .ok_or(Error::Overflow)?;
        Ok(())
    }
}

/// Exact observed source framing, before any output adaptation. Annex-B
/// `prefix_offset` includes the optional zero_byte, but excludes leading padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalFraming {
    LengthPrefixed {
        prefix_offset: u64,
        length_size: LengthSize,
        declared_length: u32,
    },
    AnnexB {
        prefix_offset: u64,
        start_code_bytes: u8,
        leading_zero_bytes: usize,
    },
}

impl NalFraming {
    fn payload_offset(self) -> Result<u64> {
        let (offset, width) = match self {
            Self::LengthPrefixed {
                prefix_offset,
                length_size,
                ..
            } => (prefix_offset, length_size.bytes()),
            Self::AnnexB {
                prefix_offset,
                start_code_bytes,
                ..
            } => (prefix_offset, start_code_bytes),
        };
        offset.checked_add(u64::from(width)).ok_or(Error::Overflow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalFacts {
    pub source: SourceId,
    pub framing: NalFraming,
    pub payload_offset: u64,
    pub payload_length: usize,
    pub trailing_zero_bytes: usize,
    pub header: NalHeader,
}

#[derive(Clone, Debug)]
pub struct NalUnit {
    bytes: Bytes,
    facts: NalFacts,
}

impl NalUnit {
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub const fn facts(&self) -> NalFacts {
        self.facts
    }

    /// The config parser fed this exact immutable source starting at offset
    /// zero. Rebind the validated NAL to that source span so a configuration
    /// does not retain one maximum-sized scratch allocation per parameter set.
    pub(crate) fn configuration_view(mut self, pool: &Budget, source: &Bytes) -> Result<Self> {
        if !self.bytes.belongs_to(pool) || !source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let start = usize::try_from(self.facts.payload_offset).map_err(|_| Error::Overflow)?;
        let end = start
            .checked_add(self.facts.payload_length)
            .ok_or(Error::Overflow)?;
        let bytes = source.slice(start..end)?;
        if NalHeader::parse(self.facts.header.codec(), bytes.as_ref())? != self.facts.header {
            return Err(Error::Invalid("configuration source/header mismatch"));
        }
        self.bytes = bytes;
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Length,
    AnnexPrefix,
    Reserve,
    LengthPayload,
    AnnexPayload,
    Append,
    Finish,
    End,
}

/// A complete-NAL reader over one explicitly completed source entity. No NAL
/// is emitted until its boundary, size and fixed header have been checked.
///
/// Length-prefixed NALs reserve their declared length once. Annex-B NALs reserve
/// `maximum_nal_bytes` once because their length is not known in advance. This
/// deliberate tradeoff prevents hidden vector-growth copies. Output clones retain that capacity.
pub struct NalReader {
    cursor: InputCursor,
    codec: Codec,
    framing: Framing,
    limits: ReaderLimits,
    compact_annex_b: bool,
    phase: Phase,
    buffer: Option<BudgetVec<u8>>,
    current: Option<NalFraming>,
    next: Option<NalFraming>,
    prefix_offset: u64,
    length_used: u8,
    length_value: u32,
    declared_length: usize,
    zeroes: usize,
    pending_byte: Option<u8>,
    append_finishes: bool,
    trailing_zero_bytes: usize,
    end_after_nal: bool,
    completed: usize,
    failure: Option<Error>,
}

impl NalReader {
    pub fn new(
        pool: &Budget,
        codec: Codec,
        framing: Framing,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: ReaderLimits,
    ) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            cursor: InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?,
            codec,
            framing,
            limits,
            compact_annex_b: false,
            phase: match framing {
                Framing::LengthPrefixed(_) => Phase::Length,
                Framing::AnnexB => Phase::AnnexPrefix,
            },
            buffer: None,
            current: None,
            next: None,
            prefix_offset: 0,
            length_used: 0,
            length_value: 0,
            declared_length: 0,
            zeroes: 0,
            pending_byte: None,
            append_finishes: false,
            trailing_zero_bytes: 0,
            end_after_nal: false,
            completed: 0,
            failure: None,
        })
    }

    pub(crate) fn compact_annex_b(
        pool: &Budget,
        codec: Codec,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: ReaderLimits,
    ) -> Result<Self> {
        let mut reader = Self::new(pool, codec, Framing::AnnexB, source, capabilities, limits)?;
        reader.compact_annex_b = true;
        Ok(reader)
    }

    pub fn request(&self) -> InputRequest {
        self.cursor.request()
    }
    pub fn token(&self) -> InputToken {
        self.cursor.token()
    }
    pub fn position(&self) -> u64 {
        self.cursor.position()
    }
    /// True once the source ended after the last NAL unit read: that unit is
    /// the final one.
    pub(crate) const fn ended(&self) -> bool {
        self.end_after_nal
    }

    pub fn source(&self) -> SourceId {
        self.cursor.source()
    }
    pub(crate) fn input_wait_buffer_capacity(&self) -> Result<usize> {
        if self.cursor.buffered_owner().is_some() {
            return Err(Error::Invalid("NAL reader has buffered input"));
        }
        Ok(self.buffer.as_ref().map_or(0, BudgetVec::capacity))
    }

    pub fn budget(&self) -> &Budget {
        self.cursor.budget()
    }

    /// Rejected caller responses do not change parser state. A short or empty
    /// window is starvation; only `finish` establishes source completion.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide(window)
    }

    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide_owned(window)
    }

    pub(crate) fn append_annex_payload(
        &mut self,
        request: &InputRequest,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.compact_annex_b
            || self.phase != Phase::AnnexPayload
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
        let buffer = self
            .buffer
            .as_mut()
            .ok_or(Error::Invalid("missing NAL builder"))?;
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

    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.cursor.finish(source, token);
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
        self.buffer = None;
        self.pending_byte = None;
        self.next = None;
    }

    /// Work units charge each input-byte visit (including its constant-size
    /// classification/copy), each deferred Annex-B payload-byte append, and
    /// each reserve/freeze attempt. No growth copies occur after reserve.
    /// Backpressure/allocation errors are retryable with the same parser;
    /// all content, transport and cancellation failures are sticky.
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<NalUnit>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.stop(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.step_inner(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.stop(error);
        }
        result
    }

    fn buffer(&mut self) -> Result<&mut BudgetVec<u8>> {
        self.buffer
            .as_mut()
            .ok_or(Error::Invalid("missing NAL builder"))
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
        let count = self.buffer()?.len().checked_add(1).ok_or(Error::Overflow)?;
        if count > self.limits.maximum_nal_bytes {
            return Err(Error::Limit {
                requested: count,
                maximum: self.limits.maximum_nal_bytes,
            });
        }
        self.reserve_compact_payload(1)?;
        self.buffer()?
            .try_push(byte)
            .map_err(|rejected| rejected.error)
    }

    fn reserve_compact_payload(&mut self, count: usize) -> Result<()> {
        if !self.compact_annex_b {
            return Ok(());
        }
        let maximum = self.limits.maximum_nal_bytes;
        reserve_nal_bytes(self.buffer()?, maximum, count)
    }

    fn observed_prefix(&self, leading_zero_bytes: usize) -> Result<NalFraming> {
        let width = if self.zeroes >= 3 { 4_u8 } else { 3_u8 };
        let offset = self
            .cursor
            .position()
            .checked_sub(u64::from(width))
            .ok_or(Error::Overflow)?;
        Ok(NalFraming::AnnexB {
            prefix_offset: offset,
            start_code_bytes: width,
            leading_zero_bytes,
        })
    }

    fn step_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<NalUnit>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Length => {
                    let width = match self.framing {
                        Framing::LengthPrefixed(width) => width.bytes(),
                        Framing::AnnexB => return Err(Error::Invalid("NAL reader phase")),
                    };
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
                            if self.length_used == width {
                                if self.length_value == 0 {
                                    return Err(Error::Invalid("zero-length NAL"));
                                }
                                self.declared_length = usize::try_from(self.length_value)
                                    .map_err(|_| Error::Overflow)?;
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
                                self.current = Some(NalFraming::LengthPrefixed {
                                    prefix_offset: self.prefix_offset,
                                    length_size: LengthSize::new(width)?,
                                    declared_length: self.length_value,
                                });
                                self.phase = Phase::Reserve;
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
                Phase::AnnexPrefix => {
                    let mut byte = [0];
                    match self.cursor.read_into(&mut byte, work, cancel)? {
                        Step::Ready(_) if byte[0] == 0 => self.add_zero()?,
                        Step::Ready(_) if byte[0] == 1 && self.zeroes >= 2 => {
                            let leading = self.zeroes - if self.zeroes >= 3 { 3 } else { 2 };
                            self.check_padding(leading)?;
                            self.current = Some(self.observed_prefix(leading)?);
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
                    let capacity = match self.framing {
                        Framing::LengthPrefixed(_) => self.declared_length,
                        Framing::AnnexB if self.compact_annex_b => 0,
                        Framing::AnnexB => self.limits.maximum_nal_bytes,
                    };
                    if self.buffer.is_none() {
                        self.buffer = Some(BudgetVec::new(self.cursor.budget()));
                    }
                    self.buffer()?.try_reserve_exact(capacity)?;
                    self.phase = match self.framing {
                        Framing::LengthPrefixed(_) => Phase::LengthPayload,
                        Framing::AnnexB => Phase::AnnexPayload,
                    };
                }
                Phase::LengthPayload => {
                    let remaining = self
                        .declared_length
                        .checked_sub(self.buffer()?.len())
                        .ok_or(Error::Overflow)?;
                    if remaining == 0 {
                        self.phase = Phase::Finish;
                        continue;
                    }
                    // Recheck cancellation between bounded copies even if the
                    // caller grants a very large step budget/window.
                    match self.cursor.read_span(remaining.min(4096), work, cancel)? {
                        Step::Ready(bytes) => {
                            self.buffer()?.try_extend_from_slice(bytes.as_ref())?;
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                    }
                }
                Phase::AnnexPayload => {
                    if self.compact_annex_b && self.zeroes == 0 {
                        let window = self.cursor.buffered();
                        let maximum = window.len().min(work.remaining().min(4096) as usize);
                        let count = nonzero_prefix_len(&window[..maximum]);
                        if count != 0 {
                            self.reserve_compact_payload(count)?;
                            let buffer = self
                                .buffer
                                .as_mut()
                                .ok_or(Error::Invalid("missing NAL builder"))?;
                            match self.cursor.read_append(buffer, count, work, cancel)? {
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
                            self.next = Some(self.observed_prefix(0)?);
                            self.zeroes = 0;
                            if self.buffer()?.is_empty() {
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
                            Phase::AnnexPayload
                        };
                    }
                }
                Phase::Finish => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let codec = self.codec;
                    let header = NalHeader::parse(codec, self.buffer()?.as_slice())?;
                    let framing = self.current.ok_or(Error::Invalid("missing NAL framing"))?;
                    let payload_offset = framing.payload_offset()?;
                    let buffer = self
                        .buffer
                        .take()
                        .ok_or(Error::Invalid("missing NAL builder"))?;
                    let bytes = match buffer.freeze() {
                        Ok(bytes) => bytes,
                        Err(rejected) => {
                            self.buffer = Some(rejected.buffer);
                            return Err(rejected.error);
                        }
                    };
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
                    self.current = self.next.take();
                    self.phase = match self.framing {
                        Framing::LengthPrefixed(_) => {
                            self.length_used = 0;
                            self.length_value = 0;
                            Phase::Length
                        }
                        Framing::AnnexB if self.end_after_nal => Phase::End,
                        Framing::AnnexB => Phase::Reserve,
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
