//! The segment index box (`sidx`): [`SegmentIndex`] and the references its cursor
//! reads, each the byte range and presentation span of a subsegment or of a nested
//! index. The stream access point fields are what the file states, not proof that
//! decoding can start there.
//!
//! Specification: ETSI TS 126 244 V18.0.0 §13.4.
use crate::{Limits, Next, boxes::BoxView, fields, policy};
use std::ops::Range;
use triergon_core::{Error, Result, Time, TimeBase, WorkBudget};

/// One reference of a `sidx` box, with its byte range and times worked out from the
/// references before it.
#[derive(Clone, Debug)]
pub struct SegmentReference {
    /// 1-based position of the reference in the box.
    pub ordinal: u32,
    /// `reference_type`: true when the reference points at another `sidx` box, false
    /// when it points at media.
    pub references_index: bool,
    /// Absolute byte range in the file: the first reference starts `first_offset` bytes
    /// after the `sidx` box, each later one where the previous ends, and each spans its
    /// 31-bit `referenced_size`.
    pub range: Range<u64>,
    /// The box's `earliest_presentation_time` plus the preceding references' durations.
    pub earliest_presentation: Time,
    /// `subsegment_duration` converted from index ticks to an exact duration.
    pub presentation_duration: Time,
    /// `starts_with_SAP`: the file states that the referenced material starts with a
    /// stream access point.
    pub starts_with_sap: bool,
    /// `SAP_type`, 0 to 7: 1 to 6 name a stream access point type, 0 states no type and
    /// 7 is reserved. Kept as stored.
    pub sap_type: u8,
    /// `SAP_delta_time`, in ticks of the index timescale; 28 bits.
    pub sap_delta_ticks: u32,
    /// The reference's first 32-bit word as stored: `reference_type` and
    /// `referenced_size`.
    pub raw_size_word: u32,
    /// The reference's third 32-bit word as stored: `starts_with_SAP`, `SAP_type` and
    /// `SAP_delta_time`.
    pub raw_sap_word: u32,
}

/// A parsed segment index box (`sidx`), kept as the original box; its references are
/// read one at a time by [`cursor`](Self::cursor).
///
/// The references are not checked against the file. A caller that follows one must
/// check that it lands on a `moof` or `sidx` box, and must bound the depth of nested
/// indexes and detect cycles itself.
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
    /// Parses a `sidx` box. Fails with `Error::Unsupported` for a version above 1 or
    /// nonzero flags, `Error::Invalid` for another box, a zero `reference_ID` or
    /// timescale, nonzero reserved bits, a size-zero box or references that do not fill
    /// the box exactly, and `Error::Limit` for more references than
    /// `limits.max_records`.
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
    /// The whole `sidx` box.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// Box version: 0 for 32-bit, 1 for 64-bit earliest time and first offset.
    pub fn version(&self) -> u8 {
        self.version
    }
    /// `reference_ID`: the stream the index describes, usually a `track_ID`; never
    /// zero.
    pub fn reference_id(&self) -> u32 {
        self.reference_id
    }
    /// Ticks per second of the index's times; never zero.
    pub fn timescale(&self) -> u32 {
        self.timescale
    }
    /// `earliest_presentation_time` of the first reference, in ticks of
    /// [`timescale`](Self::timescale).
    pub fn earliest_presentation_ticks(&self) -> u64 {
        self.earliest
    }
    /// `first_offset`: bytes from the end of the `sidx` box to the first referenced
    /// byte.
    pub fn first_offset(&self) -> u64 {
        self.first_offset
    }
    /// Declared 16-bit number of references, including references to nested indexes.
    pub fn reference_count(&self) -> u16 {
        self.count
    }
    /// A cursor over the references, in box order.
    pub fn cursor(&self) -> SegmentIndexCursor<'_> {
        SegmentIndexCursor {
            index: self,
            record: 0,
            byte: self.first_byte,
            time: i128::from(self.earliest),
        }
    }
}

/// The references of a [`SegmentIndex`], in box order.
#[derive(Clone, Copy, Debug)]
pub struct SegmentIndexCursor<'a> {
    index: &'a SegmentIndex,
    record: u32,
    byte: u64,
    time: i128,
}
impl SegmentIndexCursor<'_> {
    /// Reads the next reference, charging one work unit. A reference whose byte range
    /// or time overflows fails and leaves the cursor where it was, so a later poll
    /// reports the same failure instead of skipping it.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<SegmentReference>> {
        if self.record == u32::from(self.index.count) {
            return Ok(Next::End);
        }
        if !work.try_charge(1) {
            return Ok(Next::Yield);
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
        Ok(Next::Item(reference))
    }
}
