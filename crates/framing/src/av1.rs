//! AV1 v1.0.0 Errata 1 framing facts, independently written from the
//! specification. OBU payloads remain opaque; no operating-point filtering,
//! trailing-bit removal, frame assembly, decoding or decoder-validity claim.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Av1ObuFraming {
    /// Section 5.2: every OBU has its own size field. Never infer a missing
    /// size from the end of a packet or transport chunk.
    LowOverhead,
    /// Exactly one externally delimited OBU in a known-length source entity.
    /// Its internal size, when present, must agree with that boundary. This is
    /// not an Annex-B temporal-unit/frame-unit parser.
    OneObu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuLimits {
    /// Entire encoded OBU, including header, extension and encoded size field.
    pub maximum_obu_bytes: usize,
    pub maximum_obus: usize,
    pub maximum_input_window: usize,
}

impl Av1ObuLimits {
    pub(crate) fn validate(self) -> Result<()> {
        if self.maximum_obu_bytes == 0 || self.maximum_obus == 0 || self.maximum_input_window == 0 {
            return Err(Error::Invalid("zero AV1 OBU policy"));
        }
        Ok(())
    }
}

/// Exact LEB128 spelling, including redundant zero groups. AV1 permits up to
/// eight bytes, a terminating eighth byte, and values no greater than u32::MAX.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1Leb128 {
    raw: [u8; 8],
    width: u8,
    value: u32,
}

impl Av1Leb128 {
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw[..usize::from(self.width)]
    }
    pub const fn width(self) -> u8 {
        self.width
    }
    pub const fn value(self) -> u32 {
        self.value
    }
}

pub(crate) fn push_leb_byte(value: &mut u64, width: &mut u8, byte: u8) -> Result<bool> {
    let shift = u32::from(*width) * 7;
    *value |= u64::from(byte & 127)
        .checked_shl(shift)
        .ok_or(Error::Overflow)?;
    *width += 1;
    if byte & 128 == 0 {
        if *value > u64::from(u32::MAX) {
            return Err(Error::Invalid("AV1 LEB128 exceeds 32 bits"));
        }
        return Ok(true);
    }
    if *width == 8 {
        return Err(Error::Invalid("AV1 eighth LEB128 byte continues"));
    }
    Ok(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuExtension {
    pub raw_byte: u8,
}

impl Av1ObuExtension {
    pub const fn temporal_id(self) -> u8 {
        self.raw_byte >> 5
    }
    pub const fn spatial_id(self) -> u8 {
        (self.raw_byte >> 3) & 3
    }
    pub const fn reserved_bits(self) -> u8 {
        self.raw_byte & 7
    }
}

/// Nonzero must-be-zero header fields are retained and reported, not repaired.
/// Bits: 1 forbidden bit, 2 reserved header bit, 4 reserved extension bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuHeaderIssues(u8);

impl Av1ObuHeaderIssues {
    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuHeader {
    pub raw_byte: u8,
    /// None and an explicitly present all-zero extension remain distinct.
    pub extension: Option<Av1ObuExtension>,
}

impl Av1ObuHeader {
    pub const fn obu_type(self) -> u8 {
        (self.raw_byte >> 3) & 15
    }
    pub const fn has_size_field(self) -> bool {
        self.raw_byte & 2 != 0
    }
    pub const fn extension_present(self) -> bool {
        self.raw_byte & 4 != 0
    }
    pub const fn forbidden_bit(self) -> bool {
        self.raw_byte & 0x80 != 0
    }
    pub const fn reserved_bit(self) -> bool {
        self.raw_byte & 1 != 0
    }
    pub fn syntax_issues(self) -> Av1ObuHeaderIssues {
        Av1ObuHeaderIssues(
            u8::from(self.forbidden_bit())
                | (u8::from(self.reserved_bit()) << 1)
                | (u8::from(
                    self.extension
                        .is_some_and(|value| value.reserved_bits() != 0),
                ) << 2),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuFacts {
    pub source: SourceId,
    pub generation: u64,
    pub offset: u64,
    pub payload_offset: u64,
    pub payload_length: usize,
    pub wire_length: usize,
    pub header: Av1ObuHeader,
    pub size_field: Option<Av1Leb128>,
    /// Total source OBU length supplied by the explicit OneObu envelope.
    pub external_length: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Av1Obu {
    wire: Bytes,
    facts: Av1ObuFacts,
}

impl Av1Obu {
    pub fn wire_bytes(&self) -> &Bytes {
        &self.wire
    }
    pub const fn facts(&self) -> Av1ObuFacts {
        self.facts
    }
    pub fn payload_bytes(&self) -> Result<Bytes> {
        let start = self
            .facts
            .wire_length
            .checked_sub(self.facts.payload_length)
            .ok_or(Error::Overflow)?;
        self.wire.slice(start..self.facts.wire_length)
    }

    /// Only the private config parser calls this, after feeding that exact
    /// immutable record into the same cursor. All offsets stay record-relative.
    pub(crate) fn configuration_view(mut self, pool: &Budget, source: &Bytes) -> Result<Self> {
        if !self.wire.belongs_to(pool) || !source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let start = usize::try_from(self.facts.offset).map_err(|_| Error::Overflow)?;
        let end = start
            .checked_add(self.facts.wire_length)
            .ok_or(Error::Overflow)?;
        self.wire = source.slice(start..end)?;
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Header,
    Extension,
    Size,
    Resolve,
    Reserve,
    Prefix,
    Payload,
    Freeze,
    AwaitEnd,
    End,
}

/// Resumable complete-OBU framing. `Ready` certifies a complete bounded wire
/// envelope, not AV1 conformance. Header issues, reserved types, payload trailing
/// bits and metadata bytes remain observable without modification or filtering.
pub struct Av1ObuReader {
    cursor: InputCursor,
    framing: Av1ObuFraming,
    limits: Av1ObuLimits,
    phase: Phase,
    prefix: [u8; 10],
    prefix_len: usize,
    prefix_copied: usize,
    offset: u64,
    leb_value: u64,
    leb_width: u8,
    payload_length: usize,
    wire_length: usize,
    external_length: Option<u64>,
    output: Option<BudgetVec<u8>>,
    completed: usize,
    failure: Option<Error>,
}

impl Av1ObuReader {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        framing: Av1ObuFraming,
        limits: Av1ObuLimits,
    ) -> Result<Self> {
        limits.validate()?;
        Self::from_cursor(
            InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?,
            framing,
            limits,
        )
    }

    pub(crate) fn from_cursor(
        cursor: InputCursor,
        framing: Av1ObuFraming,
        limits: Av1ObuLimits,
    ) -> Result<Self> {
        limits.validate()?;
        if framing == Av1ObuFraming::OneObu {
            let length = cursor.capabilities().length.ok_or(Error::Unsupported(
                "one AV1 OBU needs a known external length",
            ))?;
            if length <= cursor.position() {
                return Err(Error::Invalid("empty external AV1 OBU"));
            }
        }
        Ok(Self {
            cursor,
            framing,
            limits,
            phase: Phase::Header,
            prefix: [0; 10],
            prefix_len: 0,
            prefix_copied: 0,
            offset: 0,
            leb_value: 0,
            leb_width: 0,
            payload_length: 0,
            wire_length: 0,
            external_length: None,
            output: None,
            completed: 0,
            failure: None,
        })
    }

    pub fn request(&self) -> InputRequest {
        self.cursor.request()
    }
    pub fn token(&self) -> InputToken {
        self.cursor.token()
    }
    pub fn generation(&self) -> u64 {
        self.cursor.generation()
    }
    pub fn position(&self) -> u64 {
        self.cursor.position()
    }

    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide(window)
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

    fn stop(&mut self, error: Error) {
        if error == Error::Cancelled {
            let _ = self.cursor.fail(
                self.cursor.source(),
                self.cursor.token(),
                SourceFailure::Cancelled,
            );
        }
        self.failure = Some(error);
        self.output = None;
    }

    /// One unit per input byte with constant classification/copy work, per
    /// prefix byte copied, and per resolve/reserve/freeze. No growth occurs
    /// after reserve; large payload copies are at most 4096 bytes each.
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<Av1Obu>> {
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

    fn header(&self) -> Av1ObuHeader {
        Av1ObuHeader {
            raw_byte: self.prefix[0],
            extension: if self.prefix[0] & 4 != 0 {
                Some(Av1ObuExtension {
                    raw_byte: self.prefix[1],
                })
            } else {
                None
            },
        }
    }

    fn buffer(&mut self) -> Result<&mut BudgetVec<u8>> {
        self.output
            .as_mut()
            .ok_or(Error::Invalid("missing AV1 OBU builder"))
    }

    fn step_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<Av1Obu>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Header => {
                    self.offset = self.cursor.position();
                    let mut byte = [0];
                    match self.cursor.read_into(&mut byte, work, cancel)? {
                        Step::Ready(_) => {
                            if self.completed >= self.limits.maximum_obus {
                                return Err(Error::Limit {
                                    requested: self
                                        .completed
                                        .checked_add(1)
                                        .ok_or(Error::Overflow)?,
                                    maximum: self.limits.maximum_obus,
                                });
                            }
                            self.prefix[0] = byte[0];
                            self.prefix_len = 1;
                            self.prefix_copied = 0;
                            self.leb_width = 0;
                            self.leb_value = 0;
                            self.phase = if byte[0] & 4 != 0 {
                                Phase::Extension
                            } else if byte[0] & 2 != 0 {
                                Phase::Size
                            } else {
                                Phase::Resolve
                            };
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(location) => {
                            self.phase = Phase::End;
                            return Ok(Step::End(location));
                        }
                    }
                }
                Phase::Extension | Phase::Size => {
                    let mut byte = [0];
                    match self.cursor.read_into(&mut byte, work, cancel)? {
                        Step::Ready(_) => {
                            if self.prefix_len >= self.prefix.len() {
                                return Err(Error::Invalid("AV1 prefix too long"));
                            }
                            self.prefix[self.prefix_len] = byte[0];
                            self.prefix_len += 1;
                            if self.phase == Phase::Extension {
                                self.phase = if self.prefix[0] & 2 != 0 {
                                    Phase::Size
                                } else {
                                    Phase::Resolve
                                };
                            } else if push_leb_byte(
                                &mut self.leb_value,
                                &mut self.leb_width,
                                byte[0],
                            )? {
                                self.phase = Phase::Resolve;
                            }
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                    }
                }
                Phase::Resolve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.external_length = match self.framing {
                        Av1ObuFraming::LowOverhead => None,
                        Av1ObuFraming::OneObu => Some(
                            self.cursor
                                .capabilities()
                                .length
                                .ok_or(Error::Unsupported("unknown external OBU length"))?
                                .checked_sub(self.offset)
                                .ok_or(Error::Overflow)?,
                        ),
                    };
                    let prefix = u64::try_from(self.prefix_len).map_err(|_| Error::Overflow)?;
                    let payload = if self.header().has_size_field() {
                        self.leb_value
                    } else {
                        self.external_length
                            .ok_or(Error::Unsupported("size-less OBU in low-overhead stream"))?
                            .checked_sub(prefix)
                            .ok_or(Error::Truncated)?
                    };
                    // The limit is checked on the 64-bit wire length, so a declared size
                    // that does not fit `usize` is a limit refusal on every target.
                    let wire = prefix.checked_add(payload).ok_or(Error::Overflow)?;
                    let maximum = self.limits.maximum_obu_bytes;
                    if wire > u64::try_from(maximum).unwrap_or(u64::MAX) {
                        return Err(Error::Limit {
                            requested: usize::try_from(wire).unwrap_or(usize::MAX),
                            maximum,
                        });
                    }
                    self.payload_length = usize::try_from(payload).map_err(|_| Error::Overflow)?;
                    self.wire_length = usize::try_from(wire).map_err(|_| Error::Overflow)?;
                    if self
                        .external_length
                        .is_some_and(|external| external != wire)
                    {
                        return Err(Error::Invalid(
                            "AV1 internal and external OBU sizes disagree",
                        ));
                    }
                    let end = self.offset.checked_add(wire).ok_or(Error::Overflow)?;
                    if self
                        .cursor
                        .capabilities()
                        .length
                        .is_some_and(|length| end > length)
                    {
                        return Err(Error::Truncated);
                    }
                    self.phase = Phase::Reserve;
                }
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if self.output.is_none() {
                        self.output = Some(BudgetVec::new(self.cursor.budget()));
                    }
                    let count = self.wire_length;
                    self.buffer()?.try_reserve_exact(count)?;
                    self.phase = Phase::Prefix;
                }
                Phase::Prefix => {
                    let remaining = self
                        .prefix_len
                        .checked_sub(self.prefix_copied)
                        .ok_or(Error::Overflow)?;
                    if remaining == 0 {
                        self.phase = Phase::Payload;
                        continue;
                    }
                    let count = usize::try_from(
                        work.take(u64::try_from(remaining).map_err(|_| Error::Overflow)?),
                    )
                    .map_err(|_| Error::Overflow)?;
                    if count == 0 {
                        return Ok(Step::Yield);
                    }
                    let start = self.prefix_copied;
                    let end = start.checked_add(count).ok_or(Error::Overflow)?;
                    let output = self
                        .output
                        .as_mut()
                        .ok_or(Error::Invalid("missing AV1 OBU builder"))?;
                    output.try_extend_from_slice(&self.prefix[start..end])?;
                    self.prefix_copied = end;
                }
                Phase::Payload => {
                    let length = self.buffer()?.len();
                    let remaining = self
                        .wire_length
                        .checked_sub(length)
                        .ok_or(Error::Overflow)?;
                    if remaining == 0 {
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    match self.cursor.read_span(remaining.min(4096), work, cancel)? {
                        Step::Ready(bytes) => {
                            self.buffer()?.try_extend_from_slice(bytes.as_ref())?;
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                    }
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let header = self.header();
                    let size_field = if header.has_size_field() {
                        let mut raw = [0; 8];
                        let width = usize::from(self.leb_width);
                        let start = self.prefix_len.checked_sub(width).ok_or(Error::Overflow)?;
                        raw[..width].copy_from_slice(&self.prefix[start..self.prefix_len]);
                        Some(Av1Leb128 {
                            raw,
                            width: self.leb_width,
                            value: u32::try_from(self.leb_value).map_err(|_| Error::Overflow)?,
                        })
                    } else {
                        None
                    };
                    let payload_offset = self
                        .offset
                        .checked_add(u64::try_from(self.prefix_len).map_err(|_| Error::Overflow)?)
                        .ok_or(Error::Overflow)?;
                    let output = self
                        .output
                        .take()
                        .ok_or(Error::Invalid("missing AV1 OBU builder"))?;
                    let wire = match output.freeze() {
                        Ok(wire) => wire,
                        Err(rejected) => {
                            self.output = Some(rejected.buffer);
                            return Err(rejected.error);
                        }
                    };
                    let facts = Av1ObuFacts {
                        source: self.cursor.source(),
                        generation: self.cursor.generation(),
                        offset: self.offset,
                        payload_offset,
                        payload_length: self.payload_length,
                        wire_length: self.wire_length,
                        header,
                        size_field,
                        external_length: self.external_length,
                    };
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    self.phase = if self.framing == Av1ObuFraming::OneObu {
                        Phase::AwaitEnd
                    } else {
                        Phase::Header
                    };
                    return Ok(Step::Ready(Av1Obu { wire, facts }));
                }
                Phase::AwaitEnd => {
                    let mut byte = [0];
                    match self.cursor.read_into(&mut byte, work, cancel)? {
                        Step::Ready(_) => {
                            return Err(Error::Invalid("extra bytes after one AV1 OBU"));
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(location) => {
                            self.phase = Phase::End;
                            return Ok(Step::End(location));
                        }
                    }
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
