//! AC3SpecificBox / EC3SpecificBox payloads, ETSI TS 102 366 V1.4.1 Annex F.
//! The caller supplies the box kind and exact payload; box headers and later
//! reserved-tail extensions are not guessed. Source fields are not repaired.
use crate::{BuildStep, ac3::frame::BIT_RATES_KBPS, record_input};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, InputCursor, Result, SourceId, Step, WorkBudget};

/// Which record a payload holds. The payload does not say: the caller knows it from
/// the type of the box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ac3ConfigKind {
    /// The payload of an AC3SpecificBox (`dac3`), ETSI TS 102 366 Annex F.4: three
    /// bytes.
    Dac3,
    /// The payload of an EC3SpecificBox (`dec3`), ETSI TS 102 366 Annex F.6.
    Dec3,
}

/// Caller limits of an [`Ac3ConfigParser`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ac3ConfigLimits {
    /// The longest record accepted, in bytes; not zero.
    pub maximum_record_bytes: usize,
    /// The most independent substreams a `dec3` record may declare
    /// (`num_ind_sub + 1`): 1 to 8.
    pub maximum_independent_substreams: u8,
    /// The most bytes of the record the parser takes in at a time; not zero.
    pub maximum_input_window: usize,
}

/// Bits: 1 nonzero fixed reserved fields; 2 reserved fscod; 4 unsupported bsid;
/// 8 reserved dac3 bit-rate code. No full stream/record consistency assertion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ac3ConfigIssues(u8);

impl Ac3ConfigIssues {
    /// The issue bits listed on the type; 0 when there is none.
    pub const fn bits(self) -> u8 {
        self.0
    }
    /// Whether no issue was found.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// The 24 bits of a `dac3` record. Every field reads as coded, reserved values
/// included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dac3Fields {
    raw: [u8; 3],
}

impl Dac3Fields {
    const fn word(self) -> u32 {
        u32::from_be_bytes([0, self.raw[0], self.raw[1], self.raw[2]])
    }
    /// `fscod`, the sample rate code: 0 for 48 kHz, 1 for 44.1 kHz, 2 for 32 kHz,
    /// 3 reserved.
    pub const fn fscod(self) -> u8 {
        (self.word() >> 22) as u8
    }
    /// `bsid`, 5 bits; AC-3 uses 0 to 8.
    pub const fn bsid(self) -> u8 {
        ((self.word() >> 17) & 31) as u8
    }
    /// `bsmod`, the bit stream mode: 3 bits.
    pub const fn bsmod(self) -> u8 {
        ((self.word() >> 14) & 7) as u8
    }
    /// `acmod`, the audio coding mode: 3 bits.
    pub const fn acmod(self) -> u8 {
        ((self.word() >> 11) & 7) as u8
    }
    /// `lfeon`: whether the stream codes a low-frequency effects channel.
    pub const fn lfe_on(self) -> bool {
        self.word() & 1024 != 0
    }
    /// `bit_rate_code`, 5 bits: the index of the nominal bit rate, from 0 for
    /// 32 kbit/s to 18 for 640 kbit/s; 19 to 31 are reserved.
    pub const fn bit_rate_code(self) -> u8 {
        ((self.word() >> 5) & 31) as u8
    }
    /// The five reserved bits that end the record; 0 in a conforming record.
    pub const fn reserved_bits(self) -> u8 {
        (self.word() & 31) as u8
    }
    /// The sample rate that `fscod` names, in Hz. `None` for the reserved code and
    /// for a `bsid` above 8.
    pub fn sample_rate_hz(self) -> Option<u32> {
        if self.bsid() > 8 {
            return None;
        }
        [48000, 44100, 32000]
            .get(usize::from(self.fscod()))
            .copied()
    }
    /// The nominal bit rate that `bit_rate_code` names, in bits per second: 32000
    /// to 640000. `None` for a reserved code and for a `bsid` above 8.
    pub fn nominal_bit_rate_bps(self) -> Option<u32> {
        if self.bsid() > 8 {
            return None;
        }
        BIT_RATES_KBPS
            .get(usize::from(self.bit_rate_code()))
            .map(|value| u32::from(*value) * 1000)
    }
    /// The reserved and unsupported values among the fields, as
    /// [`Ac3ConfigIssues`] bits.
    pub fn syntax_issues(self) -> Ac3ConfigIssues {
        Ac3ConfigIssues(
            u8::from(self.reserved_bits() != 0)
                | (u8::from(self.fscod() == 3) << 1)
                | (u8::from(self.bsid() > 8) << 2)
                | (u8::from(self.bit_rate_code() > 18) << 3),
        )
    }
}

/// A `dac3` record: its bytes as supplied and the fields they code.
#[derive(Clone, Debug)]
pub struct Dac3Configuration {
    raw: Bytes,
    fields: Dac3Fields,
    source: SourceId,
    generation: u64,
}

impl Dac3Configuration {
    /// The record's three bytes, sharing the allocation the parser was given.
    pub fn raw_record(&self) -> &Bytes {
        &self.raw
    }
    /// The record's fields, as coded.
    pub const fn fields(&self) -> Dac3Fields {
        self.fields
    }
    /// The source the parser was created for.
    pub const fn source(&self) -> SourceId {
        self.source
    }
    /// The generation of the parser that read the record: a number its pool gives
    /// to no other parser or reader.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// One independent substream entry of a `dec3` record: three bytes, or four when
/// it declares dependent substreams. Every field reads as coded, reserved values
/// included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dec3Substream {
    raw: [u8; 4],
    width: u8,
    offset: usize,
}

impl Dec3Substream {
    const EMPTY: Self = Self {
        raw: [0; 4],
        width: 3,
        offset: 0,
    };
    /// The entry's bytes: three, or four when `num_dep_sub` is not 0.
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw[..usize::from(self.width)]
    }
    /// `fscod`, the sample rate code: 0 for 48 kHz, 1 for 44.1 kHz, 2 for 32 kHz,
    /// 3 reserved.
    pub const fn fscod(self) -> u8 {
        self.raw[0] >> 6
    }
    /// `bsid`, 5 bits; E-AC-3 uses 11 to 16.
    pub const fn bsid(self) -> u8 {
        (self.raw[0] >> 1) & 31
    }
    /// The reserved bit after `bsid`; 0 in a conforming record.
    pub const fn reserved_one_bit(self) -> u8 {
        self.raw[0] & 1
    }
    /// `asvc`: whether the substream is an associated service.
    pub const fn asvc(self) -> bool {
        self.raw[1] & 128 != 0
    }
    /// `bsmod`, the bit stream mode: 3 bits.
    pub const fn bsmod(self) -> u8 {
        (self.raw[1] >> 4) & 7
    }
    /// `acmod`, the audio coding mode: 3 bits.
    pub const fn acmod(self) -> u8 {
        (self.raw[1] >> 1) & 7
    }
    /// `lfeon`: whether the substream codes a low-frequency effects channel.
    pub const fn lfe_on(self) -> bool {
        self.raw[1] & 1 != 0
    }
    /// The three reserved bits before `num_dep_sub`; 0 in a conforming record.
    pub const fn reserved_three_bits(self) -> u8 {
        self.raw[2] >> 5
    }
    /// Raw four-bit num_dep_sub, not an observed or validated stream count.
    /// ETSI F.6.2.12 defines it from a dependent substream ID.
    pub const fn num_dependent(self) -> u8 {
        (self.raw[2] >> 1) & 15
    }
    /// `chan_loc`, 9 bits: the channel locations of the dependent substreams.
    /// `None` when `num_dep_sub` is 0.
    pub const fn channel_location(self) -> Option<u16> {
        if self.num_dependent() == 0 {
            None
        } else {
            Some(((self.raw[2] & 1) as u16) << 8 | self.raw[3] as u16)
        }
    }
    /// The reserved bit that takes the place of `chan_loc` when `num_dep_sub` is
    /// 0; `None` otherwise.
    pub const fn no_dependent_reserved_bit(self) -> Option<u8> {
        if self.num_dependent() == 0 {
            Some(self.raw[2] & 1)
        } else {
            None
        }
    }
    /// Under this ETSI record profile, fscod == 3 is reserved. The record has no
    /// fscod2 field; a reduced E-AC-3 frame's rate is not inferred here.
    pub fn sample_rate_hz(self) -> Option<u32> {
        if !(11..=16).contains(&self.bsid()) {
            return None;
        }
        [48000, 44100, 32000]
            .get(usize::from(self.fscod()))
            .copied()
    }
    /// The reserved and unsupported values among the fields, as
    /// [`Ac3ConfigIssues`] bits.
    pub fn syntax_issues(self) -> Ac3ConfigIssues {
        Ac3ConfigIssues(
            u8::from(
                self.reserved_one_bit() != 0
                    || self.reserved_three_bits() != 0
                    || self
                        .no_dependent_reserved_bit()
                        .is_some_and(|value| value != 0),
            ) | (u8::from(self.fscod() == 3) << 1)
                | (u8::from(!(11..=16).contains(&self.bsid())) << 2),
        )
    }
}

/// A `dec3` record: its bytes as supplied, `data_rate` and one entry for each
/// independent substream.
#[derive(Clone, Debug)]
pub struct Dec3Configuration {
    raw: Bytes,
    data_rate: u16,
    entries: [Dec3Substream; 8],
    count: u8,
    tail_offset: usize,
    source: SourceId,
    generation: u64,
}

impl Dec3Configuration {
    /// The whole record, sharing the allocation the parser was given.
    pub fn raw_record(&self) -> &Bytes {
        &self.raw
    }
    /// `data_rate`, 13 bits: the data rate of the stream in kbit/s, as coded.
    pub const fn data_rate_kbps(&self) -> u16 {
        self.data_rate
    }
    /// The independent substream entries in record order: `num_ind_sub + 1` of
    /// them, 1 to 8.
    pub fn substreams(&self) -> &[Dec3Substream] {
        &self.entries[..usize::from(self.count)]
    }
    /// The source the parser was created for.
    pub const fn source(&self) -> SourceId {
        self.source
    }
    /// The generation of the parser that read the record: a number its pool gives
    /// to no other parser or reader.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    /// The bytes after the last entry, uninterpreted and sharing the record's
    /// allocation; empty when the record ends with its entries. Never fails: the range
    /// lies inside the record.
    pub fn trailing_reserved_bytes(&self) -> Result<Bytes> {
        self.raw.slice(self.tail_offset..self.raw.len())
    }
    /// The bytes of entry `index` of [`substreams`](Self::substreams), sharing the
    /// record's allocation. Fails when `index` is not below the number of entries.
    pub fn substream_bytes(&self, index: usize) -> Result<Bytes> {
        let entry = self
            .substreams()
            .get(index)
            .ok_or(Error::Invalid("dec3 substream index"))?;
        self.raw.slice(
            entry.offset
                ..entry
                    .offset
                    .checked_add(usize::from(entry.width))
                    .ok_or(Error::Overflow)?,
        )
    }
    /// The issues of all entries together, as [`Ac3ConfigIssues`] bits.
    pub fn syntax_issues(&self) -> Ac3ConfigIssues {
        Ac3ConfigIssues(
            self.substreams()
                .iter()
                .fold(0, |issues, entry| issues | entry.syntax_issues().bits()),
        )
    }
}

/// The record an [`Ac3ConfigParser`] read.
#[derive(Clone, Debug)]
pub enum Ac3Configuration {
    /// A `dac3` record.
    Dac3(Dac3Configuration),
    /// A `dec3` record.
    Dec3(Dec3Configuration),
}

impl Ac3Configuration {
    /// The whole record, sharing the allocation the parser was given.
    pub fn raw_record(&self) -> &Bytes {
        match self {
            Self::Dac3(value) => value.raw_record(),
            Self::Dec3(value) => value.raw_record(),
        }
    }
    /// The source the parser was created for.
    pub fn source(&self) -> SourceId {
        match self {
            Self::Dac3(value) => value.source(),
            Self::Dec3(value) => value.source(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Dac3,
    Dec3Header,
    Descriptor,
    Finish,
}

/// Pool-owned, already-complete box payload inspection. At most eight fixed
/// descriptors are stored inline; returned spans alias the original record.
/// Trailing dec3 reserved bytes are retained without scanning or interpretation.
pub struct Ac3ConfigParser {
    source: Option<Bytes>,
    cursor: Option<InputCursor>,
    identity: SourceId,
    generation: u64,
    limits: Ac3ConfigLimits,
    phase: Phase,
    scratch: [u8; 4],
    used: usize,
    need: usize,
    descriptor_offset: usize,
    entries: [Dec3Substream; 8],
    count: u8,
    completed: usize,
    data_rate: u16,
    complete: bool,
    failure: Option<Error>,
}

impl std::fmt::Debug for Ac3ConfigParser {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Ac3ConfigParser")
            .field("source", &self.identity)
            .field("generation", &self.generation)
            .field("limits", &self.limits)
            .field("complete", &self.complete)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl Ac3ConfigParser {
    /// A parser of `record`, the complete payload of the box that `kind` names.
    /// `source` only labels the result. Fails when `maximum_independent_substreams` is
    /// not 1 to 8 or another limit is zero; when `record` is not charged to `pool`, is
    /// longer than `maximum_record_bytes`, or is shorter than three bytes (`dac3`) or
    /// two (`dec3`).
    pub fn new(
        pool: &Budget,
        source: SourceId,
        record: Bytes,
        kind: Ac3ConfigKind,
        limits: Ac3ConfigLimits,
    ) -> Result<Self> {
        if !(1..=8).contains(&limits.maximum_independent_substreams) {
            return Err(Error::Invalid("audio independent substream policy"));
        }
        record_input::check_record(
            pool,
            &record,
            if kind == Ac3ConfigKind::Dac3 { 3 } else { 2 },
            limits.maximum_record_bytes,
        )?;
        let cursor = record_input::cursor(pool, source, &record, limits.maximum_input_window)?;
        let generation = cursor.generation();
        Ok(Self {
            source: Some(record),
            cursor: Some(cursor),
            identity: source,
            generation,
            limits,
            phase: if kind == Ac3ConfigKind::Dac3 {
                Phase::Dac3
            } else {
                Phase::Dec3Header
            },
            scratch: [0; 4],
            used: 0,
            need: if kind == Ac3ConfigKind::Dac3 { 3 } else { 2 },
            descriptor_offset: 2,
            entries: [Dec3Substream::EMPTY; 8],
            count: 0,
            completed: 0,
            data_rate: 0,
            complete: false,
            failure: None,
        })
    }
    fn stop(&mut self, error: Error) {
        self.failure = Some(error);
        self.source = None;
        self.cursor = None;
    }
    /// Advances by at most the work left in `work`, and returns the record once it is
    /// read. Fails when a `dac3` record is longer than three bytes, a `dec3` record
    /// declares more independent substreams than the limit or ends inside an entry, or
    /// `cancel` is set. Every later call returns the same error.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Ac3Configuration>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.stop(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.step_inner(work, cancel);
        if let Err(error) = result {
            self.stop(error);
        }
        result
    }
    fn step_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Ac3Configuration>> {
        if self.complete {
            return Ok(BuildStep::Complete);
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.phase != Phase::Finish && self.used < self.need {
                let cursor = self
                    .cursor
                    .as_mut()
                    .ok_or(Error::Invalid("missing audio record cursor"))?;
                match cursor.read_into(&mut self.scratch[self.used..self.need], work, cancel)? {
                    Step::Ready(count) => {
                        self.used = self.used.checked_add(count).ok_or(Error::Overflow)?;
                    }
                    Step::Yield => return Ok(BuildStep::Yield),
                    Step::End(_) => return Err(Error::Truncated),
                    Step::NeedInput(request) => {
                        let source = self
                            .source
                            .as_ref()
                            .ok_or(Error::Invalid("missing audio record source"))?;
                        match record_input::window(source, &request)? {
                            Some(window) => cursor.provide(&window)?,
                            None => cursor.finish(request.source, request.token)?,
                        }
                    }
                }
                continue;
            }
            if self.phase == Phase::Descriptor && self.need == 3 && (self.scratch[2] >> 1) & 15 != 0
            {
                self.need = 4;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            match self.phase {
                Phase::Dac3 => {
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("missing dac3 source"))?;
                    if source.len() != 3 {
                        return Err(Error::Invalid("extra dac3 record bytes"));
                    }
                    let fields = Dac3Fields {
                        raw: [self.scratch[0], self.scratch[1], self.scratch[2]],
                    };
                    let raw = self
                        .source
                        .take()
                        .ok_or(Error::Invalid("missing dac3 source"))?;
                    self.cursor = None;
                    self.complete = true;
                    return Ok(BuildStep::Ready(Ac3Configuration::Dac3(
                        Dac3Configuration {
                            raw,
                            fields,
                            source: self.identity,
                            generation: self.generation,
                        },
                    )));
                }
                Phase::Dec3Header => {
                    let word = u16::from_be_bytes([self.scratch[0], self.scratch[1]]);
                    self.data_rate = word >> 3;
                    self.count = ((word & 7) as u8).checked_add(1).ok_or(Error::Overflow)?;
                    if self.count > self.limits.maximum_independent_substreams {
                        return Err(Error::Limit {
                            requested: usize::from(self.count),
                            maximum: usize::from(self.limits.maximum_independent_substreams),
                        });
                    }
                    self.scratch = [0; 4];
                    self.used = 0;
                    self.need = 3;
                    self.phase = Phase::Descriptor;
                }
                Phase::Descriptor => {
                    let cursor = self
                        .cursor
                        .as_ref()
                        .ok_or(Error::Invalid("missing dec3 cursor"))?;
                    self.entries[self.completed] = Dec3Substream {
                        raw: self.scratch,
                        width: u8::try_from(self.need).map_err(|_| Error::Overflow)?,
                        offset: self.descriptor_offset,
                    };
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    self.descriptor_offset =
                        usize::try_from(cursor.position()).map_err(|_| Error::Overflow)?;
                    self.scratch = [0; 4];
                    self.used = 0;
                    self.need = 3;
                    if self.completed == usize::from(self.count) {
                        self.phase = Phase::Finish;
                    }
                }
                Phase::Finish => {
                    let raw = self
                        .source
                        .take()
                        .ok_or(Error::Invalid("missing dec3 source"))?;
                    self.cursor = None;
                    self.complete = true;
                    return Ok(BuildStep::Ready(Ac3Configuration::Dec3(
                        Dec3Configuration {
                            raw,
                            data_rate: self.data_rate,
                            entries: self.entries,
                            count: self.count,
                            tail_offset: self.descriptor_offset,
                            source: self.identity,
                            generation: self.generation,
                        },
                    )));
                }
            }
        }
    }
}
