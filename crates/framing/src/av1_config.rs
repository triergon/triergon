//! AV1CodecConfigurationRecord v1 facts from AOM's approved av1-isobmff v1.3.0.
//! This parses record/framing syntax, not Sequence Header OBU payloads or their
//! consistency with the fixed record fields and associated samples.
use crate::{Av1Obu, Av1ObuFraming, Av1ObuLimits, Av1ObuReader, BuildStep, record_input};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, Result, SourceId, Step, WorkBudget,
};

/// Bits: 1 marker missing, 2 nonzero upper reserved bits, 4 nonzero absent-delay
/// reserved bits. Empty issues do not certify Sequence Header/sample semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ConfigSyntaxIssues(u8);

impl Av1ConfigSyntaxIssues {
    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

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

    pub const fn raw_bytes(self) -> [u8; 4] {
        self.raw
    }
    pub const fn marker(self) -> bool {
        self.raw[0] & 128 != 0
    }
    pub const fn version(self) -> u8 {
        self.raw[0] & 127
    }
    pub const fn seq_profile(self) -> u8 {
        self.raw[1] >> 5
    }
    pub const fn seq_level_idx_0(self) -> u8 {
        self.raw[1] & 31
    }
    pub const fn seq_tier_0(self) -> bool {
        self.raw[2] & 128 != 0
    }
    pub const fn high_bitdepth(self) -> bool {
        self.raw[2] & 64 != 0
    }
    pub const fn twelve_bit(self) -> bool {
        self.raw[2] & 32 != 0
    }
    pub const fn monochrome(self) -> bool {
        self.raw[2] & 16 != 0
    }
    pub const fn chroma_subsampling_x(self) -> bool {
        self.raw[2] & 8 != 0
    }
    pub const fn chroma_subsampling_y(self) -> bool {
        self.raw[2] & 4 != 0
    }
    pub const fn chroma_sample_position(self) -> u8 {
        self.raw[2] & 3
    }
    pub const fn reserved_three_bits(self) -> u8 {
        self.raw[3] >> 5
    }
    pub const fn initial_presentation_delay_present(self) -> bool {
        self.raw[3] & 16 != 0
    }
    pub const fn delay_or_reserved_bits(self) -> u8 {
        self.raw[3] & 15
    }
    pub const fn initial_presentation_delay_minus_one(self) -> Option<u8> {
        if self.initial_presentation_delay_present() {
            Some(self.delay_or_reserved_bits())
        } else {
            None
        }
    }
    pub const fn absent_delay_reserved_bits(self) -> Option<u8> {
        if self.initial_presentation_delay_present() {
            None
        } else {
            Some(self.delay_or_reserved_bits())
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1ConfigLimits {
    pub maximum_record_bytes: usize,
    /// Count and wire-size limits apply to configOBUs as well as scratch work.
    pub obu: Av1ObuLimits,
}

pub struct Av1Configuration {
    raw: Bytes,
    fields: Av1ConfigFields,
    obus: BudgetVec<Av1Obu>,
    source: SourceId,
    generation: u64,
}

impl Av1Configuration {
    pub fn raw_record(&self) -> &Bytes {
        &self.raw
    }
    pub const fn fields(&self) -> Av1ConfigFields {
        self.fields
    }
    pub fn config_obus(&self) -> &[Av1Obu] {
        self.obus.as_slice()
    }
    pub const fn source(&self) -> SourceId {
        self.source
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub fn config_obu_bytes(&self) -> Result<Bytes> {
        self.raw.slice(4..self.raw.len())
    }
}

/// Accepts the codec record only, without the surrounding av1C box size/type.
/// Empty configOBUs are allowed. The at-most-one/first Sequence Header OBU rule
/// is checked; its payload/record-field agreement and sample requirements are
/// deliberately unverified. Source issues and unknown OBU types are preserved.
pub struct Av1ConfigParser {
    pool: Budget,
    source: Option<Bytes>,
    identity: SourceId,
    generation: u64,
    header_cursor: Option<InputCursor>,
    reader: Option<Av1ObuReader>,
    header: [u8; 4],
    header_used: usize,
    fields: Option<Av1ConfigFields>,
    limits: Av1ConfigLimits,
    obus: BudgetVec<Av1Obu>,
    pending: Option<Av1Obu>,
    reserved: bool,
    complete: bool,
    failure: Option<Error>,
}

impl Av1ConfigParser {
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
        Ok(Self {
            pool: pool.clone(),
            source: Some(record),
            identity: source,
            generation,
            header_cursor: Some(cursor),
            reader: None,
            header: [0; 4],
            header_used: 0,
            fields: None,
            limits,
            obus: BudgetVec::new(pool),
            pending: None,
            reserved: false,
            complete: false,
            failure: None,
        })
    }

    fn stop(&mut self, error: Error) {
        self.failure = Some(error);
        self.pending = None;
        self.reader = None;
        self.header_cursor = None;
        self.source = None;
        self.obus = BudgetVec::new(&self.pool);
    }

    /// Charges fixed-header bytes, OBU work, one reserve, one descriptor insert
    /// per OBU, and one final record handoff. Backpressure retains the exact
    /// pending builder/descriptor. No source or payload reparse on retry.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Av1Configuration>> {
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

    fn step_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Av1Configuration>> {
        if self.complete {
            return Ok(BuildStep::Complete);
        }
        if !self.reserved {
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            self.obus.try_reserve_exact(self.limits.obu.maximum_obus)?;
            self.reserved = true;
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.header_used < 4 {
                let cursor = self
                    .header_cursor
                    .as_mut()
                    .ok_or(Error::Invalid("missing av1C header cursor"))?;
                match cursor.read_into(&mut self.header[self.header_used..], work, cancel)? {
                    Step::Ready(count) => {
                        self.header_used =
                            self.header_used.checked_add(count).ok_or(Error::Overflow)?
                    }
                    Step::Yield => return Ok(BuildStep::Yield),
                    Step::End(_) => return Err(Error::Truncated),
                    Step::NeedInput(request) => {
                        let source = self
                            .source
                            .as_ref()
                            .ok_or(Error::Invalid("missing av1C source"))?;
                        match record_input::window(source, &request)? {
                            Some(window) => cursor.provide(&window)?,
                            None => cursor.finish(request.source, request.token)?,
                        }
                    }
                }
                continue;
            }
            if self.fields.is_none() {
                if !work.try_charge(1) {
                    return Ok(BuildStep::Yield);
                }
                let fields = Av1ConfigFields::parse(self.header)?;
                let cursor = self
                    .header_cursor
                    .take()
                    .ok_or(Error::Invalid("missing av1C header cursor"))?;
                self.reader = Some(Av1ObuReader::from_cursor(
                    cursor,
                    Av1ObuFraming::LowOverhead,
                    self.limits.obu,
                )?);
                self.fields = Some(fields);
            }
            if self.pending.is_some() {
                if !work.try_charge(1) {
                    return Ok(BuildStep::Yield);
                }
                let obu = self
                    .pending
                    .take()
                    .ok_or(Error::Invalid("missing av1C pending OBU"))?;
                if obu.facts().header.obu_type() == 1 && !self.obus.is_empty() {
                    return Err(Error::Invalid(
                        "av1C Sequence Header OBU must occur only once and first",
                    ));
                }
                if self.obus.len() >= self.limits.obu.maximum_obus {
                    return Err(Error::Limit {
                        requested: self.obus.len().checked_add(1).ok_or(Error::Overflow)?,
                        maximum: self.limits.obu.maximum_obus,
                    });
                }
                let source = self
                    .source
                    .as_ref()
                    .ok_or(Error::Invalid("missing av1C source"))?;
                let obu = obu.configuration_view(&self.pool, source)?;
                if let Err(rejected) = self.obus.try_push(obu) {
                    self.pending = Some(rejected.value);
                    return Err(rejected.error);
                }
            }
            let reader = self
                .reader
                .as_mut()
                .ok_or(Error::Invalid("missing av1C OBU reader"))?;
            match reader.step(work, cancel)? {
                Step::Ready(obu) => self.pending = Some(obu),
                Step::Yield => return Ok(BuildStep::Yield),
                Step::NeedInput(request) => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("missing av1C source"))?;
                    match record_input::window(source, &request)? {
                        Some(window) => reader.provide(&window)?,
                        None => reader.finish(request.source, request.token)?,
                    }
                }
                Step::End(_) => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    let raw = self
                        .source
                        .take()
                        .ok_or(Error::Invalid("missing av1C source"))?;
                    let fields = self.fields.ok_or(Error::Invalid("missing av1C fields"))?;
                    let obus = std::mem::replace(&mut self.obus, BudgetVec::new(&self.pool));
                    self.reader = None;
                    self.complete = true;
                    return Ok(BuildStep::Ready(Av1Configuration {
                        raw,
                        fields,
                        obus,
                        source: self.identity,
                        generation: self.generation,
                    }));
                }
            }
        }
    }
}
