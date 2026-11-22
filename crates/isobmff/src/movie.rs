//! Movie and track headers, sample descriptions and edit lists, read from their boxes as
//! stored. An edit list maps a time at rate one on request; it is not applied to samples
//! here, and no codec configuration is interpreted.
//!
//! Specification: Adobe F4V 10.1 §§2.4–2.8; Apple QuickTime File Format (sample
//! descriptions, subtitle media).
use crate::{
    Limits,
    boxes::{BoxChildren, BoxView},
    fields, policy,
};
use triergon_core::{Error, Result, Time, TimeBase};

/// The movie header `mvhd`: the movie clock and duration.
#[derive(Debug)]
pub struct MovieHeader {
    /// Ticks per second of the movie clock, in which `tkhd` and edit durations count;
    /// never zero.
    pub timescale: u32,
    /// Duration in ticks of the movie timescale as stored; all ones means unknown (see
    /// [`declared_duration_ticks`](Self::declared_duration_ticks)).
    pub duration: u64,
    raw: BoxView,
}
impl MovieHeader {
    /// The movie clock, `1/timescale` seconds per tick. A zero timescale is invalid.
    pub fn time_base(&self) -> Result<TimeBase> {
        TimeBase::new(1, u64::from(self.timescale))
    }

    /// Duration in movie ticks when the header declares one. The raw field
    /// retains the all-ones encoding of an unknown duration.
    pub fn declared_duration_ticks(&self) -> Option<u64> {
        let unknown = match self.raw.payload().first().copied() {
            Some(0) => u64::from(u32::MAX),
            Some(1) => u64::MAX,
            _ => return None,
        };
        (self.duration != unknown).then_some(self.duration)
    }

    /// Reads an `mvhd` of version 0 or 1. `Error::Invalid` for another box type or a zero
    /// timescale, `Error::Unsupported` for another version or nonzero flags.
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

/// A track's identity, media clock and handler, from its `tkhd`, `mdhd` and `hdlr` boxes,
/// which are kept as read.
#[derive(Debug)]
pub struct TrackHeader {
    /// `track_ID` of the `tkhd` box; never zero.
    pub track_id: u32,
    /// Ticks per second of the media clock (`mdhd`), in which sample times count; zero
    /// only in a header read by
    /// [`RecoveryTrackHeader`](crate::compat::mov::recovery::RecoveryTrackHeader).
    pub timescale: u32,
    /// Media duration in ticks of the media timescale, as `mdhd` stores it.
    pub duration: u64,
    /// The `hdlr` handler type, such as `vide` or `soun`; see
    /// [`media_kind`](crate::media_kind).
    pub handler: [u8; 4],
    /// The `tkhd` track-enabled flag.
    pub enabled: bool,
    /// The `tkhd` box as read.
    pub raw_tkhd: BoxView,
    /// The `mdhd` box as read.
    pub raw_mdhd: BoxView,
    /// The `hdlr` box as read.
    pub raw_hdlr: BoxView,
}
impl TrackHeader {
    /// The media clock, `1/timescale` seconds per tick. A zero timescale is invalid.
    pub fn time_base(&self) -> Result<TimeBase> {
        TimeBase::new(1, u64::from(self.timescale))
    }
    /// Reads the three boxes of one track. `Error::Invalid` for a wrong box type, a zero
    /// track ID or a zero timescale; `Error::Unsupported` for an unknown `tkhd` or `mdhd`
    /// version, nonzero `mdhd` flags, or nonzero `hdlr` version or flags.
    pub fn parse(tkhd: &BoxView, mdhd: &BoxView, hdlr: &BoxView) -> Result<Self> {
        Self::parse_inner(tkhd, mdhd, hdlr, false)
    }
    /// [`parse`](Self::parse), also accepting a zero media timescale when `observed_zero`
    /// is `true`.
    pub(crate) fn parse_inner(
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

/// The fixed fields a sample entry has before its child boxes, as the caller names them
/// for [`SampleDescription::extensions`].
#[derive(Clone, Copy, Debug)]
pub enum EntryLayout {
    /// A visual sample entry: 70 bytes of fields after the 8 common ones.
    Visual,
    /// A version 0 audio or QuickTime sound entry: 20 bytes after the common ones.
    AudioV0,
    /// A `tx3g` text entry: 30 bytes after the common ones.
    Tx3g,
}

/// One sample entry of `stsd`, with its box kept whole.
#[derive(Clone, Debug)]
pub struct SampleDescription {
    /// 1-based position in `stsd`, as
    /// [`Sample::description_index`](crate::Sample::description_index) counts.
    pub index: u32,
    /// The entry's four-character code (its box type), such as `avc1` or `mp4a`.
    pub format: [u8; 4],
    /// 1-based index of the entry's data reference in `dref`; never zero.
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
    /// The whole sample entry box.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// Walks the child boxes after the fixed fields of `layout`, which the caller picks;
    /// nothing is inferred from the format. `Error::Invalid` for [`EntryLayout::Tx3g`] on
    /// another format, `Error::Unsupported` for [`EntryLayout::AudioV0`] on a version 1 or
    /// 2 sound entry.
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

/// One `elst` entry, as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditEntry {
    /// Duration of the edit in ticks of the movie timescale.
    pub segment_duration: u64,
    /// Media time the edit starts at, in ticks of the media timescale; -1 for an empty
    /// edit.
    pub media_time: i64,
    /// Integer part of the media rate; 1 for normal speed.
    pub rate_integer: i16,
    /// Fractional part of the media rate, in 1/65536 units.
    pub rate_fraction: i16,
}
impl EditEntry {
    /// Whether this is an empty edit (`media_time` -1), which presents no media.
    pub fn is_empty(&self) -> bool {
        self.media_time == -1
    }
    /// The segment duration as a [`Time`] in `movie_base`, the movie time base.
    pub fn duration(&self, movie_base: TimeBase) -> Result<Time> {
        Time::from_ticks(i128::from(self.segment_duration), movie_base)
    }
    /// Maps the media composition time `source_cts` to movie time for an edit that
    /// starts at `movie_start`, the media clock being `media_base`. `None` for an empty
    /// edit, `Error::Unsupported` for a rate other than 1. The time is not checked against
    /// the edit's extent and nothing is rounded.
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
            return Err(Error::Unsupported("edit presentation at a non-unit rate"));
        }
        let source_start = Time::from_ticks(i128::from(self.media_time), media_base)?;
        Ok(Some(
            source_cts
                .checked_sub(source_start)?
                .checked_add(movie_start)?,
        ))
    }
}

/// A checked `elst` box of version 0 (32-bit fields) or 1 (64-bit fields), as Adobe F4V
/// 10.1 §2.5.4 describes both. Entries are read on demand with their stored rates.
#[derive(Debug)]
pub struct EditList {
    raw: BoxView,
    count: u32,
    version: u8,
}
impl EditList {
    /// Checks the box and its entry count against `limits.max_records`. `Error::Invalid`
    /// for another box type or a size that does not match the count, `Error::Unsupported`
    /// for a version above 1 or nonzero flags.
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
    /// The `elst` box as read.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// Number of declared `elst` rows, including empty edits.
    pub fn len(&self) -> u32 {
        self.count
    }
    /// Whether the list has no entries.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// The entry at 0-based `index`; `Error::Invalid` past the last.
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
