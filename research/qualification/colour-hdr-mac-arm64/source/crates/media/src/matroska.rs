//! Initial Matroska packet slice, specified by `harness/slice.json`.
//!
//! Streaming `Read` is sufficient for demuxing. Seeking additionally requires `Seek`
//! and supports validated Cues, bounded fallback scans and all-track preroll. A
//! resumable push-input API remains future work; no full Matroska claim is made.
use crate::timing::BlockTiming;
use crate::{
    Error, Limits, Result,
    ebml::{self, Header, Input},
};
use std::{
    borrow::Cow,
    collections::VecDeque,
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};

const SEGMENT: u64 = 0x18538067;
const CLUSTER: u64 = 0x1f43b675;
const INFO: u64 = 0x1549a966;
const TRACKS: u64 = 0x1654ae6b;
const CUES: u64 = 0x1c53bb6b;

#[derive(Clone, Debug)]
pub struct Track {
    packet_prefix: Vec<u8>,
    packet_decoder: Option<crate::content_encoding::Decoder>,
    /// Complete bounded TrackEntry payload, retaining extensions not interpreted yet.
    pub source_metadata: Vec<u8>,
    pub number: u64,
    pub uid: u64,
    pub kind: u64,
    pub codec: Cow<'static, str>,
    pub codec_private: Vec<u8>,
    pub name: String,
    pub language: Cow<'static, str>,
    pub default: bool,
    pub forced: bool,
    pub default_duration_ns: Option<u64>,
    pub codec_delay_ns: u64,
    pub seek_preroll_ns: u64,
    pub width: u64,
    pub height: u64,
    pub sample_rate: f64,
    pub channels: u64,
    /// Bounded static colour/HDR metadata; complete source TrackEntry remains above.
    pub colour: Option<crate::colour::Colour>,
}

#[derive(Debug)]
pub struct PacketData {
    storage: Vec<u8>,
    range: std::ops::Range<usize>,
}

impl AsRef<[u8]> for PacketData {
    fn as_ref(&self) -> &[u8] {
        &self.storage[self.range.clone()]
    }
}
impl std::ops::Deref for PacketData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}
impl PacketData {
    pub fn retained_capacity(&self) -> usize {
        self.storage.capacity()
    }

    fn decode(mut self, track: &Track, limits: Limits, cancel: &AtomicBool) -> Result<Self> {
        if track.packet_decoder.is_none() {
            return self.restore_prefix(&track.packet_prefix, limits.block_bytes);
        }
        if matches!(
            track.packet_decoder,
            Some(crate::content_encoding::Decoder::Zlib)
        ) {
            let inflate_limits = crate::zlib::InflateLimits {
                output_bytes: limits.block_bytes,
                expansion_ratio: limits.decompression_ratio,
                work: limits.decompression_work,
            };
            if let Some(range) = crate::zlib::stored_range(self.as_ref(), inflate_limits, cancel)? {
                let base = self.range.start;
                self.range = base.checked_add(range.start).ok_or(Error::Overflow)?
                    ..base.checked_add(range.end).ok_or(Error::Overflow)?;
                return Ok(self);
            }
        }
        let decoder = track
            .packet_decoder
            .as_ref()
            .ok_or(Error::Invalid("missing content decoder"))?;
        let bytes = decoder.decode(self.as_ref(), limits, limits.block_bytes, cancel)?;
        let range = 0..bytes.len();
        Ok(Self {
            storage: bytes,
            range,
        })
    }

    fn from_frame(
        frame: &[u8],
        track: &Track,
        limits: Limits,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let bytes = if let Some(decoder) = &track.packet_decoder {
            decoder.decode(frame, limits, limits.block_bytes, cancel)?
        } else {
            let total = frame
                .len()
                .checked_add(track.packet_prefix.len())
                .ok_or(Error::Overflow)?;
            if total > limits.block_bytes {
                return Err(Error::Limit("restored packet bytes"));
            }
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(total)
                .map_err(|_| Error::Limit("packet allocation"))?;
            bytes.extend_from_slice(&track.packet_prefix);
            bytes.extend_from_slice(frame);
            bytes
        };
        let range = 0..bytes.len();
        Ok(Self {
            storage: bytes,
            range,
        })
    }

    fn restore_prefix(mut self, prefix: &[u8], limit: usize) -> Result<Self> {
        if prefix.is_empty() {
            return Ok(self);
        }
        let total = prefix
            .len()
            .checked_add(self.len())
            .ok_or(Error::Overflow)?;
        if total > limit {
            return Err(Error::Limit("restored packet bytes"));
        }
        if prefix.len() <= self.range.start {
            let start = self.range.start - prefix.len();
            self.storage[start..self.range.start].copy_from_slice(prefix);
            self.range.start = start;
            return Ok(self);
        }
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(total)
            .map_err(|_| Error::Limit("packet allocation"))?;
        storage.extend_from_slice(prefix);
        storage.extend_from_slice(self.as_ref());
        Ok(Self {
            storage,
            range: 0..total,
        })
    }
}

#[derive(Debug)]
pub struct Packet {
    pub timing: BlockTiming,
    pub track: usize,
    /// Later laced frames are unknown unless the container supplies their duration.
    pub pts_ns: Option<i64>,
    /// Matroska supplies presentation time, not a codec-derived decode timestamp.
    pub dts_ns: Option<i64>,
    pub duration_ns: Option<u64>,
    pub keyframe: bool,
    pub discard_padding_ns: i64,
    pub data: PacketData,
}

#[derive(Clone, Copy, Debug)]
struct Location {
    cluster_start: u64,
    position: u64,
    cluster_end: Option<u64>,
    cluster_time: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct SeekReceipt {
    pub requested_ns: i64,
    pub actual_ns: i64,
    pub scanned_bytes: u64,
    pub used_index: bool,
    /// Decode returned packets, but suppress presentation until this source time.
    pub discard_until_ns: i64,
    pub preroll_ns: u64,
    /// The complete first Cluster is replayed. Consumers may apply one-time
    /// stream initialization; this fact comes from source position, not PTS zero.
    pub replays_from_start: bool,
}

pub struct Demuxer<R> {
    input: Input<R>,
    limits: Limits,
    tracks: Vec<Track>,
    scale: u64,
    segment_end: Option<u64>,
    cluster_end: Option<u64>,
    in_cluster: bool,
    cluster_time: Option<u64>,
    pending_header: Option<Header>,
    pending: VecDeque<Packet>,
    first_cluster: u64,
    location: Location,
    metadata_used: usize,
    failed: bool,
    opaque_metadata: Vec<(u64, Vec<u8>)>,
    segment_data: u64,
    seek_targets: Vec<(u64, u64)>,
    resolved_tracks: Option<u64>,
    cues: Vec<crate::cues::Cue>,
    loaded_metadata: Vec<u64>,
    current_cluster: u64,
}

impl<R: Read> Demuxer<R> {
    pub fn open(reader: R, limits: Limits) -> Result<Self> {
        Self::open_with(reader, limits, |_| {
            Err(Error::Unsupported(
                "Tracks must precede first Cluster on sequential input",
            ))
        })
    }

    fn open_with(
        reader: R,
        limits: Limits,
        mut resolve: impl FnMut(&mut Self) -> Result<()>,
    ) -> Result<Self> {
        let mut input = Input {
            reader,
            position: 0,
            elements_left: limits.elements,
            bytes_left: limits.scan_bytes,
        };
        let h = input.header()?.ok_or(Error::Truncated)?;
        if h.id != 0x1a45dfa3 {
            return Err(Error::Invalid("missing EBML header"));
        }
        let header = input.bytes(
            h.size.ok_or(Error::Invalid("unknown EBML header size"))?,
            limits.metadata_bytes,
        )?;
        let mut doctype = false;
        ebml::children(&header, |id, data| {
            match id {
                0x4282 => {
                    doctype = data == b"matroska" || data == b"webm";
                }
                0x4285 if ebml::uint(data)? > 4 => {
                    return Err(Error::Unsupported("DocTypeReadVersion"));
                }
                0x42f7 if ebml::uint(data)? > 1 => {
                    return Err(Error::Unsupported("EBMLReadVersion"));
                }
                0x42f2 if ebml::uint(data)? > 4 => return Err(Error::Unsupported("EBML ID width")),
                0x42f3 if ebml::uint(data)? > 8 => {
                    return Err(Error::Unsupported("EBML size width"));
                }
                _ => (),
            }
            Ok(())
        })?;
        if !doctype {
            return Err(Error::Unsupported("document type"));
        }
        let segment = loop {
            let h = input.header()?.ok_or(Error::Truncated)?;
            if h.id == SEGMENT {
                break h;
            }
            if h.id != 0xec && h.id != 0xbf {
                return Err(Error::Invalid("element before Segment"));
            }
            input.skip(
                h.size.ok_or(Error::Invalid("unknown global size"))?,
                limits.scan_bytes,
            )?;
        };
        let segment_end = end(segment)?;
        let mut this = Self {
            input,
            limits,
            tracks: Vec::new(),
            scale: 1_000_000,
            segment_end,
            cluster_end: None,
            in_cluster: false,
            cluster_time: None,
            pending_header: None,
            pending: VecDeque::new(),
            first_cluster: 0,
            location: Location {
                cluster_start: 0,
                position: 0,
                cluster_end: None,
                cluster_time: None,
            },
            metadata_used: header.len(),
            failed: false,
            opaque_metadata: Vec::new(),
            segment_data: segment.data,
            seek_targets: Vec::new(),
            resolved_tracks: None,
            cues: Vec::new(),
            loaded_metadata: Vec::new(),
            current_cluster: 0,
        };
        loop {
            let h = this.input.header()?.ok_or(Error::Truncated)?;
            this.check_parent(h, this.segment_end)?;
            if h.id == CLUSTER {
                if this.tracks.is_empty() {
                    resolve(&mut this)?;
                    if this.tracks.is_empty() {
                        return Err(Error::Invalid("missing Tracks"));
                    }
                }
                this.first_cluster = h.start;
                this.pending_header = Some(h);
                break;
            }
            this.metadata(h)?;
        }
        Ok(this)
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }
    pub fn timestamp_scale_ns(&self) -> u64 {
        self.scale
    }

    /// Bounded source Info/Tags payloads. Interpretation is not yet implemented.
    pub fn opaque_metadata(&self) -> &[(u64, Vec<u8>)] {
        &self.opaque_metadata
    }

    fn check_parent(&self, h: Header, parent: Option<u64>) -> Result<()> {
        if let Some(p) = parent {
            if h.data > p || end(h)?.is_some_and(|e| e > p) {
                return Err(Error::Invalid("element exceeds parent"));
            }
        }
        Ok(())
    }

    fn metadata(&mut self, h: Header) -> Result<()> {
        let size = h.size.ok_or(Error::Invalid("unknown metadata size"))?;
        if self.loaded_metadata.contains(&h.start)
            || (h.id == TRACKS && self.resolved_tracks == Some(h.start))
        {
            return self.input.skip(size, self.limits.scan_bytes);
        }
        match h.id {
            INFO | TRACKS | CUES | 0x1254c367 | 0x114d9b74 => {
                let remaining = self
                    .limits
                    .metadata_bytes
                    .checked_sub(self.metadata_used)
                    .ok_or(Error::Limit("metadata"))?;
                let data = self.input.bytes(size, remaining)?;
                self.metadata_used = self
                    .metadata_used
                    .checked_add(data.len())
                    .ok_or(Error::Overflow)?;
                if self.loaded_metadata.len() >= 256 {
                    return Err(Error::Limit("metadata locations"));
                }
                if h.id == CUES {
                    if !self.cues.is_empty() {
                        return Err(Error::Invalid("duplicate Cues"));
                    }
                    self.cues = crate::cues::parse(
                        &data,
                        self.segment_data,
                        self.segment_end,
                        self.limits.index_entries,
                    )?;
                } else if h.id == INFO {
                    ebml::children(&data, |id, b| {
                        if id == 0x2ad7b1 {
                            self.scale = ebml::uint(b)?;
                        }
                        Ok(())
                    })?;
                    if self.scale == 0 {
                        return Err(Error::Invalid("zero TimestampScale"));
                    }
                    self.opaque_metadata.push((h.id, data));
                } else if h.id == TRACKS {
                    ebml::children(&data, |id, b| {
                        if id == 0xae {
                            if self.tracks.len() >= self.limits.tracks {
                                return Err(Error::Limit("track count"));
                            }
                            let private_budget = self
                                .limits
                                .metadata_bytes
                                .checked_sub(self.metadata_used)
                                .ok_or(Error::Limit("decoded private metadata"))?;
                            let (t, decoded_private_bytes) =
                                parse_track(b, self.limits, private_budget)?;
                            self.metadata_used = self
                                .metadata_used
                                .checked_add(decoded_private_bytes)
                                .ok_or(Error::Overflow)?;
                            if self.tracks.iter().any(|p| p.number == t.number) {
                                return Err(Error::Invalid("duplicate track number"));
                            }
                            self.tracks.push(t);
                        }
                        Ok(())
                    })?;
                } else if h.id == 0x114d9b74 {
                    ebml::children(&data, |id, entry| {
                        if id != 0x4dbb {
                            return Ok(());
                        }
                        let mut identifier = None;
                        let mut position = None;
                        ebml::children(entry, |id, b| {
                            match id {
                                0x53ab => identifier = Some(ebml::uint(b)?),
                                0x53ac => position = Some(ebml::uint(b)?),
                                _ => (),
                            };
                            Ok(())
                        })?;
                        if let (Some(id), Some(relative)) = (identifier, position) {
                            if matches!(id, INFO | TRACKS | CUES | 0x114d9b74) {
                                if self.seek_targets.len() >= 256 {
                                    return Err(Error::Limit("metadata seek targets"));
                                }
                                self.seek_targets.push((
                                    id,
                                    self.segment_data
                                        .checked_add(relative)
                                        .ok_or(Error::Overflow)?,
                                ));
                            }
                        } else {
                            return Err(Error::Invalid("incomplete Seek entry"));
                        }
                        Ok(())
                    })?;
                } else {
                    self.opaque_metadata.push((h.id, data));
                }
                // Only successful metadata parsing may suppress a later retry.
                self.loaded_metadata.push(h.start);
            }
            // These are deliberately unimplemented semantics, not silent metadata loss.
            0x1941a469 | 0x1043a770 => return Err(Error::Unsupported("attachments/chapters")),
            _ => self.input.skip(size, self.limits.scan_bytes)?,
        }
        Ok(())
    }

    /// Stream one owned compressed packet. A hard element limit bounds empty/irrelevant scans.
    /// Cancellation is checked between elements; a blocking caller read remains caller-owned.
    pub fn next_packet(&mut self, cancel: &AtomicBool) -> Result<Option<Packet>> {
        if self.failed {
            return Err(Error::Invalid(
                "reader requires explicit seek/reset after error",
            ));
        }
        self.input.bytes_left = self.limits.scan_bytes;
        let result = self.read_packet(cancel);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn read_packet(&mut self, cancel: &AtomicBool) -> Result<Option<Packet>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(p) = self.pending.pop_front() {
                return Ok(Some(p));
            }
            if self.in_cluster && self.cluster_end == Some(self.input.position) {
                self.in_cluster = false;
            }
            if self.segment_end == Some(self.input.position) {
                return Ok(None);
            }
            let h = match self.pending_header.take() {
                Some(h) => h,
                None => match self.input.header()? {
                    Some(h) => h,
                    None if self.segment_end.is_some_and(|e| self.input.position < e)
                        || self.cluster_end.is_some_and(|e| self.input.position < e) =>
                    {
                        return Err(Error::Truncated);
                    }
                    None => return Ok(None),
                },
            };
            self.check_parent(h, self.segment_end)?;
            if h.id == CLUSTER {
                if self.in_cluster && self.cluster_end.is_some() {
                    return Err(Error::Invalid("nested Cluster"));
                }
                self.in_cluster = true;
                self.current_cluster = h.start;
                self.cluster_end = end(h)?;
                self.cluster_time = None;
                continue;
            }
            if self.in_cluster && is_level_one(h.id) {
                if self.cluster_end.is_some() {
                    return Err(Error::Invalid("level-one element in finite Cluster"));
                }
                self.in_cluster = false;
            }
            if !self.in_cluster {
                self.metadata(h)?;
                continue;
            }
            self.check_parent(h, self.cluster_end)?;
            let size = h.size.ok_or(Error::Invalid("unknown Cluster child size"))?;
            match h.id {
                0xe7 => {
                    self.cluster_time = Some(ebml::uint(&self.input.bytes(size, 8)?)?);
                }
                0xa3 | 0xa0 => {
                    let b = self.input.bytes(size, self.limits.block_bytes)?;
                    self.location = Location {
                        cluster_start: self.current_cluster,
                        position: h.start,
                        cluster_end: self.cluster_end,
                        cluster_time: self.cluster_time,
                    };
                    if let Some(packet) = self.block(b, h.id == 0xa3, cancel)? {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        return Ok(Some(packet));
                    }
                }
                _ => self.input.skip(size, self.limits.scan_bytes)?,
            }
        }
    }

    fn block(
        &mut self,
        bytes: Vec<u8>,
        simple: bool,
        cancel: &AtomicBool,
    ) -> Result<Option<Packet>> {
        let mut block = None;
        let mut duration = None;
        let mut reference = false;
        let mut discard = 0;
        if simple {
            block = Some(0..bytes.len());
        } else {
            ebml::child_ranges(&bytes, |id, range| {
                let b = &bytes[range.clone()];
                match id {
                    0xa1 => {
                        if block.replace(range).is_some() {
                            return Err(Error::Invalid("duplicate Block"));
                        }
                    }
                    0x9b => duration = Some(ebml::uint(b)?),
                    0xfb => reference = true,
                    0x75a2 => discard = ebml::sint(b)?,
                    0x75a1 | 0xa4 => return Err(Error::Unsupported("Block additions/CodecState")),
                    _ => (),
                }
                Ok(())
            })?;
        }
        let block = block.ok_or(Error::Invalid("BlockGroup without Block"))?;
        let b = &bytes[block.clone()];
        let (number, w, _) = ebml::vint(b, false)?;
        let header = b.get(w..w + 3).ok_or(Error::Truncated)?;
        let offset = i16::from_be_bytes([header[0], header[1]]);
        let flags = header[2];
        let track = self
            .tracks
            .iter()
            .position(|t| t.number == number)
            .ok_or(Error::Invalid("unregistered block track"))?;
        let t = &self.tracks[track];
        let ticks = i128::from(
            self.cluster_time
                .ok_or(Error::Invalid("Block before Cluster Timestamp"))?,
        ) + i128::from(offset);
        let keyframe = if simple {
            flags & 0x80 != 0
        } else {
            !reference
        };
        let base_timing = BlockTiming {
            timestamp_ticks: ticks,
            scale_ns: self.scale,
            codec_delay_ns: t.codec_delay_ns,
            default_duration_ns: t.default_duration_ns,
            duration_ticks: duration,
            frame: 0,
            frames: 1,
        };
        if (flags >> 1) & 3 == 0 {
            let range = block.start + w + 3..block.end;
            return Ok(Some(packet(
                track,
                base_timing,
                keyframe,
                discard,
                PacketData {
                    storage: bytes,
                    range,
                }
                .decode(t, self.limits, cancel)?,
            )?));
        }
        let frames = lace(&b[w + 3..], (flags >> 1) & 3)?;
        let count = frames.len() as u16;
        self.pending
            .try_reserve_exact(frames.len().saturating_sub(1))
            .map_err(|_| Error::Limit("pending packets"))?;
        let mut first = None;
        let mut decoded_bytes = 0_usize;
        for (n, frame) in frames.into_iter().enumerate() {
            let timing = BlockTiming {
                frame: n as u16,
                frames: count,
                ..base_timing
            };
            let packet = packet(
                track,
                timing,
                keyframe,
                discard,
                PacketData::from_frame(
                    frame,
                    t,
                    Limits {
                        block_bytes: self
                            .limits
                            .block_bytes
                            .checked_sub(decoded_bytes)
                            .ok_or(Error::Limit("decoded laced block bytes"))?,
                        ..self.limits
                    },
                    cancel,
                )?,
            )?;
            decoded_bytes = decoded_bytes
                .checked_add(packet.data.retained_capacity())
                .ok_or(Error::Overflow)?;
            if decoded_bytes > self.limits.block_bytes {
                return Err(Error::Limit("decoded laced block capacity"));
            }
            if n == 0 {
                first = Some(packet);
            } else {
                self.pending.push_back(packet);
            }
        }
        Ok(first)
    }
}

fn packet(
    track: usize,
    timing: BlockTiming,
    keyframe: bool,
    discard_padding_ns: i64,
    data: PacketData,
) -> Result<Packet> {
    let source = timing.source()?;
    Ok(Packet {
        timing,
        track,
        pts_ns: source.pts.map(|p| p.rounded_ns()).transpose()?,
        dts_ns: None,
        duration_ns: source
            .duration
            .map(|d| d.rounded_duration_ns())
            .transpose()?,
        keyframe,
        discard_padding_ns,
        data,
    })
}

impl<R: Read + Seek> Demuxer<R> {
    /// Resolve bounded, possibly chained SeekHeads when Tracks follows Cluster data.
    pub fn open_seekable(reader: R, limits: Limits) -> Result<Self> {
        Self::open_with(reader, limits, |demux| demux.resolve_headers())
    }

    fn resolve_headers(&mut self) -> Result<()> {
        let saved = self.input.position;
        let mut seen = Vec::new();
        let mut cursor = 0;
        while cursor < self.seek_targets.len() {
            let (id, position) = self.seek_targets[cursor];
            cursor += 1;
            if seen.contains(&position) {
                continue;
            }
            seen.push(position);
            if self.segment_end.is_some_and(|end| position >= end) {
                return Err(Error::Invalid("metadata seek outside Segment"));
            }
            self.input.reader.seek(SeekFrom::Start(position))?;
            self.input.position = position;
            let header = self.input.header()?.ok_or(Error::Truncated)?;
            if header.id != id {
                return Err(Error::Invalid("SeekID disagrees with target"));
            }
            self.check_parent(header, self.segment_end)?;
            if id != TRACKS || self.tracks.is_empty() {
                self.metadata(header)?;
                if id == TRACKS {
                    self.resolved_tracks = Some(header.start);
                }
            }
        }
        self.input.reader.seek(SeekFrom::Start(saved))?;
        self.input.position = saved;
        Ok(())
    }

    /// Bounded previous-keyframe scan. No fabricated landing on a non-keyframe or EOF.
    /// On failure callers must explicitly seek/reset again before consuming more packets.
    pub fn seek_previous(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
    ) -> Result<SeekReceipt> {
        self.failed = true;
        self.pending.clear();
        let result = self.seek_dispatch(track, requested_ns, cancel, false);
        if result.is_ok() {
            self.failed = false;
        }
        result
    }

    /// Seek far enough back for every declared track's SeekPreRoll. All interleaved
    /// packets from the landing Cluster are returned, including packets to decode
    /// without presentation before `discard_until_ns`.
    pub fn seek_for_playback(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
    ) -> Result<SeekReceipt> {
        self.failed = true;
        self.pending.clear();
        let result = self.seek_dispatch(track, requested_ns, cancel, true);
        if result.is_ok() {
            self.failed = false;
        }
        result
    }

    fn seek_dispatch(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
        preroll: bool,
    ) -> Result<SeekReceipt> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.tracks.get(track).is_none_or(|t| t.kind != 1) {
            return Err(Error::Unsupported("seek requires video track"));
        }
        self.input.bytes_left = self.limits.scan_bytes;
        self.input.elements_left = self.limits.elements;
        self.resolve_headers()?;
        let metadata_scanned = self.limits.scan_bytes - self.input.bytes_left;
        let roll = if preroll {
            self.tracks
                .iter()
                .map(|t| t.seek_preroll_ns)
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        let target = i64::try_from(i128::from(requested_ns) - i128::from(roll))
            .map_err(|_| Error::Overflow)?;
        let number = self.tracks[track].number;
        let delay = self.tracks[track].codec_delay_ns;
        let scale = self.scale;
        // Compare in tick space so unselected, extreme CueTimes cannot overflow.
        let target_ticks = (i128::from(target) + i128::from(delay)).div_euclid(i128::from(scale));
        let requested_ticks =
            (i128::from(requested_ns) + i128::from(delay)).div_euclid(i128::from(scale));
        let first = self.cues.partition_point(|cue| cue.track < number);
        let end = self.cues.partition_point(|cue| cue.track <= number);
        let track_cues = &self.cues[first..end];
        let before = track_cues.partition_point(|cue| i128::from(cue.ticks) <= target_ticks);
        let cue = before
            .checked_sub(1)
            .and_then(|index| track_cues.get(index))
            .or_else(|| {
                track_cues
                    .first()
                    .filter(|cue| preroll && i128::from(cue.ticks) <= requested_ticks)
            })
            .copied();
        let tail_gap = preroll
            && before == track_cues.len()
            && cue.is_some_and(|cue| i128::from(cue.ticks) < target_ticks);
        let mut receipt = if let Some(cue) = cue {
            let validated = self.seek_cue(track, cue, cancel, preroll)?;
            if preroll && tail_gap {
                // The last stored cue may precede uncued RAPs in the file tail.
                let mut tail = self.seek_scan_at(track, target, cancel, cue.cluster, None)?;
                self.rewind_cluster(self.location.cluster_start)?;
                tail.used_index = true;
                tail.scanned_bytes = tail
                    .scanned_bytes
                    .checked_add(validated.scanned_bytes)
                    .ok_or(Error::Overflow)?;
                tail
            } else if preroll {
                validated
            } else {
                let stop = self
                    .cues
                    .iter()
                    .filter(|c| {
                        c.track == number
                            && i128::from(c.ticks) > target_ticks
                            && c.cluster > cue.cluster
                    })
                    .map(|c| c.cluster)
                    .min();
                let mut scanned = self.seek_scan_at(track, target, cancel, cue.cluster, stop)?;
                scanned.used_index = true;
                scanned.scanned_bytes = scanned
                    .scanned_bytes
                    .checked_add(validated.scanned_bytes)
                    .ok_or(Error::Overflow)?;
                scanned
            }
        } else {
            let result = self.seek_scan(track, target, cancel);
            // A preroll request before the beginning may use the first available RAP.
            let mut result = match result {
                Err(Error::Unsupported("no previous random-access block"))
                    if preroll && target < 0 =>
                {
                    self.seek_scan(track, 0, cancel)
                }
                value => value,
            }?;
            if preroll {
                self.rewind_cluster(self.location.cluster_start)?;
            }
            result.used_index = false;
            result
        };
        receipt.scanned_bytes = receipt
            .scanned_bytes
            .checked_add(metadata_scanned)
            .ok_or(Error::Overflow)?;
        receipt.requested_ns = requested_ns;
        receipt.discard_until_ns = requested_ns;
        receipt.preroll_ns = roll;
        receipt.replays_from_start = preroll && self.location.cluster_start == self.first_cluster;
        Ok(receipt)
    }

    fn rewind_cluster(&mut self, position: u64) -> Result<()> {
        self.input.reader.seek(SeekFrom::Start(position))?;
        self.input.position = position;
        self.input.elements_left = self.limits.elements;
        self.input.bytes_left = self.limits.scan_bytes;
        self.in_cluster = false;
        self.cluster_end = None;
        self.cluster_time = None;
        self.pending.clear();
        self.pending_header = None;
        Ok(())
    }

    fn seek_cue(
        &mut self,
        track: usize,
        cue: crate::cues::Cue,
        cancel: &AtomicBool,
        whole_cluster: bool,
    ) -> Result<SeekReceipt> {
        if cue.codec_state != 0 {
            return Err(Error::Unsupported("cue CodecState"));
        }
        self.rewind_cluster(cue.cluster)?;
        let header = self.input.header()?.ok_or(Error::Truncated)?;
        if header.id != CLUSTER {
            return Err(Error::Invalid("cue target is not Cluster"));
        }
        self.check_parent(header, self.segment_end)?;
        let cluster_end = end(header)?;
        let relative = cue
            .relative
            .map(|relative| header.data.checked_add(relative).ok_or(Error::Overflow))
            .transpose()?;
        if relative.is_some_and(|p| cluster_end.is_some_and(|end| p >= end)) {
            return Err(Error::Invalid("cue relative position outside Cluster"));
        }
        self.pending_header = Some(header);
        let wanted = i64::try_from(
            i128::from(cue.ticks)
                .checked_mul(i128::from(self.scale))
                .and_then(|value| value.checked_sub(i128::from(self.tracks[track].codec_delay_ns)))
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)?;
        let mut previous = None;
        let mut blocks = 0_u64;
        let location = loop {
            if cluster_end == Some(self.input.position) && self.pending.is_empty() {
                return Err(Error::Invalid(
                    "cue does not reference a random-access block",
                ));
            }
            let p = self
                .read_packet(cancel)?
                .ok_or(Error::Invalid("cue beyond packet data"))?;
            if self.location.cluster_start != cue.cluster {
                return Err(Error::Invalid("cue timestamp absent from Cluster"));
            }
            let first = previous != Some(self.location.position);
            if first {
                blocks += 1;
                previous = Some(self.location.position);
            }
            if first && p.track == track && p.keyframe && p.pts_ns == Some(wanted) {
                if relative.is_some_and(|position| position != self.location.position)
                    || cue.block.is_some_and(|block| block != blocks)
                {
                    return Err(Error::Invalid(
                        "cue position disagrees with referenced block",
                    ));
                }
                break self.location;
            }
        };
        let scanned_bytes = self.limits.scan_bytes - self.input.bytes_left;
        if whole_cluster {
            self.rewind_cluster(cue.cluster)?;
        } else {
            self.restore_location(location)?;
        }
        Ok(SeekReceipt {
            requested_ns: wanted,
            actual_ns: wanted,
            scanned_bytes,
            used_index: true,
            discard_until_ns: wanted,
            preroll_ns: 0,
            replays_from_start: false,
        })
    }

    fn restore_location(&mut self, location: Location) -> Result<()> {
        self.input.reader.seek(SeekFrom::Start(location.position))?;
        self.input.position = location.position;
        self.input.elements_left = self.limits.elements;
        self.input.bytes_left = self.limits.scan_bytes;
        self.in_cluster = true;
        self.current_cluster = location.cluster_start;
        self.cluster_end = location.cluster_end;
        self.cluster_time = location.cluster_time;
        self.pending.clear();
        self.pending_header = None;
        self.location = location;
        Ok(())
    }

    fn seek_scan(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
    ) -> Result<SeekReceipt> {
        self.seek_scan_at(track, requested_ns, cancel, self.first_cluster, None)
    }

    fn seek_scan_at(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
        start: u64,
        stop: Option<u64>,
    ) -> Result<SeekReceipt> {
        if self.tracks.get(track).is_none_or(|t| t.kind != 1) {
            return Err(Error::Unsupported("seek requires video track"));
        }
        self.input.reader.seek(SeekFrom::Start(start))?;
        self.input.position = start;
        self.input.elements_left = self.limits.elements;
        self.in_cluster = false;
        self.cluster_end = None;
        self.pending_header = None;
        self.pending.clear();
        let mut best = None;
        let mut previous_block = None;
        self.input.bytes_left = self.limits.scan_bytes;
        loop {
            if self.pending.is_empty() && stop.is_some_and(|stop| self.input.position >= stop) {
                break;
            }
            let Some(p) = self.read_packet(cancel)? else {
                break;
            };
            let first_in_block = previous_block != Some(self.location.position);
            previous_block = Some(self.location.position);
            if self.input.position - start > self.limits.scan_bytes {
                return Err(Error::Limit("seek scan bytes"));
            }
            if let Some(pts) = p.pts_ns {
                if first_in_block
                    && p.track == track
                    && p.keyframe
                    && pts <= requested_ns
                    && best.is_none_or(|(time, _)| pts > time)
                {
                    best = Some((pts, self.location));
                }
            }
        }
        let scanned_bytes = self.input.position - start;
        let (actual_ns, loc) = best.ok_or(Error::Unsupported("no previous random-access block"))?;
        self.restore_location(loc)?;
        Ok(SeekReceipt {
            requested_ns,
            actual_ns,
            scanned_bytes,
            used_index: false,
            discard_until_ns: requested_ns,
            preroll_ns: 0,
            replays_from_start: false,
        })
    }
}

fn end(h: Header) -> Result<Option<u64>> {
    h.size
        .map(|s| h.data.checked_add(s).ok_or(Error::Overflow))
        .transpose()
}

fn is_level_one(id: u64) -> bool {
    matches!(
        id,
        INFO | TRACKS | 0x114d9b74 | 0x1c53bb6b | 0x1254c367 | 0x1043a770 | 0x1941a469
    )
}

fn string(bytes: &[u8]) -> Result<String> {
    Ok(std::str::from_utf8(bytes)
        .map_err(|_| Error::Invalid("UTF-8 metadata"))?
        .trim_end_matches('\0')
        .to_owned())
}

fn metadata_text(bytes: &[u8]) -> Result<Cow<'static, str>> {
    let known = match bytes {
        b"V_MPEG4/ISO/AVC" => "V_MPEG4/ISO/AVC",
        b"V_MPEGH/ISO/HEVC" => "V_MPEGH/ISO/HEVC",
        b"A_PCM/INT/LIT" => "A_PCM/INT/LIT",
        b"A_AAC" => "A_AAC",
        b"A_AC3" => "A_AC3",
        b"A_EAC3" => "A_EAC3",
        b"A_VORBIS" => "A_VORBIS",
        b"S_TEXT/UTF8" => "S_TEXT/UTF8",
        b"S_TEXT/ASS" => "S_TEXT/ASS",
        b"S_VOBSUB" => "S_VOBSUB",
        b"eng" => "eng",
        b"und" => "und",
        _ => return Ok(Cow::Owned(string(bytes)?)),
    };
    Ok(Cow::Borrowed(known))
}

fn parse_track(data: &[u8], limits: Limits, private_budget: usize) -> Result<(Track, usize)> {
    let metadata_limit = limits.metadata_bytes;
    let mut t = Track {
        packet_prefix: Vec::new(),
        packet_decoder: None,
        source_metadata: data.to_vec(),
        number: 0,
        uid: 0,
        kind: 0,
        codec: Cow::Borrowed(""),
        codec_private: Vec::new(),
        name: String::new(),
        language: "eng".into(),
        default: true,
        forced: false,
        default_duration_ns: None,
        codec_delay_ns: 0,
        seek_preroll_ns: 0,
        width: 0,
        height: 0,
        sample_rate: 8000.0,
        channels: 1,
        colour: None,
    };
    let mut prefixes = crate::content_encoding::Encodings::default();
    let mut encoded = false;
    ebml::children(data, |id, b| {
        match id {
            0xd7 => t.number = ebml::uint(b)?,
            0x73c5 => t.uid = ebml::uint(b)?,
            0x83 => t.kind = ebml::uint(b)?,
            0x86 => t.codec = metadata_text(b)?,
            0x63a2 => t.codec_private = b.to_vec(),
            0x536e => t.name = string(b)?,
            0x22b59c | 0x22b59d => t.language = metadata_text(b)?,
            0x88 => t.default = ebml::uint(b)? != 0,
            0x55aa => t.forced = ebml::uint(b)? != 0,
            0x23e383 => t.default_duration_ns = Some(ebml::uint(b)?),
            0x56aa => t.codec_delay_ns = ebml::uint(b)?,
            0x56bb => t.seek_preroll_ns = ebml::uint(b)?,
            0x23314f if b != 1_f64.to_be_bytes() && b != 1_f32.to_be_bytes() => {
                return Err(Error::Unsupported("TrackTimestampScale"));
            }
            0x6d80 => {
                if encoded {
                    return Err(Error::Invalid("duplicate ContentEncodings"));
                }
                prefixes = crate::content_encoding::parse(b, metadata_limit)?;
                encoded = true;
            }
            0xe0 => ebml::children(b, |id, b| {
                match id {
                    0xb0 => t.width = ebml::uint(b)?,
                    0xba => t.height = ebml::uint(b)?,
                    0x55b0 => {
                        if t.colour.is_some() {
                            return Err(Error::Invalid("duplicate Colour"));
                        }
                        t.colour = Some(crate::colour::parse(b)?);
                    }
                    _ => (),
                };
                Ok(())
            })?,
            0xe1 => ebml::children(b, |id, b| {
                match id {
                    0x9f => t.channels = ebml::uint(b)?,
                    0xb5 => {
                        t.sample_rate = match b.len() {
                            4 => f32::from_be_bytes(
                                b.try_into().map_err(|_| Error::Invalid("float"))?,
                            ) as f64,
                            8 => f64::from_be_bytes(
                                b.try_into().map_err(|_| Error::Invalid("float"))?,
                            ),
                            _ => return Err(Error::Invalid("sampling frequency width")),
                        };
                        if !t.sample_rate.is_finite() || t.sample_rate <= 0.0 {
                            return Err(Error::Invalid("sampling frequency"));
                        }
                    }
                    _ => (),
                };
                Ok(())
            })?,
            _ => (),
        }
        Ok(())
    })?;
    let compressed_private = prefixes
        .private
        .as_ref()
        .is_some_and(crate::content_encoding::Decoder::contains_zlib);
    let private_limit = if compressed_private {
        metadata_limit.min(private_budget)
    } else {
        metadata_limit
    };
    if let Some(decoder) = prefixes.private {
        t.codec_private = decoder.decode(
            &t.codec_private,
            limits,
            private_limit,
            &AtomicBool::new(false),
        )?;
    }
    t.packet_prefix = prefixes.frame_prefix;
    t.packet_decoder = prefixes.frame;
    if t.number == 0 || t.uid == 0 || t.kind == 0 || t.codec.is_empty() {
        return Err(Error::Invalid("missing track fields"));
    }
    let private_bytes = if compressed_private {
        t.codec_private.capacity()
    } else {
        0
    };
    if private_bytes > private_budget {
        return Err(Error::Limit("decoded private metadata capacity"));
    }
    Ok((t, private_bytes))
}

/// Decode Xiph, fixed or EBML lacing with exact payload accounting and checked deltas.
fn lace(data: &[u8], mode: u8) -> Result<Vec<&[u8]>> {
    if mode == 0 {
        return Ok(vec![data]);
    }
    let count = usize::from(*data.first().ok_or(Error::Truncated)?) + 1;
    if count < 2 {
        return Err(Error::Invalid("lacing requires multiple frames"));
    }
    let mut at = 1;
    let mut sizes = Vec::with_capacity(count);
    if mode == 2 {
        if (data.len() - 1) % count != 0 {
            return Err(Error::Invalid("fixed lace remainder"));
        }
        sizes.resize(count, (data.len() - 1) / count);
    } else {
        for n in 0..count - 1 {
            let size = if mode == 1 {
                let mut total = 0_usize;
                loop {
                    let b = *data.get(at).ok_or(Error::Truncated)?;
                    at += 1;
                    total = total.checked_add(b as usize).ok_or(Error::Overflow)?;
                    if b != 255 {
                        break;
                    }
                }
                total
            } else if mode == 3 {
                let (value, width, _) = ebml::vint(data.get(at..).ok_or(Error::Truncated)?, false)?;
                at += width;
                if n == 0 {
                    usize::try_from(value).map_err(|_| Error::Overflow)?
                } else {
                    let delta = i128::from(value) - ((1_i128 << (7 * width - 1)) - 1);
                    usize::try_from(sizes[n - 1] as i128 + delta)
                        .map_err(|_| Error::Invalid("negative/overflow lace size"))?
                }
            } else {
                return Err(Error::Invalid("lace mode"));
            };
            sizes.push(size);
        }
        let used = sizes
            .iter()
            .try_fold(0_usize, |v, n| v.checked_add(*n).ok_or(Error::Overflow))?;
        sizes.push(
            data.len()
                .checked_sub(at)
                .and_then(|n| n.checked_sub(used))
                .ok_or(Error::Invalid("lace exceeds block"))?,
        );
    }
    let mut frames = Vec::with_capacity(count);
    for size in sizes {
        let end = at.checked_add(size).ok_or(Error::Overflow)?;
        frames.push(data.get(at..end).ok_or(Error::Truncated)?);
        at = end;
    }
    if at != data.len() {
        return Err(Error::Invalid("unconsumed lace bytes"));
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_lacing_schemes_and_malformed_lengths() {
        assert_eq!(
            lace(&[1, 1, 4, 5, 6], 1).unwrap(),
            vec![&[4][..], &[5, 6][..]]
        );
        assert_eq!(
            lace(&[1, 4, 5, 6, 7], 2).unwrap(),
            vec![&[4, 5][..], &[6, 7][..]]
        );
        assert_eq!(
            lace(&[2, 0x81, 0xc0, 4, 5, 6, 7, 8, 9], 3).unwrap(),
            vec![&[4][..], &[5, 6][..], &[7, 8, 9][..]]
        );
        for b in [&[][..], &[0][..], &[1, 255][..], &[2, 0x81, 0x80][..]] {
            assert!(lace(b, 3).is_err());
        }
        assert!(lace(&[1, 0, 1, 2], 2).is_err());
    }
}
