//! Historical primary Adobe F4V10.1 §§2.16.1-.2 syntax and MPEG m76694
//! table9 Original corroboration. Compact source hints; no native-clock or
//! decoder-recovery claim, including for an empty tfra table.
use crate::{BoxView, Limits, fields, policy};
use std::ops::Range;
use triergon_core::{Error, Result, WorkBudget};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FragmentRandomAccessEntry {
    pub ordinal: u32,
    pub time_ticks: u64,
    pub moof_offset: u64,
    pub traf_number: u32,
    pub trun_number: u32,
    pub sample_number: u32,
}

#[derive(Debug)]
pub enum FragmentRandomAccessStep {
    Entry(FragmentRandomAccessEntry),
    Yield,
    End,
}

/// Retains the complete original box. Header/bounds validation is constant
/// work; each fixed-width row is interpreted only when its cursor is polled.
#[derive(Clone, Debug)]
pub struct TrackFragmentRandomAccess {
    raw: BoxView,
    version: u8,
    track_id: u32,
    widths: [u8; 3],
    count: u32,
    stride: usize,
}
impl TrackFragmentRandomAccess {
    pub fn parse(raw: BoxView, limits: Limits) -> Result<Self> {
        if raw.header().kind != *b"tfra" {
            return Err(Error::Invalid("not tfra"));
        }
        let body = raw.payload();
        let (version, flags) = fields::full(body)?;
        if version > 1 || flags != 0 {
            return Err(Error::Unsupported("tfra version/flags"));
        }
        let track_id = fields::u32_at(body, 4)?;
        if track_id == 0 {
            return Err(Error::Invalid("zero tfra track ID"));
        }
        let descriptor = fields::u32_at(body, 8)?;
        if descriptor & !0x3f != 0 {
            return Err(Error::Invalid("nonzero tfra reserved bits"));
        }
        let widths = [
            u8::try_from((descriptor >> 4) & 3).map_err(|_| Error::Overflow)? + 1,
            u8::try_from((descriptor >> 2) & 3).map_err(|_| Error::Overflow)? + 1,
            u8::try_from(descriptor & 3).map_err(|_| Error::Overflow)? + 1,
        ];
        let count = fields::u32_at(body, 12)?;
        let stride = (if version == 0 { 8usize } else { 16usize })
            .checked_add(usize::from(widths[0]))
            .and_then(|n| n.checked_add(usize::from(widths[1])))
            .and_then(|n| n.checked_add(usize::from(widths[2])))
            .ok_or(Error::Overflow)?;
        fields::exact_rows(body, 16, count, stride)?;
        policy(count, limits.max_records)?;
        Ok(Self {
            raw,
            version,
            track_id,
            widths,
            count,
            stride,
        })
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn track_id(&self) -> u32 {
        self.track_id
    }
    pub fn entry_count(&self) -> u32 {
        self.count
    }
    pub fn version(&self) -> u8 {
        self.version
    }
    pub fn number_widths(&self) -> [u8; 3] {
        self.widths
    }
    pub fn cursor(&self) -> FragmentRandomAccessCursor<'_> {
        FragmentRandomAccessCursor {
            index: self,
            row: 0,
            failed: None,
        }
    }
    fn entry(&self, row: u32) -> Result<FragmentRandomAccessEntry> {
        if row >= self.count {
            return Err(Error::Invalid("tfra row outside table"));
        }
        let at = usize::try_from(row)
            .map_err(|_| Error::Overflow)?
            .checked_mul(self.stride)
            .and_then(|n| n.checked_add(16))
            .ok_or(Error::Overflow)?;
        let body = self.raw.payload();
        let (time_ticks, moof_offset, number_at) = if self.version == 0 {
            (
                u64::from(fields::u32_at(body, at)?),
                u64::from(fields::u32_at(
                    body,
                    at.checked_add(4).ok_or(Error::Overflow)?,
                )?),
                at.checked_add(8).ok_or(Error::Overflow)?,
            )
        } else {
            (
                fields::u64_at(body, at)?,
                fields::u64_at(body, at.checked_add(8).ok_or(Error::Overflow)?)?,
                at.checked_add(16).ok_or(Error::Overflow)?,
            )
        };
        let traf_number = number(body, number_at, self.widths[0])?;
        let number_at = number_at
            .checked_add(usize::from(self.widths[0]))
            .ok_or(Error::Overflow)?;
        let trun_number = number(body, number_at, self.widths[1])?;
        let number_at = number_at
            .checked_add(usize::from(self.widths[1]))
            .ok_or(Error::Overflow)?;
        let sample_number = number(body, number_at, self.widths[2])?;
        if traf_number == 0 || trun_number == 0 || sample_number == 0 {
            return Err(Error::Invalid("zero tfra coordinate"));
        }
        Ok(FragmentRandomAccessEntry {
            ordinal: row.checked_add(1).ok_or(Error::Overflow)?,
            time_ticks,
            moof_offset,
            traf_number,
            trun_number,
            sample_number,
        })
    }
}

fn number(body: &[u8], at: usize, width: u8) -> Result<u32> {
    let width = usize::from(width);
    let mut padded = [0u8; 4];
    let start = padded
        .len()
        .checked_sub(width)
        .ok_or(Error::Invalid("tfra number width"))?;
    padded[start..].copy_from_slice(fields::slice(body, at, width)?);
    Ok(u32::from_be_bytes(padded))
}

#[derive(Debug)]
pub struct FragmentRandomAccessCursor<'a> {
    index: &'a TrackFragmentRandomAccess,
    row: u32,
    failed: Option<Error>,
}
impl FragmentRandomAccessCursor<'_> {
    /// One bounded row per unit. A bad record is sticky and cannot be skipped
    /// on retry. No ordering, range-target or source-clock inference is made.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<FragmentRandomAccessStep> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.row == self.index.count {
            return Ok(FragmentRandomAccessStep::End);
        }
        if !work.try_charge(1) {
            return Ok(FragmentRandomAccessStep::Yield);
        }
        match self.index.entry(self.row) {
            Ok(entry) => {
                self.row = entry.ordinal;
                Ok(FragmentRandomAccessStep::Entry(entry))
            }
            Err(error) => {
                self.failed = Some(error);
                Err(error)
            }
        }
    }
}

/// Tail lookup requires actual supplied `mfro` bytes and a known source end.
/// It does not scan signatures, resolve tfra targets or issue range requests.
#[derive(Clone, Debug)]
pub struct MovieFragmentRandomAccessTail {
    raw: BoxView,
    parent: Range<u64>,
}
impl MovieFragmentRandomAccessTail {
    pub fn parse(raw: BoxView, source_end: u64) -> Result<Self> {
        if raw.header().kind != *b"mfro" {
            return Err(Error::Invalid("not mfro"));
        }
        let body = raw.payload();
        let (version, flags) = fields::full(body)?;
        if version > 1 || flags != 0 {
            return Err(Error::Unsupported("mfro version/flags"));
        }
        fields::exact_rows(body, 8, 0, 0)?;
        if raw.header().end()? != Some(source_end) {
            return Err(Error::Invalid("mfro not at supplied source end"));
        }
        let size = u64::from(fields::u32_at(body, 4)?);
        let tail_size = raw.header().size.ok_or(Error::Invalid("unbounded mfro"))?;
        if size < tail_size.checked_add(8).ok_or(Error::Overflow)? {
            return Err(Error::Invalid("mfro parent shorter than tail and header"));
        }
        let start = source_end
            .checked_sub(size)
            .ok_or(Error::Invalid("mfro parent outside source"))?;
        Ok(Self {
            raw,
            parent: start..source_end,
        })
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn parent_range(&self) -> Range<u64> {
        self.parent.clone()
    }
    pub fn require_mfra(&self, parent: &BoxView) -> Result<()> {
        if parent.header().kind != *b"mfra"
            || parent.header().start != self.parent.start
            || parent.header().end()? != Some(self.parent.end)
        {
            return Err(Error::Invalid("mfro parent kind/range mismatch"));
        }
        if self.raw.header().start < parent.header().payload_range()?.start {
            return Err(Error::Invalid("mfro overlaps mfra header"));
        }
        let offset = self
            .raw
            .header()
            .start
            .checked_sub(parent.header().start)
            .ok_or(Error::Overflow)?;
        let offset = usize::try_from(offset).map_err(|_| Error::Overflow)?;
        // The complete tail is fixed16 or24 bytes in this bounded profile.
        // Comparing it also refuses a same-address parent with different bytes.
        let embedded = fields::slice(parent.bytes().as_ref(), offset, self.raw.bytes().len())?;
        if embedded != self.raw.bytes().as_ref() {
            return Err(Error::Invalid("mfro bytes differ from mfra tail"));
        }
        Ok(())
    }
}
