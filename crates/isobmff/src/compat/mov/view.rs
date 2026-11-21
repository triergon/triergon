//! Single packets and stream entries as FFmpeg's MOV demuxer reports them:
//! `PacketView` is one packet, `PacketProjector` projects one source sample to it, and
//! `StreamView` is a track as the demuxer lists it. Which samples become packets, and
//! how often, is the packet and edit cursors' work. Source samples are never changed.
use crate::{
    Access, Sample,
    compat::mov::clocks::{ClockField, MediaClock},
    fields,
    movie::{EditEntry, SampleDescription, TrackHeader},
    table::SampleTable,
};
use std::ops::Range;
use triergon_core::{Bytes, Error, Result, Rounding, Time, TimeBase};

/// An edit-list entry placed on the movie timeline.
#[derive(Clone, Copy, Debug)]
pub struct ProjectedEdit {
    /// The `elst` entry.
    pub entry: EditEntry,
    /// The presentation time at which the edit starts: the sum of the durations of
    /// the entries before it.
    pub movie_start: Time,
    /// The `mvhd` time base in which `entry.segment_duration` counts.
    pub movie_base: TimeBase,
}

/// Projects one source sample, on its own, to the packet FFmpeg's MOV demuxer
/// reports for it.
#[derive(Clone, Copy, Debug)]
pub struct PacketProjector {
    media_base: TimeBase,
    sync_entries: Option<u32>,
}
/// A packet as FFmpeg's MOV demuxer reports it, naming the source sample it carries.
/// Times and the duration count ticks of the track's demux time base
/// ([`MediaClock::demux_base`](crate::compat::mov::clock::MediaClock::demux_base)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacketView {
    /// 1-based ordinal of the source sample.
    pub source_ordinal: u64,
    /// 1-based index of the sample's entry in `stsd`.
    pub source_description: u32,
    /// Absolute file offsets of the sample's bytes.
    pub source_range: Range<u64>,
    /// Presentation time; `None` where the field holds the demuxer's null value.
    pub pts: Option<i64>,
    /// Decode time; `None` where the field holds the demuxer's null value.
    pub dts: Option<i64>,
    /// The stored presentation time with its exact value.
    pub pts_field: ClockField,
    /// The stored decode time with its exact value.
    pub dts_field: ClockField,
    /// Duration: the sample's decode delta unless an edit or index rule replaces it.
    pub duration: i64,
    /// Packet flags as FFmpeg reports them: 1 key, 4 discard, 16 disposable.
    pub flags: u32,
}

/// A track as FFmpeg's MOV demuxer lists it as a stream, for raw video (`raw `) and
/// `tx3g` text; [`PcmStream`](crate::compat::mov::pcm::PcmStream) builds one for
/// QuickTime PCM. The source header and descriptions are kept unchanged.
#[derive(Clone, Debug)]
pub struct StreamView {
    /// `track_ID` of the `tkhd` box.
    pub track_id: u32,
    /// FFmpeg's codec name: `rawvideo`, `mov_text`, `pcm_u8` or `pcm_s16be`.
    pub codec: &'static str,
    /// FFmpeg's media type number: 0 video, 1 audio, 3 subtitle.
    pub media_type: u8,
    /// The stream time base; equal to `clock.demux_base`.
    pub time_base: TimeBase,
    /// The stream clock and why it is what it is.
    pub clock: MediaClock,
    /// Codec private data: a `tx3g` entry's bytes after its sample entry fields;
    /// empty for raw video and PCM.
    pub extradata: Bytes,
    /// ISO 639-2/T language code from `mdhd`, three lowercase letters.
    pub language: [u8; 3],
    /// Handler name from `hdlr`, ASCII, without its terminating NUL.
    pub handler_name: Bytes,
    /// Vendor code of a raw-video or PCM entry, always zero. `None` for text, and
    /// for raw video under another major brand or with an empty sample table.
    pub vendor_id: Option<[u8; 4]>,
}
impl StreamView {
    /// The stream of a track from its first description, given whether the file
    /// has fragments and its `ftyp` major brand. Raw video keeps its vendor code
    /// only under the `qt  ` major brand, whatever the compatible brands, and is
    /// `Error::Unsupported` without a major brand.
    pub fn for_source(
        header: &TrackHeader,
        table: &SampleTable,
        has_fragments: bool,
        major_brand: Option<[u8; 4]>,
    ) -> Result<Self> {
        Self::for_sample_table(header, table, has_fragments)?.with_major_brand(major_brand)
    }

    /// [`for_source`](Self::for_source) for a raw-video track whose `mdhd` timescale
    /// is zero, with the 1/1 s clock. `Error::Unsupported` unless the track has a
    /// `vide` handler, one `raw ` description, a zero timescale and samples.
    pub fn for_zero_scale_source(
        header: &TrackHeader,
        table: &SampleTable,
        major_brand: Option<[u8; 4]>,
        profile: crate::table::recovery::RunRecoveryProfile,
    ) -> Result<Self> {
        let [entry] = table.descriptions() else {
            return Err(Error::Unsupported(
                "zero-scale native catalogue description count",
            ));
        };
        if header.timescale != 0
            || header.handler != *b"vide"
            || entry.format != *b"raw "
            || table.sample_count() == 0
        {
            return Err(Error::Unsupported("zero-scale native raw-video catalogue"));
        }
        let clock = MediaClock::for_zero_scale(profile)?;
        let mut view = Self::with_projection_base(header, entry, clock.demux_base)?;
        view.clock = clock;
        view.with_major_brand(major_brand)
    }

    fn with_major_brand(mut self, major_brand: Option<[u8; 4]>) -> Result<Self> {
        if self.codec == "rawvideo" {
            match major_brand {
                Some(brand) if brand == *b"qt  " => {}
                Some(_) => self.vendor_id = None,
                None => return Err(Error::Unsupported("native major-brand context absent")),
            }
        }
        Ok(self)
    }
    /// The stream of a track from its first description. A raw-video track with an
    /// empty sample table has no vendor code when the file has fragments and is
    /// `Error::Unsupported` otherwise; `Error::Invalid` for a table without a
    /// description. Audio other than PCM is not listed.
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
    /// The stream for description `entry` alone, in the track's media time base.
    /// [`for_sample_table`](Self::for_sample_table) also handles an empty table.
    pub fn new(header: &TrackHeader, entry: &SampleDescription) -> Result<Self> {
        Self::with_projection_base(header, entry, header.time_base()?)
    }
    /// The stream for description `entry` with `demux_base` as its time base.
    /// `Error::Unsupported` for another codec or handler, a nonzero raw-video
    /// vendor, a QuickTime language code, or a handler name that is not
    /// NUL-terminated ASCII in the ISO `hdlr` layout; `Error::Limit` for a name
    /// field over 256 bytes.
    pub(crate) fn with_projection_base(
        header: &TrackHeader,
        entry: &SampleDescription,
        demux_base: TimeBase,
    ) -> Result<Self> {
        let clock = MediaClock::from_source(demux_base)?;
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
            time_base: clock.demux_base,
            clock,
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

fn demux_field(time: Option<Time>, base: TimeBase) -> Result<ClockField> {
    let ticks = time
        .ok_or(Error::Unsupported("native clock origin unresolved"))?
        .to_ticks(base, Rounding::NearestAway)?;
    ClockField::from_exact_ticks(ticks)
}
/// The demux clock a packet projection reads.
#[derive(Clone, Copy)]
enum Clock {
    /// The sample's own clocks, mapped through an edit when there is one.
    Source(Option<ProjectedEdit>),
    /// The sample's own clocks, mapped through an edit from the demux's decode start.
    DemuxEdit(ProjectedEdit, Time),
    /// A regular track's demux DTS shift.
    Regular { shift: i64 },
    /// A fragment's demux DTS, with the whole track's negative composition minimum.
    Fragment { dts: i128, minimum: i64 },
}

impl PacketProjector {
    /// A projector for a track with media time base `media_base` and
    /// `sync_entries` entries in `stss` (`None` without `stss`). With an empty
    /// `stss`, the first sample is a key packet, as the demuxer marks it.
    pub fn new(media_base: TimeBase, sync_entries: Option<u32>) -> Self {
        Self {
            media_base,
            sync_entries,
        }
    }
    /// The packet for `source`, mapped through `edit` at rate one when given, and
    /// marked discard where its presentation time lies outside the edit. It sees
    /// no other sample, so it applies no composition shift; a
    /// [`PacketCursor`](crate::compat::mov::packets::PacketCursor) does that for a
    /// whole track. `Error::Unsupported` for unknown clocks, partial sync,
    /// dependency or unknown access flags, and under an edit for a composition
    /// offset, an empty `stss` or a time the edit does not map; `Error::Invalid`
    /// for an empty edit.
    pub fn project(&self, source: &Sample, edit: Option<ProjectedEdit>) -> Result<PacketView> {
        self.project_inner(source, Clock::Source(edit))
    }
    /// [`project`](Self::project) through `edit` as the demuxer maps it: the
    /// sample's times move by the edit's movie start less `decode_start`, the decode
    /// time from which the edit's packets start. `Error::Unsupported` for an edit
    /// whose rate is not one.
    pub(crate) fn project_demux_edit(
        &self,
        source: &Sample,
        edit: ProjectedEdit,
        decode_start: Time,
    ) -> Result<PacketView> {
        if edit.entry.rate_integer != 1 || edit.entry.rate_fraction != 0 {
            return Err(Error::Unsupported("native non-unit edit point projection"));
        }
        self.project_inner(source, Clock::DemuxEdit(edit, decode_start))
    }
    /// Regular-only reference observation, after a bounded whole-wire-run scan.
    /// A raw32 signed demux interpretation is deliberately distinct from source
    /// ctts-v0 unsigned semantics. Never call this for a fragment trun offset.
    pub(crate) fn project_regular(&self, source: &Sample, shift: i64) -> Result<PacketView> {
        if source.fragment_flags.is_some() {
            return Err(Error::Invalid("regular native clock applied to fragment"));
        }
        self.project_inner(source, Clock::Regular { shift })
    }
    /// Reference fragment-only clock view, independently of source knownness.
    /// Demux controls support raw32 signed CTO in either wire version and
    /// compensation by the whole track's negative minimum. Not source semantics.
    pub(crate) fn project_fragment(
        &self,
        source: &Sample,
        demux_dts: i128,
        minimum: i64,
    ) -> Result<PacketView> {
        if source.fragment_flags.is_none() || minimum > 0 {
            return Err(Error::Invalid("fragment native clock context"));
        }
        self.project_inner(
            source,
            Clock::Fragment {
                dts: demux_dts,
                minimum,
            },
        )
    }
    fn project_inner(&self, source: &Sample, clock: Clock) -> Result<PacketView> {
        if source.partial_sync || source.dependency.is_some() || source.access == Access::Unknown {
            return Err(Error::Unsupported(
                "native dependency/recovery flag profile not frozen",
            ));
        }
        // Packet clocks stay in media-base ticks. Only an edit mapping needs
        // exact time; converting ticks to exact time and back in the same base
        // is the identity, and costs a gcd and wide divisions per packet.
        // The demux reads the composition offset as raw32 signed in either version.
        let raw_offset = || {
            source
                .composition_offset
                .map_or(0, |value| i128::from(value.raw_bits().cast_signed()))
        };
        let (pts, dts) = match clock {
            Clock::Fragment { dts, minimum } => {
                let composition = dts
                    .checked_add(raw_offset())
                    .and_then(|value| value.checked_sub(i128::from(minimum)))
                    .ok_or(Error::Overflow)?;
                (composition, dts)
            }
            Clock::Regular { shift } => {
                let ticks = source
                    .dts_ticks
                    .ok_or(Error::Unsupported("native DTS unknown"))?;
                (
                    ticks.checked_add(raw_offset()).ok_or(Error::Overflow)?,
                    ticks
                        .checked_add(i128::from(shift))
                        .ok_or(Error::Overflow)?,
                )
            }
            Clock::Source(_) | Clock::DemuxEdit(..) => source
                .cts_ticks
                .zip(source.dts_ticks)
                .ok_or(Error::Unsupported("native clock origin unresolved"))?,
        };
        let (edit, demux_edit_start) = match clock {
            Clock::Source(edit) => (edit, None),
            Clock::DemuxEdit(edit, start) => (Some(edit), Some(start)),
            Clock::Regular { .. } | Clock::Fragment { .. } => (None, None),
        };
        let mut flags = u32::from(source.is_container_sync());
        // Observed on the reference worker: explicit empty stss still makes its
        // first packet key. This does not grant source random-access/recovery proof.
        if self.sync_entries == Some(0) && source.ordinal == 1 {
            flags |= 1;
        }
        let (pts_field, dts_field) = match edit {
            None => (
                ClockField::from_exact_ticks(pts)?,
                ClockField::from_exact_ticks(dts)?,
            ),
            Some(mapping) => {
                if mapping.entry.is_empty() {
                    return Err(Error::Invalid("empty edit has no packet occurrence"));
                }
                if self.sync_entries == Some(0) || source.composition_offset.is_some() {
                    return Err(Error::Unsupported(
                        "native edited/reordered or empty-sync profile not frozen",
                    ));
                }
                let pts = Time::from_ticks(pts, self.media_base)?;
                let dts = Time::from_ticks(dts, self.media_base)?;
                let map = |time: Time| -> Result<Option<Time>> {
                    if let Some(start) = demux_edit_start {
                        Ok(Some(
                            time.checked_sub(start)?.checked_add(mapping.movie_start)?,
                        ))
                    } else {
                        mapping
                            .entry
                            .map_rate_one(time, mapping.movie_start, self.media_base)
                    }
                };
                let (mapped_pts, mapped_dts) = (map(pts)?, map(dts)?);
                let source_start =
                    Time::from_ticks(i128::from(mapping.entry.media_time), self.media_base)?;
                let source_end =
                    source_start.checked_add(mapping.entry.duration(mapping.movie_base)?)?;
                if pts < source_start || pts >= source_end {
                    flags |= 4;
                }
                (
                    demux_field(mapped_pts, self.media_base)?,
                    demux_field(mapped_dts, self.media_base)?,
                )
            }
        };
        Ok(PacketView {
            source_ordinal: source.ordinal,
            source_description: source.description_index,
            source_range: source.range.clone(),
            pts: pts_field.api_ticks(),
            dts: dts_field.api_ticks(),
            pts_field,
            dts_field,
            duration: i64::from(source.duration_ticks),
            flags,
        })
    }
}
impl PacketView {
    /// Sets the duration to the distance from this packet's decode time to
    /// `following`, the next packet's, as the demuxer's index does. Keeps it when
    /// either time is null or `following` is earlier; `Error::Overflow` when the
    /// distance does not fit `i64`.
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
