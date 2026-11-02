//! ETSI TS 126 244 V18.0.0 §13.4: compact source segment-index references.
//! SAP declarations and byte/time candidates do not establish decoder recovery.
use crate::{BoxView, Limits, fields, policy};
use safe_media_core::{Error, Result, Time, TimeBase, WorkBudget};
use std::ops::Range;

#[derive(Clone, Debug)]
pub struct SegmentReference {
    pub ordinal: u32,
    pub references_index: bool,
    pub range: Range<u64>,
    /// Movie composition timeline after edits, not a replacement source DTS.
    pub earliest_presentation: Time,
    pub presentation_duration: Time,
    pub starts_with_sap: bool,
    /// Values 0..6 are documented; reserved 7 stays raw, never grants recovery.
    pub sap_type: u8,
    pub sap_delta_ticks: u32,
    pub raw_size_word: u32,
    pub raw_sap_word: u32,
}

#[derive(Debug)]
pub enum IndexStep {
    Reference(SegmentReference),
    Yield,
    End,
}

/// Keeps the complete source box and a compact fixed-width table. Targets must
/// still be resolved against the correct source and checked as moof/sidx boxes;
/// hierarchical graphs require caller-owned depth/cycle and range budgets.
#[derive(Clone, Debug)]
pub struct SegmentIndex {
    raw: BoxView,
    version: u8,
    reference_id: u32,
    timescale: u32,
    earliest: u64,
    first_offset: u64,
    first_byte: u64,
    records: usize,
    count: u16,
}
impl SegmentIndex {
    pub fn parse(raw: BoxView, limits: Limits) -> Result<Self> {
        if raw.header().kind != *b"sidx" {
            return Err(Error::Invalid("not sidx"));
        }
        let b = raw.payload();
        let (version, flags) = fields::full(b)?;
        if version > 1 || flags != 0 {
            return Err(Error::Unsupported("sidx version/flags"));
        }
        let reference_id = fields::u32_at(b, 4)?;
        let timescale = fields::u32_at(b, 8)?;
        if reference_id == 0 || timescale == 0 {
            return Err(Error::Invalid("zero sidx track/timescale"));
        }
        let (earliest, first_offset, reserved) = if version == 0 {
            (
                u64::from(fields::u32_at(b, 12)?),
                u64::from(fields::u32_at(b, 16)?),
                20,
            )
        } else {
            (fields::u64_at(b, 12)?, fields::u64_at(b, 20)?, 28)
        };
        if fields::u16_at(b, reserved)? != 0 {
            return Err(Error::Invalid("nonzero sidx reserved bits"));
        }
        let count = fields::u16_at(b, reserved + 2)?;
        let records = reserved + 4;
        fields::exact_rows(b, records, u32::from(count), 12)?;
        policy(u32::from(count), limits.max_records)?;
        let after_box = raw
            .header()
            .end()?
            .ok_or(Error::Invalid("unbounded sidx"))?;
        let first_byte = after_box.checked_add(first_offset).ok_or(Error::Overflow)?;
        Ok(Self {
            raw,
            version,
            reference_id,
            timescale,
            earliest,
            first_offset,
            first_byte,
            records,
            count,
        })
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn version(&self) -> u8 {
        self.version
    }
    pub fn reference_id(&self) -> u32 {
        self.reference_id
    }
    pub fn timescale(&self) -> u32 {
        self.timescale
    }
    pub fn earliest_presentation_ticks(&self) -> u64 {
        self.earliest
    }
    pub fn first_offset(&self) -> u64 {
        self.first_offset
    }
    pub fn reference_count(&self) -> u16 {
        self.count
    }
    pub fn cursor(&self) -> SegmentIndexCursor<'_> {
        SegmentIndexCursor {
            index: self,
            record: 0,
            byte: self.first_byte,
            time: i128::from(self.earliest),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SegmentIndexCursor<'a> {
    index: &'a SegmentIndex,
    record: u32,
    byte: u64,
    time: i128,
}
impl SegmentIndexCursor<'_> {
    /// One reference per work unit. Arithmetic, source slices and time creation
    /// all succeed before cursor state changes; repeated failures cannot skip rows.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<IndexStep> {
        if self.record == u32::from(self.index.count) {
            return Ok(IndexStep::End);
        }
        if !work.try_charge(1) {
            return Ok(IndexStep::Yield);
        }
        let at = usize::try_from(self.record)
            .map_err(|_| Error::Overflow)?
            .checked_mul(12)
            .and_then(|n| n.checked_add(self.index.records))
            .ok_or(Error::Overflow)?;
        let b = self.index.raw.payload();
        let raw_size_word = fields::u32_at(b, at)?;
        let duration = fields::u32_at(b, at + 4)?;
        let raw_sap_word = fields::u32_at(b, at + 8)?;
        let end = self
            .byte
            .checked_add(u64::from(raw_size_word & 0x7fff_ffff))
            .ok_or(Error::Overflow)?;
        let base = TimeBase::new(1, u64::from(self.index.timescale))?;
        let next_time = self
            .time
            .checked_add(i128::from(duration))
            .ok_or(Error::Overflow)?;
        let ordinal = self.record.checked_add(1).ok_or(Error::Overflow)?;
        let reference = SegmentReference {
            ordinal,
            references_index: raw_size_word & 0x8000_0000 != 0,
            range: self.byte..end,
            earliest_presentation: Time::from_ticks(self.time, base)?,
            presentation_duration: Time::from_ticks(i128::from(duration), base)?,
            starts_with_sap: raw_sap_word & 0x8000_0000 != 0,
            sap_type: u8::try_from((raw_sap_word >> 28) & 7).map_err(|_| Error::Overflow)?,
            sap_delta_ticks: raw_sap_word & 0x0fff_ffff,
            raw_size_word,
            raw_sap_word,
        };
        self.record = ordinal;
        self.byte = end;
        self.time = next_time;
        Ok(IndexStep::Reference(reference))
    }
}
