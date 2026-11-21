//! The AV1CodecConfigurationRecord of the AV1 ISOBMFF binding v1.3.0: its fixed
//! fields and the framing of its configuration OBUs. The payload of a Sequence
//! Header OBU is not read, nor checked against the record's fields or samples.
use crate::retryable;
use crate::{
    BuildStep,
    av1::{Av1Obu, Av1ObuFraming, Av1ObuLimits, Av1ObuReader},
    record_input,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceId, Step, WorkBudget};

/// Bits: 1 marker missing, 2 nonzero upper reserved bits, 4 nonzero absent-delay
/// reserved bits. Empty issues do not certify Sequence Header/sample semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ConfigSyntaxIssues(u8);

impl Av1ConfigSyntaxIssues {
    /// The issue bits listed on the type; 0 when the fixed fields have none.
    pub const fn bits(self) -> u8 {
        self.0
    }
    /// True when the marker is set and every reserved bit is zero.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// The four fixed bytes of an `AV1CodecConfigurationRecord` (AV1-ISOBMFF
/// section 2.3.3), each field as coded. The record repeats the values of its
/// Sequence Header OBU; they are not compared with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ConfigFields {
    raw: [u8; 4],
}

impl Av1ConfigFields {
    fn parse(raw: [u8; 4]) -> Result<Self> {
        if raw[0] & 127 != 1 {
            return Err(Error::Unsupported("AV1 configuration record version"));
        }
        Ok(Self { raw })
    }

    /// `marker` as coded; the specification has it 1.
    pub const fn marker(self) -> bool {
        self.raw[0] & 128 != 0
    }
    /// `version`: always 1, the only version the parser accepts.
    pub const fn version(self) -> u8 {
        self.raw[0] & 127
    }
    /// `seq_profile`, 0 to 7.
    pub const fn seq_profile(self) -> u8 {
        self.raw[1] >> 5
    }
    /// `seq_level_idx_0`, 0 to 31: the level of operating point 0.
    pub const fn seq_level_idx_0(self) -> u8 {
        self.raw[1] & 31
    }
    /// `seq_tier_0`: the tier of operating point 0.
    pub const fn seq_tier_0(self) -> bool {
        self.raw[2] & 128 != 0
    }
    /// `high_bitdepth`: more than 8 bits per sample.
    pub const fn high_bitdepth(self) -> bool {
        self.raw[2] & 64 != 0
    }
    /// `twelve_bit`: with `high_bitdepth` in profile 2, 12 bits per sample
    /// instead of 10.
    pub const fn twelve_bit(self) -> bool {
        self.raw[2] & 32 != 0
    }
    /// `monochrome`: the stream has no chroma planes.
    pub const fn monochrome(self) -> bool {
        self.raw[2] & 16 != 0
    }
    /// `chroma_subsampling_x`: chroma has half the horizontal resolution.
    pub const fn chroma_subsampling_x(self) -> bool {
        self.raw[2] & 8 != 0
    }
    /// `chroma_subsampling_y`: chroma has half the vertical resolution.
    pub const fn chroma_subsampling_y(self) -> bool {
        self.raw[2] & 4 != 0
    }
    /// `chroma_sample_position`, 0 to 3.
    pub const fn chroma_sample_position(self) -> u8 {
        self.raw[2] & 3
    }
    /// The three reserved bits of the fourth byte as coded; the specification
    /// has them zero.
    pub const fn reserved_three_bits(self) -> u8 {
        self.raw[3] >> 5
    }
    /// `initial_presentation_delay_present`.
    pub const fn initial_presentation_delay_present(self) -> bool {
        self.raw[3] & 16 != 0
    }
    /// The low four bits of the fourth byte as coded:
    /// `initial_presentation_delay_minus_one` when the delay is present,
    /// reserved bits otherwise.
    pub const fn delay_or_reserved_bits(self) -> u8 {
        self.raw[3] & 15
    }
    /// `initial_presentation_delay_minus_one`, 0 to 15; `None` when
    /// `initial_presentation_delay_present` is 0.
    pub const fn initial_presentation_delay_minus_one(self) -> Option<u8> {
        if self.initial_presentation_delay_present() {
            Some(self.delay_or_reserved_bits())
        } else {
            None
        }
    }
    /// The four reserved bits that take the place of an absent delay, as
    /// coded; `None` when the delay is present.
    pub const fn absent_delay_reserved_bits(self) -> Option<u8> {
        if self.initial_presentation_delay_present() {
            None
        } else {
            Some(self.delay_or_reserved_bits())
        }
    }
    /// The violations among the fixed fields: a missing marker and reserved
    /// bits that are not zero.
    pub fn syntax_issues(self) -> Av1ConfigSyntaxIssues {
        Av1ConfigSyntaxIssues(
            u8::from(!self.marker())
                | (u8::from(self.reserved_three_bits() != 0) << 1)
                | (u8::from(
                    self.absent_delay_reserved_bits()
                        .is_some_and(|value| value != 0),
                ) << 2),
        )
    }
}

/// Bounds on an `av1C` record and the OBUs it carries. Each must be non-zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ConfigLimits {
    /// The most bytes of the whole record.
    pub maximum_record_bytes: usize,
    /// Count and wire-size limits apply to configOBUs as well as scratch work.
    /// The parser reserves room for `maximum_obus` OBU descriptions before it
    /// reads the first, and reads the record in windows of at most
    /// `maximum_input_window` bytes.
    pub obu: Av1ObuLimits,
}

/// A parsed `av1C` record: its fixed fields and its configuration OBUs, which
/// share the record's bytes.
#[derive(Debug)]
pub struct Av1Configuration {
    raw: Bytes,
    fields: Av1ConfigFields,
    obus: BudgetVec<Av1Obu>,
    source: SourceId,
    generation: u64,
}

impl Av1Configuration {
    /// The whole record as it was given to the parser.
    pub fn raw_record(&self) -> &Bytes {
        &self.raw
    }
    /// The four fixed bytes before `configOBUs`.
    pub const fn fields(&self) -> Av1ConfigFields {
        self.fields
    }
    /// The OBUs of `configOBUs` in record order; none when the field is empty.
    /// The offsets in their facts count from the start of the record.
    pub fn config_obus(&self) -> &[Av1Obu] {
        self.obus.as_slice()
    }
    /// The source the record was given to the parser as.
    pub const fn source(&self) -> SourceId {
        self.source
    }
    /// The input generation the record was read under, which the facts of its
    /// OBUs carry too.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Accepts the codec record only, without the surrounding av1C box size/type.
/// Empty configOBUs are allowed. The at-most-one/first Sequence Header OBU rule
/// is checked; its payload/record-field agreement and sample requirements are
/// deliberately unverified. Syntax issues and unknown OBU types are preserved.
#[derive(Debug)]
pub struct Av1ConfigParser {
    pool: Budget,
    identity: SourceId,
    generation: u64,
    limits: Av1ConfigLimits,
    state: State,
}

/// Where a parse stands. The record, its OBU reader and the OBUs read are held only
/// while it is read.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one read per parser; boxing it would allocate outside the pool"
)]
enum State {
    Reading(Read),
    Complete,
    Failed(Error),
}

#[derive(Debug)]
struct Read {
    source: Bytes,
    reader: Av1ObuReader,
    obus: BudgetVec<Av1Obu>,
    stage: Stage,
}

#[derive(Debug)]
enum Stage {
    /// The OBU table, reserved before anything is read.
    Reserve,
    /// The four fixed bytes.
    Header { bytes: [u8; 4], used: usize },
    /// The configOBUs after them, with one read and not yet stored.
    Obus {
        fields: Av1ConfigFields,
        pending: Option<Av1Obu>,
    },
}

impl Av1ConfigParser {
    /// A parser for `record`, the complete payload of an `av1C` box. `source` names the
    /// record in the configuration and in the facts of its OBUs. Fails when a limit is
    /// zero, or the record is charged to another pool than `pool`, is longer than
    /// `maximum_record_bytes` or is shorter than its four fixed bytes.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        record: Bytes,
        limits: Av1ConfigLimits,
    ) -> Result<Self> {
        limits.obu.validate()?;
        record_input::check_record(pool, &record, 4, limits.maximum_record_bytes)?;
        let cursor = record_input::cursor(pool, source, &record, limits.obu.maximum_input_window)?;
        let generation = cursor.generation();
        let reader = Av1ObuReader::from_cursor(cursor, Av1ObuFraming::LowOverhead, limits.obu)?;
        Ok(Self {
            pool: pool.clone(),
            identity: source,
            generation,
            limits,
            state: State::Reading(Read {
                source: record,
                reader,
                obus: BudgetVec::new(pool),
                stage: Stage::Reserve,
            }),
        })
    }

    /// Charges fixed-header bytes, OBU work, one reserve, one descriptor insert
    /// per OBU, and one final record handoff. Backpressure retains the exact
    /// pending builder/descriptor. No source or payload reparse on retry.
    ///
    /// Fails on cancellation; a `version` other than 1; an OBU that
    /// [`Av1ObuReader::step`] refuses in low-overhead framing, such as one without a
    /// size field or one the record ends inside; a Sequence Header OBU after another
    /// OBU; more OBUs than `maximum_obus`. Each is final: every later call returns the
    /// same error. Backpressure and allocation errors leave the parser where it was, to
    /// be stepped again.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Av1Configuration>> {
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.state = State::Failed(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.read(work, cancel);
        if let Err(error) = result
            && !retryable(error)
        {
            self.state = State::Failed(error);
        }
        result
    }

    fn read(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Av1Configuration>> {
        let State::Reading(read) = &mut self.state else {
            return Ok(BuildStep::Complete);
        };
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match &mut read.stage {
                Stage::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    read.obus.try_reserve_exact(self.limits.obu.maximum_obus)?;
                    read.stage = Stage::Header {
                        bytes: [0; 4],
                        used: 0,
                    };
                }
                Stage::Header { bytes, used } if *used < 4 => {
                    match read.reader.read_into(&mut bytes[*used..], work, cancel)? {
                        Step::Ready(count) => {
                            *used = used.checked_add(count).ok_or(Error::Overflow)?;
                        }
                        Step::Yield => return Ok(BuildStep::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                        Step::NeedInput(request) => {
                            match record_input::window(&read.source, &request)? {
                                Some(window) => read.reader.provide(&window)?,
                                None => read.reader.finish(request.source, request.token)?,
                            }
                        }
                    }
                }
                &mut Stage::Header { bytes, .. } => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    read.stage = Stage::Obus {
                        fields: Av1ConfigFields::parse(bytes)?,
                        pending: None,
                    };
                }
                Stage::Obus { fields, pending } => {
                    if pending.is_some() && !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    if let Some(obu) = pending.take() {
                        if obu.facts().header.obu_type() == 1 && !read.obus.is_empty() {
                            return Err(Error::Invalid(
                                "av1C Sequence Header OBU must occur only once and first",
                            ));
                        }
                        if read.obus.len() >= self.limits.obu.maximum_obus {
                            return Err(Error::Limit {
                                requested: read.obus.len().checked_add(1).ok_or(Error::Overflow)?,
                                maximum: self.limits.obu.maximum_obus,
                            });
                        }
                        let obu = obu.configuration_view(&self.pool, &read.source)?;
                        if let Err(rejected) = read.obus.try_push(obu) {
                            *pending = Some(rejected.value);
                            return Err(rejected.error);
                        }
                    }
                    match read.reader.step(work, cancel)? {
                        Step::Ready(obu) => *pending = Some(obu),
                        Step::Yield => return Ok(BuildStep::Yield),
                        Step::NeedInput(request) => {
                            match record_input::window(&read.source, &request)? {
                                Some(window) => read.reader.provide(&window)?,
                                None => read.reader.finish(request.source, request.token)?,
                            }
                        }
                        Step::End(_) => {
                            if !work.try_charge(1) {
                                return Ok(BuildStep::Yield);
                            }
                            let configuration = Av1Configuration {
                                raw: read.source.clone(),
                                fields: *fields,
                                obus: std::mem::replace(&mut read.obus, BudgetVec::new(&self.pool)),
                                source: self.identity,
                                generation: self.generation,
                            };
                            self.state = State::Complete;
                            return Ok(BuildStep::Ready(configuration));
                        }
                    }
                }
            }
        }
    }
}
