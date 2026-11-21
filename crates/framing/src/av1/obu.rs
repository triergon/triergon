//! AV1 v1.0.0 Errata 1 framing facts, independently written from the
//! specification. OBU payloads remain opaque; no operating-point filtering,
//! trailing-bit removal, frame assembly, decoding or decoder-validity claim.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

/// How the OBUs of a source are delimited.
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

/// Bounds on what an [`Av1ObuReader`] accepts. Each must be non-zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuLimits {
    /// Entire encoded OBU, including header, extension and encoded size field.
    pub maximum_obu_bytes: usize,
    /// The most OBUs one reader frames; the start of one more is refused.
    pub maximum_obus: usize,
    /// The most bytes one answered input window may hold.
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
    /// The bytes as coded, 1 to 8 of them.
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw[..usize::from(self.width)]
    }
    /// Number of coded bytes, 1 to 8.
    pub const fn width(self) -> u8 {
        self.width
    }
    /// The value the bytes decode to.
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

/// The OBU extension header byte (`obu_extension_header`, section 5.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuExtension {
    /// The byte as coded.
    pub raw_byte: u8,
}

impl Av1ObuExtension {
    /// `temporal_id`, 0 to 7.
    pub const fn temporal_id(self) -> u8 {
        self.raw_byte >> 5
    }
    /// `spatial_id`, 0 to 3.
    pub const fn spatial_id(self) -> u8 {
        (self.raw_byte >> 3) & 3
    }
    /// `extension_header_reserved_3bits` as coded; the specification has them
    /// zero.
    pub const fn reserved_bits(self) -> u8 {
        self.raw_byte & 7
    }
}

/// Nonzero must-be-zero header fields are retained and reported, not repaired.
/// Bits: 1 forbidden bit, 2 reserved header bit, 4 reserved extension bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuHeaderIssues(u8);

impl Av1ObuHeaderIssues {
    /// The issue bits listed on the type; 0 when no field is set.
    pub const fn bits(self) -> u8 {
        self.0
    }
    /// True when no must-be-zero field is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// An OBU header (`obu_header`, section 5.3.2): its first byte and, when
/// `obu_extension_flag` is set, the extension byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuHeader {
    /// The first header byte as coded.
    pub raw_byte: u8,
    /// None and an explicitly present all-zero extension remain distinct.
    pub extension: Option<Av1ObuExtension>,
}

impl Av1ObuHeader {
    /// `obu_type`, 0 to 15; reserved values are returned as coded.
    pub const fn obu_type(self) -> u8 {
        (self.raw_byte >> 3) & 15
    }
    /// `obu_has_size_field`: an `obu_size` follows the header.
    pub const fn has_size_field(self) -> bool {
        self.raw_byte & 2 != 0
    }
    /// `obu_extension_flag`: an extension byte follows the first byte.
    pub const fn extension_present(self) -> bool {
        self.raw_byte & 4 != 0
    }
    /// `obu_forbidden_bit` as coded; the specification has it zero.
    pub const fn forbidden_bit(self) -> bool {
        self.raw_byte & 0x80 != 0
    }
    /// `obu_reserved_1bit` as coded; the specification has it zero.
    pub const fn reserved_bit(self) -> bool {
        self.raw_byte & 1 != 0
    }
    /// The must-be-zero fields of the header that are set.
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

/// Where one OBU lies in its source, with its header and size field as coded.
/// Offsets and lengths are in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ObuFacts {
    /// The source the OBU was read from.
    pub source: SourceId,
    /// The generation of the input the OBU was read from
    /// ([`InputToken::generation`]).
    pub generation: u64,
    /// Offset of the OBU's first byte in the source.
    pub offset: u64,
    /// Offset of the payload in the source: after the header, extension and
    /// size field.
    pub payload_offset: u64,
    /// Bytes of payload: `obu_size` when it is coded, otherwise what the
    /// external length leaves after the header.
    pub payload_length: usize,
    /// Bytes of the whole OBU as coded: header, extension, size field and
    /// payload.
    pub wire_length: usize,
    /// The header as coded.
    pub header: Av1ObuHeader,
    /// The `obu_size` field as coded; `None` when `obu_has_size_field` is 0.
    pub size_field: Option<Av1Leb128>,
    /// Total source OBU length supplied by the explicit OneObu envelope.
    pub external_length: Option<u64>,
}

/// One complete OBU: its bytes as coded and where they lie in the source.
#[derive(Clone, Debug)]
pub struct Av1Obu {
    wire: Bytes,
    facts: Av1ObuFacts,
}

impl Av1Obu {
    /// The whole OBU as coded: header, extension, size field and payload.
    pub fn wire_bytes(&self) -> &Bytes {
        &self.wire
    }
    /// Where the OBU lies in its source, with its header and size field.
    pub const fn facts(&self) -> Av1ObuFacts {
        self.facts
    }
    /// The payload, sharing the allocation of [`wire_bytes`](Self::wire_bytes).
    ///
    /// # Errors
    ///
    /// Only when the lengths of [`facts`](Self::facts) do not describe the
    /// bytes, which holds for no OBU a reader or parser of this module returns.
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
#[derive(Debug)]
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
    /// A reader at offset zero of `source`, taking input windows charged to
    /// `pool`.
    ///
    /// # Errors
    ///
    /// When a limit is zero, or when `framing` is [`Av1ObuFraming::OneObu`] and
    /// `capabilities` gives no length or a length of zero.
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

    /// The request for the bytes at [`position`](Self::position).
    pub fn request(&self) -> InputRequest {
        self.cursor.request()
    }
    /// The token an answer to the current request must carry.
    pub fn token(&self) -> InputToken {
        self.cursor.token()
    }
    /// The generation of the reader's input, which the facts of each OBU carry.
    pub fn generation(&self) -> u64 {
        self.cursor.generation()
    }
    /// Offset in the source of the next byte the reader takes.
    pub fn position(&self) -> u64 {
        self.cursor.position()
    }

    /// Answers the current request with `window`.
    ///
    /// # Errors
    ///
    /// The error the reader stopped with, or a window the request does not
    /// admit: another token, source or pool, an offset other than
    /// [`position`](Self::position), or more bytes than
    /// [`Av1ObuLimits::maximum_input_window`]. A refused window leaves the
    /// reader as it was.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide(window)
    }

    /// Answers the current request with the end of the source. An end inside an
    /// OBU makes the next [`step`](Self::step) fail as truncated.
    ///
    /// # Errors
    ///
    /// The error the reader stopped with, a token or source other than the
    /// current one, or a source that ends before its known length, which stops
    /// the reader as truncated.
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

    /// Answers the current request with a failure of the source: the reader
    /// stops, and every later call returns the transport or cancellation error.
    ///
    /// # Errors
    ///
    /// The error the reader already stopped with, or a token or source other
    /// than the current one.
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
    ///
    /// `Ready` is the next complete OBU. `NeedInput` waits for an answer through
    /// [`provide`](Self::provide), [`finish`](Self::finish) or
    /// [`fail`](Self::fail), `Yield` for more work. `End` is the end of the
    /// source at an OBU boundary, after the single OBU of
    /// [`Av1ObuFraming::OneObu`].
    ///
    /// # Errors
    ///
    /// Cancellation; a source that ends inside an OBU; a size field that
    /// continues past eight bytes or exceeds `u32::MAX`; an OBU longer than
    /// `maximum_obu_bytes`, or one more than `maximum_obus`; an OBU without a
    /// size field in low-overhead framing; for a single OBU, a size that
    /// disagrees with the source length, or bytes after the OBU. Each stops the
    /// reader, and every later call returns the same error. A pool that refuses
    /// the OBU's storage returns backpressure or an allocation error, after
    /// which the call may be repeated.
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
                            self.output = Some(rejected.value);
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
