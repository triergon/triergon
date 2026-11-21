//! The track fragment random access box (`tfra`) and the movie fragment random access
//! offset box (`mfro`) that locates the `mfra` box holding it at the end of a file. A
//! `tfra` row names a random access sample by its time and by `moof`, `traf`, `trun`
//! and sample number. The rows are what the file states and are not checked against
//! the fragments; an empty table is not read as marking every sample a sync sample.
//!
//! Specification: Adobe F4V 10.1 §§2.16.1–2.16.2; MPEG m76694 Table 9.
use crate::{Limits, Next, boxes::BoxView, fields, policy};
use std::ops::Range;
use triergon_core::{Error, Result, WorkBudget};

/// One row of a `tfra` box, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FragmentRandomAccessEntry {
    /// 1-based position of the row in the box.
    pub ordinal: u32,
    /// `time` of the random access sample, in ticks of the track's media timescale.
    pub time_ticks: u64,
    /// `moof_offset`: absolute file offset in bytes of the `moof` box holding the
    /// sample.
    pub moof_offset: u64,
    /// 1-based `traf` within that `moof`; never zero.
    pub traf_number: u32,
    /// 1-based `trun` within that `traf`; never zero.
    pub trun_number: u32,
    /// 1-based sample within that `trun`; never zero.
    pub sample_number: u32,
}

/// A parsed track fragment random access box (`tfra`), kept as the original box.
/// Parsing checks the header and that the rows fill the box exactly; each row is read
/// when [`cursor`](Self::cursor) reaches it.
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
    /// Parses a `tfra` box. Fails with `Error::Unsupported` for a version above 1 or
    /// nonzero flags, `Error::Invalid` for another box, a zero `track_ID`, nonzero
    /// reserved bits or rows that do not fill the box exactly, and `Error::Limit` for
    /// more rows than `limits.max_records`.
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
    /// The whole `tfra` box.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// `track_ID` of the track the rows belong to; never zero.
    pub fn track_id(&self) -> u32 {
        self.track_id
    }
    /// `number_of_entry`: the number of rows.
    pub fn entry_count(&self) -> u32 {
        self.count
    }
    /// Box version: 0 for 32-bit, 1 for 64-bit time and `moof` offset.
    pub fn version(&self) -> u8 {
        self.version
    }
    /// Byte widths, 1 to 4, of the `traf`, `trun` and sample numbers, in that order.
    pub fn number_widths(&self) -> [u8; 3] {
        self.widths
    }
    /// A cursor over the rows, in box order.
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

/// The rows of a [`TrackFragmentRandomAccess`], in box order.
#[derive(Debug)]
pub struct FragmentRandomAccessCursor<'a> {
    index: &'a TrackFragmentRandomAccess,
    row: u32,
    failed: Option<Error>,
}
impl FragmentRandomAccessCursor<'_> {
    /// Reads the next row, charging one work unit. A row with a zero `traf`, `trun` or
    /// sample number fails with `Error::Invalid`, and every later poll returns the same
    /// error. Rows are returned as stored: their order, offsets and times are not
    /// checked.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<FragmentRandomAccessEntry>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.row == self.index.count {
            return Ok(Next::End);
        }
        if !work.try_charge(1) {
            return Ok(Next::Yield);
        }
        match self.index.entry(self.row) {
            Ok(entry) => {
                self.row = entry.ordinal;
                Ok(Next::Item(entry))
            }
            Err(error) => {
                self.failed = Some(error);
                Err(error)
            }
        }
    }
}

/// A movie fragment random access offset box (`mfro`), the last box of a file, which
/// states the size of the `mfra` box it ends.
///
/// Parsing needs the `mfro` box itself and the source length. Reading the `mfra` box
/// at [`parent_range`](Self::parent_range) and its `tfra` boxes is the caller's job.
#[derive(Clone, Debug)]
pub struct MovieFragmentRandomAccessTail {
    raw: BoxView,
    parent: Range<u64>,
}
impl MovieFragmentRandomAccessTail {
    /// Parses an `mfro` box that ends at `source_end`, the source length in bytes.
    /// Fails with `Error::Unsupported` for a version above 1 or nonzero flags, and
    /// `Error::Invalid` for another box, a box not ending at `source_end`, or a stated
    /// `mfra` size smaller than this box plus an 8-byte header or larger than the
    /// source.
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
    /// The whole `mfro` box.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// Absolute byte range the stated `mfra` box occupies; it ends at the source end.
    pub fn parent_range(&self) -> Range<u64> {
        self.parent.clone()
    }
    /// Checks that `parent` is the `mfra` box at [`parent_range`](Self::parent_range)
    /// and ends with this `mfro` box, byte for byte; `Error::Invalid` otherwise.
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
        // The tail is 16 bytes, or 24 with a 64-bit box size. Comparing it also
        // refuses a parent at the same address with different bytes.
        let embedded = fields::slice(parent.bytes().as_ref(), offset, self.raw.bytes().len())?;
        if embedded != self.raw.bytes().as_ref() {
            return Err(Error::Invalid("mfro bytes differ from mfra tail"));
        }
        Ok(())
    }
}
