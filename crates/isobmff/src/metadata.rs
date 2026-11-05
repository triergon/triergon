//! Source fields only; no edit application, colour policy or codec decoder.
//! Adobe §§2.4-2.8; Apple sample-description/subtitle documentation.
use crate::{BoxChildren, BoxView, Limits, fields, policy};
use triergon_core::{Error, Result, Time, TimeBase};

#[derive(Debug)]
pub struct MovieHeader {
    pub timescale: u32,
    pub duration: u64,
    pub raw: BoxView,
}
impl MovieHeader {
    pub fn time_base(&self) -> Result<TimeBase> {
        TimeBase::new(1, u64::from(self.timescale))
    }
    pub fn parse(raw: &BoxView) -> Result<Self> {
        if raw.header().kind != *b"mvhd" {
            return Err(Error::Invalid("not mvhd"));
        }
        let (timescale, duration) = clock_fields(raw.payload())?;
        Ok(Self {
            timescale,
            duration,
            raw: raw.clone(),
        })
    }
}

fn clock_fields(b: &[u8]) -> Result<(u32, u64)> {
    let (scale, duration) = raw_clock_fields(b)?;
    if scale == 0 {
        return Err(Error::Invalid("zero media/movie timescale"));
    }
    Ok((scale, duration))
}
fn raw_clock_fields(b: &[u8]) -> Result<(u32, u64)> {
    let (version, flags) = fields::full(b)?;
    if flags != 0 {
        return Err(Error::Unsupported("clock header flags"));
    }
    let (scale, duration) = match version {
        0 => (fields::u32_at(b, 12)?, u64::from(fields::u32_at(b, 16)?)),
        1 => (fields::u32_at(b, 20)?, fields::u64_at(b, 24)?),
        _ => return Err(Error::Unsupported("clock header version")),
    };
    Ok((scale, duration))
}

#[derive(Debug)]
pub struct TrackHeader {
    pub track_id: u32,
    pub timescale: u32,
    pub duration: u64,
    pub handler: [u8; 4],
    pub enabled: bool,
    pub raw_tkhd: BoxView,
    pub raw_mdhd: BoxView,
    pub raw_hdlr: BoxView,
}
impl TrackHeader {
    pub fn time_base(&self) -> Result<TimeBase> {
        TimeBase::new(1, u64::from(self.timescale))
    }
    pub fn parse(tkhd: &BoxView, mdhd: &BoxView, hdlr: &BoxView) -> Result<Self> {
        Self::parse_inner(tkhd, mdhd, hdlr, false)
    }
    fn parse_inner(
        tkhd: &BoxView,
        mdhd: &BoxView,
        hdlr: &BoxView,
        observed_zero: bool,
    ) -> Result<Self> {
        if tkhd.header().kind != *b"tkhd"
            || mdhd.header().kind != *b"mdhd"
            || hdlr.header().kind != *b"hdlr"
        {
            return Err(Error::Invalid("track header box types"));
        }
        let (version, flags) = fields::full(tkhd.payload())?;
        let at = match version {
            0 => 12,
            1 => 20,
            _ => return Err(Error::Unsupported("tkhd version")),
        };
        let track_id = fields::u32_at(tkhd.payload(), at)?;
        if track_id == 0 {
            return Err(Error::Invalid("zero track ID"));
        }
        let (timescale, duration) = if observed_zero {
            raw_clock_fields(mdhd.payload())?
        } else {
            clock_fields(mdhd.payload())?
        };
        fields::full_zero(hdlr.payload())?;
        let handler = fields::fourcc(hdlr.payload(), 8)?;
        Ok(Self {
            track_id,
            timescale,
            duration,
            handler,
            enabled: flags & 1 != 0,
            raw_tkhd: tkhd.clone(),
            raw_mdhd: mdhd.clone(),
            raw_hdlr: hdlr.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum EntryLayout {
    Visual,
    AudioV0,
    Tx3g,
}

#[derive(Clone, Debug)]
pub struct SampleDescription {
    pub index: u32,
    pub format: [u8; 4],
    pub data_reference_index: u16,
    raw: BoxView,
}
impl SampleDescription {
    pub(crate) fn parse(raw: BoxView, index: u32) -> Result<Self> {
        let body = raw.payload();
        fields::slice(body, 0, 8)?;
        if body[..6] != [0; 6] {
            return Err(Error::Invalid("sample entry reserved bytes"));
        }
        let data_reference_index = fields::u16_at(body, 6)?;
        if data_reference_index == 0 {
            return Err(Error::Invalid("zero data reference index"));
        }
        Ok(Self {
            index,
            format: raw.header().kind,
            data_reference_index,
            raw,
        })
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// The caller explicitly chooses a sourced entry layout. Unknown formats and
    /// audio v1/v2 remain completely preserved, never treated as visual entries.
    pub fn extensions(&self, layout: EntryLayout, limits: Limits) -> Result<BoxChildren> {
        let skip = match layout {
            EntryLayout::Visual => 78,
            EntryLayout::Tx3g => {
                if self.format != *b"tx3g" {
                    return Err(Error::Invalid("not tx3g"));
                }
                38
            }
            EntryLayout::AudioV0 => {
                if fields::u16_at(self.raw.payload(), 8)? != 0 {
                    return Err(Error::Unsupported("audio v1/v2 entry layout"));
                }
                28
            }
        };
        self.raw.children(skip, limits)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditEntry {
    pub segment_duration: u64,
    pub media_time: i64,
    pub rate_integer: i16,
    pub rate_fraction: i16,
}
impl EditEntry {
    pub fn is_empty(&self) -> bool {
        self.media_time == -1
    }
    pub fn duration(&self, movie_base: TimeBase) -> Result<Time> {
        Time::from_ticks(i128::from(self.segment_duration), movie_base)
    }
    /// Point mapping only, with explicit caller-supplied movie cursor. Does not
    /// infer clipping/display extents, apply dwell/reverse rates or round clocks.
    pub fn map_rate_one(
        &self,
        source_cts: Time,
        movie_start: Time,
        media_base: TimeBase,
    ) -> Result<Option<Time>> {
        if self.is_empty() {
            return Ok(None);
        }
        if self.rate_integer != 1 || self.rate_fraction != 0 {
            return Err(Error::Unsupported("non-unit edit presentation remains G04"));
        }
        let source_start = Time::from_ticks(i128::from(self.media_time), media_base)?;
        Ok(Some(
            source_cts
                .checked_sub(source_start)?
                .checked_add(movie_start)?,
        ))
    }
}

/// Both widths are documented by Adobe §2.5.4. Keeps raw rates; does not
/// invent presentation/dwell/reverse/zero-duration edit semantics.
#[derive(Debug)]
pub struct EditList {
    raw: BoxView,
    count: u32,
    version: u8,
}
impl EditList {
    pub fn parse(raw: BoxView, limits: Limits) -> Result<Self> {
        if raw.header().kind != *b"elst" {
            return Err(Error::Invalid("not elst"));
        }
        let (version, flags) = fields::full(raw.payload())?;
        if flags != 0 || version > 1 {
            return Err(Error::Unsupported("edit list version/flags"));
        }
        let count = fields::u32_at(raw.payload(), 4)?;
        fields::exact_rows(raw.payload(), 8, count, if version == 0 { 12 } else { 20 })?;
        policy(count, limits.max_records)?;
        Ok(Self {
            raw,
            count,
            version,
        })
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn len(&self) -> u32 {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn entry(&self, index: u32) -> Result<EditEntry> {
        if index >= self.count {
            return Err(Error::Invalid("edit index"));
        }
        let stride = if self.version == 0 { 12usize } else { 20 };
        let p = usize::try_from(index)
            .map_err(|_| Error::Overflow)?
            .checked_mul(stride)
            .and_then(|v| v.checked_add(8))
            .ok_or(Error::Overflow)?;
        let b = self.raw.payload();
        let (segment_duration, media_time, rate) = if self.version == 0 {
            (
                u64::from(fields::u32_at(b, p)?),
                i64::from(fields::i32_at(b, p + 4)?),
                p + 8,
            )
        } else {
            (fields::u64_at(b, p)?, fields::i64_at(b, p + 8)?, p + 16)
        };
        Ok(EditEntry {
            segment_duration,
            media_time,
            rate_integer: fields::i16_at(b, rate)?,
            rate_fraction: fields::i16_at(b, rate + 2)?,
        })
    }
}

/// Raw source clock remains zero/invalid. Only the explicitly labelled native
/// projection has a fallback base; source_seconds must still reject that scale.
#[derive(Debug)]
pub struct Mov9RecoveryTrackHeader {
    source: TrackHeader,
}
impl Mov9RecoveryTrackHeader {
    pub fn parse(
        tkhd: &BoxView,
        mdhd: &BoxView,
        hdlr: &BoxView,
        _profile: crate::PinnedMovD32RegularRecoveryV1,
    ) -> Result<Self> {
        Ok(Self {
            source: TrackHeader::parse_inner(tkhd, mdhd, hdlr, true)?,
        })
    }
    pub fn source(&self) -> &TrackHeader {
        &self.source
    }
    pub fn into_source(self) -> TrackHeader {
        self.source
    }
    pub fn native_projection_base(&self) -> Result<TimeBase> {
        TimeBase::new(1, u64::from(self.source.timescale.max(1)))
    }
    pub fn source_clock_valid(&self) -> bool {
        self.source.timescale != 0
    }
    pub fn source_clock_error(&self) -> Option<Error> {
        (!self.source_clock_valid()).then_some(Error::Invalid("zero media/movie timescale"))
    }
}
