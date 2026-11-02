//! Explicit pinned-MOV observation projection. Never changes source sample facts.
//! This is not the edit occurrence selector, a standards rule, or a parity waiver.
use crate::{Access, EditEntry, Sample, SampleDescription, SampleTable, TrackHeader, fields};
use safe_media_core::{Bytes, Error, Result, Rounding, Time, TimeBase};
use std::ops::Range;

#[derive(Clone, Copy, Debug)]
pub struct Mov9Edit {
    pub entry: EditEntry,
    pub movie_start: Time,
    pub movie_base: TimeBase,
}

#[derive(Clone, Copy, Debug)]
pub struct Mov9View {
    media_base: TimeBase,
    sync_entries: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mov9PacketView {
    pub source_ordinal: u64,
    pub source_description: u32,
    pub source_range: Range<u64>,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: i64,
    pub flags: u32,
}

/// Explicit narrow native catalogue projection for authored raw-video and tx3g
/// carriers. Raw source headers/descriptions stay authoritative and retained.
#[derive(Clone, Debug)]
pub struct Mov9StreamView {
    pub track_id: u32,
    pub codec: &'static str,
    pub media_type: u8,
    pub time_base: TimeBase,
    pub extradata: Bytes,
    pub language: [u8; 3],
    pub handler_name: Bytes,
    pub vendor_id: Option<[u8; 4]>,
}
impl Mov9StreamView {
    /// Source-operation context is required for native tag presence. The pinned
    /// no-stream-info operation omits the raw-video vendor tag for an empty regular
    /// table initialized by fragments. The raw source entry remains untouched.
    /// Audio catalogue projection is still unsupported; no video rule is applied.
    pub fn for_sample_table(
        header: &TrackHeader,
        table: &SampleTable,
        has_fragments: bool,
    ) -> Result<Self> {
        let entry = table
            .descriptions()
            .first()
            .ok_or(Error::Invalid("native catalogue has no sample entry"))?;
        let mut view = Self::new(header, entry)?;
        if view.codec == "rawvideo" && table.sample_count() == 0 {
            if !has_fragments {
                return Err(Error::Unsupported(
                    "native empty nonfragmented video catalogue profile",
                ));
            }
            view.vendor_id = None;
        }
        Ok(view)
    }
    /// Description-only projection for the regular sample profile. Whole source
    /// operations use `for_sample_table` to retain initialization context.
    pub fn new(header: &TrackHeader, entry: &SampleDescription) -> Result<Self> {
        let (codec, media_type, vendor_id, extra_start) = match (&header.handler, &entry.format) {
            (b"vide", b"raw ") => {
                fields::slice(entry.raw().payload(), 0, 78)?;
                let vendor = fields::fourcc(entry.raw().payload(), 12)?;
                if vendor != [0; 4] {
                    return Err(Error::Unsupported(
                        "native raw-video nonzero vendor projection",
                    ));
                }
                ("rawvideo", 0, Some(vendor), entry.raw().bytes().len())
            }
            (b"sbtl" | b"text" | b"subt", b"tx3g") => {
                fields::slice(entry.raw().payload(), 0, 38)?;
                (
                    "mov_text",
                    3,
                    None,
                    usize::from(entry.raw().header().header_size) + 8,
                )
            }
            _ => {
                return Err(Error::Unsupported(
                    "native codec/carriage catalogue projection",
                ));
            }
        };
        let mdhd = header.raw_mdhd.payload();
        let (version, _) = fields::full(mdhd)?;
        let language_at = match version {
            0 => 20,
            1 => 32,
            _ => return Err(Error::Unsupported("native media header version")),
        };
        let packed = fields::u16_at(mdhd, language_at)?;
        let mut language = [0; 3];
        for (i, byte) in language.iter_mut().enumerate() {
            let value = u8::try_from((packed >> (10 - 5 * i)) & 31).map_err(|_| Error::Overflow)?;
            if !(1..=26).contains(&value) || packed & 0x8000 != 0 {
                return Err(Error::Unsupported("native legacy/unmapped media language"));
            }
            *byte = value + 0x60;
        }
        let hdlr = header.raw_hdlr.payload();
        // Other QuickTime Pascal/component-name layouts are not silently treated
        // as this ISO-shaped, null-terminated handler-name observation profile.
        if fields::u32_at(hdlr, 4)? != 0 || fields::slice(hdlr, 12, 12)? != [0; 12] {
            return Err(Error::Unsupported(
                "native legacy handler catalogue projection",
            ));
        }
        let tail = hdlr.get(24..).ok_or(Error::Truncated)?;
        if tail.len() > 256 {
            return Err(Error::Limit {
                requested: tail.len(),
                maximum: 256,
            });
        }
        let name_len = tail
            .iter()
            .position(|&c| c == 0)
            .ok_or(Error::Unsupported("native unterminated handler name"))?;
        if !tail[..name_len].iter().all(u8::is_ascii) {
            return Err(Error::Unsupported("native non-ASCII handler projection"));
        }
        let name_start = usize::from(header.raw_hdlr.header().header_size) + 24;
        Ok(Self {
            track_id: header.track_id,
            codec,
            media_type,
            time_base: header.time_base()?,
            extradata: entry
                .raw()
                .bytes()
                .slice(extra_start..entry.raw().bytes().len())?,
            language,
            handler_name: header
                .raw_hdlr
                .bytes()
                .slice(name_start..name_start + name_len)?,
            vendor_id,
        })
    }
}

fn native_ticks(time: Option<Time>, base: TimeBase) -> Result<Option<i64>> {
    time.map(|value| {
        let ticks = value.to_i64_ticks(base, Rounding::NearestAway)?;
        if ticks == i64::MIN {
            return Err(Error::Unsupported("native timestamp sentinel collision"));
        }
        Ok(ticks)
    })
    .transpose()
}
impl Mov9View {
    pub fn new(media_base: TimeBase, sync_entries: Option<u32>) -> Self {
        Self {
            media_base,
            sync_entries,
        }
    }
    /// Point/flag projection for an independently selected source occurrence.
    /// The caller must not obtain occurrence selection from expected native output.
    pub fn project(&self, source: &Sample, edit: Option<Mov9Edit>) -> Result<Mov9PacketView> {
        if source.partial_sync || source.dependency.is_some() || source.access == Access::Unknown {
            return Err(Error::Unsupported(
                "native dependency/recovery flag profile not frozen",
            ));
        }
        let times = source.source_times(self.media_base)?;
        if times.dts.is_none() || times.cts.is_none() {
            return Err(Error::Unsupported("native clock origin unresolved"));
        }
        let mut pts = times.cts;
        let mut dts = times.dts;
        let mut flags = u32::from(source.is_container_sync());
        // Observed on the exact n9.0 worker: explicit empty stss still makes its
        // first packet key. This does not grant source random-access/recovery proof.
        if self.sync_entries == Some(0) && source.ordinal == 1 {
            flags |= 1;
        }
        if let Some(mapping) = edit {
            if mapping.entry.is_empty() {
                return Err(Error::Invalid("empty edit has no packet occurrence"));
            }
            if self.sync_entries == Some(0) || source.composition_offset.is_some() {
                return Err(Error::Unsupported(
                    "native edited/reordered or empty-sync profile not frozen",
                ));
            }
            let map = |time: Option<Time>| -> Result<Option<Time>> {
                time.map(|t| {
                    mapping
                        .entry
                        .map_rate_one(t, mapping.movie_start, self.media_base)
                })
                .transpose()
                .map(Option::flatten)
            };
            pts = map(pts)?;
            dts = map(dts)?;
            let source_start =
                Time::from_ticks(i128::from(mapping.entry.media_time), self.media_base)?;
            let source_end =
                source_start.checked_add(mapping.entry.duration(mapping.movie_base)?)?;
            if times
                .cts
                .is_some_and(|cts| cts < source_start || cts >= source_end)
            {
                flags |= 4;
            }
        }
        Ok(Mov9PacketView {
            source_ordinal: source.ordinal,
            source_description: source.description_index,
            source_range: source.range.clone(),
            pts: native_ticks(pts, self.media_base)?,
            dts: native_ticks(dts, self.media_base)?,
            duration: times
                .decode_delta
                .to_i64_ticks(self.media_base, Rounding::NearestAway)?,
            flags,
        })
    }
}
impl Mov9PacketView {
    /// Observed native occurrence-index duration rule. The next DTS must come
    /// from the source-derived occurrence plan, never from expected native output.
    pub fn with_following_dts(mut self, following: Option<i64>) -> Result<Self> {
        if let (Some(current), Some(next)) = (self.dts, following) {
            if next < current {
                return Ok(self);
            }
            self.duration = next.checked_sub(current).ok_or(Error::Overflow)?;
        }
        Ok(self)
    }
}
