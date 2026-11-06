//! Validates externally declared record ranges, not ISO avcC/hvcC grammar.
//!
//! Length fields are explicitly supplied generic big-endian integers. Neither
//! field offsets nor array membership/completeness are inferred from the wire.
//! The full record (including unknown/reserved bytes) remains authoritative.
use crate::{BuildStep, Codec, ConfigInspection, LengthSize, NalHeader};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceId, WorkBudget};

/// An externally supplied integer range followed immediately by a NAL range.
/// Widths 1..=4 describe generic big-endian fields, not ISO width eligibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredConfigNal {
    pub length_field: Range<usize>,
    pub payload: Range<usize>,
}

/// Array membership and metadata are declarations. Only byte ranges, lengths
/// and the fixed NAL headers are verified against the source bytes.
#[derive(Clone, Debug)]
pub struct DeclaredConfigArray<'a> {
    pub raw_range: Range<usize>,
    pub nal_unit_type: u8,
    pub completeness: Option<bool>,
    pub nals: &'a [DeclaredConfigNal],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeclaredConfigLimits {
    pub maximum_record_bytes: usize,
    pub maximum_arrays: usize,
    pub maximum_nals: usize,
    pub maximum_nal_bytes: usize,
}

/// Zero-copy source span plus the independently checked fixed NAL header.
#[derive(Clone, Debug)]
pub struct DeclaredConfigNalView {
    bytes: Bytes,
    length_bytes: Bytes,
    range: Range<usize>,
    header: NalHeader,
    mismatch: bool,
}

impl DeclaredConfigNalView {
    pub fn source_bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn length_field_bytes(&self) -> &Bytes {
        &self.length_bytes
    }
    pub fn source_range(&self) -> Range<usize> {
        self.range.clone()
    }
    pub const fn header(&self) -> NalHeader {
        self.header
    }
    /// Reports a declaration disagreement without changing or filtering bytes.
    pub const fn array_type_mismatch(&self) -> bool {
        self.mismatch
    }
}

#[derive(Clone, Debug)]
pub struct DeclaredConfigArrayView {
    bytes: Bytes,
    range: Range<usize>,
    nal_indices: Range<usize>,
    declared_type: u8,
    declared_completeness: Option<bool>,
}

impl DeclaredConfigArrayView {
    pub fn source_bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn source_range(&self) -> Range<usize> {
        self.range.clone()
    }
    pub fn nal_indices(&self) -> Range<usize> {
        self.nal_indices.clone()
    }
    pub const fn declared_nal_unit_type(&self) -> u8 {
        self.declared_type
    }
    pub const fn declared_completeness(&self) -> Option<bool> {
        self.declared_completeness
    }
}

/// Owns all output descriptors and source storage; no declaration borrow escapes.
/// SourceId and sample width are caller declarations, not inferred wire facts.
pub struct DeclaredConfiguration {
    source: SourceId,
    codec: Codec,
    raw: Bytes,
    sample_width: LengthSize,
    arrays: BudgetVec<DeclaredConfigArrayView>,
    nals: BudgetVec<DeclaredConfigNalView>,
}

impl DeclaredConfiguration {
    pub const fn source_id(&self) -> SourceId {
        self.source
    }
    pub const fn codec(&self) -> Codec {
        self.codec
    }
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub const fn sample_length_size(&self) -> LengthSize {
        self.sample_width
    }
    pub const fn inspection(&self) -> ConfigInspection {
        ConfigInspection::DeclaredOpaque
    }
    pub fn arrays(&self) -> &[DeclaredConfigArrayView] {
        self.arrays.as_slice()
    }
    pub fn nals(&self) -> &[DeclaredConfigNalView] {
        self.nals.as_slice()
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Count,
    ReserveArrays,
    ReserveNals,
    StartArray,
    StartNal,
    Length,
    PublishNal,
    PublishArray,
    Finish,
    Complete,
}

/// A complete owned record plus immutable external layout. Each count, range
/// validation, length byte and descriptor publication consumes bounded work.
/// Descriptor capacity is reserved while empty, never by copying a growing
/// prefix inside one work unit. Backpressure/allocation errors are retryable.
/// This parser does not expose incremental external input/generation or certify
/// record versions, reserved-bit values, ISO grammar or SPS/RBSP semantics.
pub struct DeclaredConfigParser<'a> {
    pool: Budget,
    source: SourceId,
    codec: Codec,
    raw: Option<Bytes>,
    sample_width: LengthSize,
    declarations: &'a [DeclaredConfigArray<'a>],
    limits: DeclaredConfigLimits,
    arrays: BudgetVec<DeclaredConfigArrayView>,
    nals: BudgetVec<DeclaredConfigNalView>,
    phase: Phase,
    count_index: usize,
    total_nals: usize,
    previous_array_end: usize,
    array_index: usize,
    nal_index: usize,
    array_nal_start: usize,
    previous_nal_end: usize,
    field_position: usize,
    field_value: u32,
    failure: Option<Error>,
}

impl<'a> DeclaredConfigParser<'a> {
    pub fn new(
        pool: &Budget,
        codec: Codec,
        source: SourceId,
        raw: Bytes,
        sample_width: LengthSize,
        arrays: &'a [DeclaredConfigArray<'a>],
        limits: DeclaredConfigLimits,
    ) -> Result<Self> {
        if !raw.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if raw.is_empty() || limits.maximum_record_bytes == 0 || limits.maximum_nal_bytes == 0 {
            return Err(Error::Invalid("empty declared configuration policy/source"));
        }
        check_limit(raw.len(), limits.maximum_record_bytes)?;
        check_limit(arrays.len(), limits.maximum_arrays)?;
        Ok(Self {
            pool: pool.clone(),
            source,
            codec,
            raw: Some(raw),
            sample_width,
            declarations: arrays,
            limits,
            arrays: BudgetVec::new(pool),
            nals: BudgetVec::new(pool),
            phase: Phase::Count,
            count_index: 0,
            total_nals: 0,
            previous_array_end: 0,
            array_index: 0,
            nal_index: 0,
            array_nal_start: 0,
            previous_nal_end: 0,
            field_position: 0,
            field_value: 0,
            failure: None,
        })
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<DeclaredConfiguration>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.stop(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.stop(error);
        }
        result
    }

    fn stop(&mut self, error: Error) {
        self.failure = Some(error);
        self.raw = None;
        self.arrays = BudgetVec::new(&self.pool);
        self.nals = BudgetVec::new(&self.pool);
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<DeclaredConfiguration>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.phase, Phase::Complete) {
                return Ok(BuildStep::Complete);
            }
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            match self.phase {
                Phase::Count => {
                    if self.count_index == self.declarations.len() {
                        self.phase = Phase::ReserveArrays;
                        continue;
                    }
                    let array = &self.declarations[self.count_index];
                    let raw = self.raw.as_ref().ok_or(Error::Invalid("missing record"))?;
                    check_range(&array.raw_range, self.previous_array_end, raw.len())?;
                    let type_maximum = match self.codec {
                        Codec::Avc => 31,
                        Codec::Hevc => 63,
                    };
                    if array.nal_unit_type > type_maximum {
                        return Err(Error::Invalid("declared NAL array type"));
                    }
                    self.total_nals = self
                        .total_nals
                        .checked_add(array.nals.len())
                        .ok_or(Error::Overflow)?;
                    check_limit(self.total_nals, self.limits.maximum_nals)?;
                    self.previous_array_end = array.raw_range.end;
                    self.count_index += 1;
                }
                Phase::ReserveArrays => {
                    self.arrays.try_reserve_exact(self.declarations.len())?;
                    self.phase = Phase::ReserveNals;
                }
                Phase::ReserveNals => {
                    self.nals.try_reserve_exact(self.total_nals)?;
                    self.phase = Phase::StartArray;
                }
                Phase::StartArray => {
                    if self.array_index == self.declarations.len() {
                        self.phase = Phase::Finish;
                    } else {
                        self.array_nal_start = self.nals.len();
                        self.nal_index = 0;
                        self.previous_nal_end = self.declarations[self.array_index].raw_range.start;
                        self.phase = Phase::StartNal;
                    }
                }
                Phase::StartNal => {
                    let array = &self.declarations[self.array_index];
                    if self.nal_index == array.nals.len() {
                        self.phase = Phase::PublishArray;
                        continue;
                    }
                    let nal = &array.nals[self.nal_index];
                    check_range(
                        &nal.length_field,
                        self.previous_nal_end,
                        array.raw_range.end,
                    )?;
                    check_range(&nal.payload, nal.length_field.end, array.raw_range.end)?;
                    let width = nal.length_field.len();
                    if !(1..=4).contains(&width) || nal.payload.start != nal.length_field.end {
                        return Err(Error::Invalid("declared NAL length field range"));
                    }
                    if nal.payload.len() < self.codec.minimum_header_bytes() {
                        return Err(Error::Truncated);
                    }
                    check_limit(nal.payload.len(), self.limits.maximum_nal_bytes)?;
                    self.field_position = nal.length_field.start;
                    self.field_value = 0;
                    self.phase = Phase::Length;
                }
                Phase::Length => {
                    let nal = &self.declarations[self.array_index].nals[self.nal_index];
                    if self.field_position == nal.length_field.end {
                        if usize::try_from(self.field_value).map_err(|_| Error::Overflow)?
                            != nal.payload.len()
                        {
                            return Err(Error::Invalid("declared NAL length mismatch"));
                        }
                        self.phase = Phase::PublishNal;
                    } else {
                        let raw = self.raw.as_ref().ok_or(Error::Invalid("missing record"))?;
                        self.field_value =
                            (self.field_value << 8) | u32::from(raw.as_ref()[self.field_position]);
                        self.field_position += 1;
                    }
                }
                Phase::PublishNal => {
                    let array = &self.declarations[self.array_index];
                    let nal = &array.nals[self.nal_index];
                    let raw = self.raw.as_ref().ok_or(Error::Invalid("missing record"))?;
                    let bytes = raw.slice(nal.payload.clone())?;
                    let header = NalHeader::parse(self.codec, bytes.as_ref())?;
                    let view = DeclaredConfigNalView {
                        bytes,
                        length_bytes: raw.slice(nal.length_field.clone())?,
                        range: nal.payload.clone(),
                        header,
                        mismatch: header.nal_unit_type() != array.nal_unit_type,
                    };
                    self.nals.try_push(view).map_err(|error| error.error)?;
                    self.previous_nal_end = nal.payload.end;
                    self.nal_index += 1;
                    self.phase = Phase::StartNal;
                }
                Phase::PublishArray => {
                    let array = &self.declarations[self.array_index];
                    let raw = self.raw.as_ref().ok_or(Error::Invalid("missing record"))?;
                    let view = DeclaredConfigArrayView {
                        bytes: raw.slice(array.raw_range.clone())?,
                        range: array.raw_range.clone(),
                        nal_indices: self.array_nal_start..self.nals.len(),
                        declared_type: array.nal_unit_type,
                        declared_completeness: array.completeness,
                    };
                    self.arrays.try_push(view).map_err(|error| error.error)?;
                    self.array_index += 1;
                    self.phase = Phase::StartArray;
                }
                Phase::Finish => {
                    let raw = self.raw.take().ok_or(Error::Invalid("missing record"))?;
                    let output = DeclaredConfiguration {
                        source: self.source,
                        codec: self.codec,
                        raw,
                        sample_width: self.sample_width,
                        arrays: std::mem::replace(&mut self.arrays, BudgetVec::new(&self.pool)),
                        nals: std::mem::replace(&mut self.nals, BudgetVec::new(&self.pool)),
                    };
                    self.phase = Phase::Complete;
                    return Ok(BuildStep::Ready(output));
                }
                Phase::Complete => return Ok(BuildStep::Complete),
            }
        }
    }
}

fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}

fn check_range(range: &Range<usize>, minimum: usize, maximum: usize) -> Result<()> {
    if range.start < minimum || range.start > range.end || range.end > maximum {
        Err(Error::Invalid("declared configuration range"))
    } else {
        Ok(())
    }
}
