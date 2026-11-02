//! Initial Matroska packet slice, specified by `harness/slice.json`.
//!
//! Streaming `Read` is sufficient for demuxing. Seeking additionally requires `Seek`
//! and uses a bounded scan for a previous video random-access block. Cues and a
//! resumable push-input API are future work; no full Matroska claim is made.
use crate::timing::BlockTiming;
use crate::{
    Error, Limits, Result,
    ebml::{self, Header, Input},
};
use std::{
    collections::VecDeque,
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};

const SEGMENT: u64 = 0x18538067;
const CLUSTER: u64 = 0x1f43b675;
const INFO: u64 = 0x1549a966;
const TRACKS: u64 = 0x1654ae6b;

#[derive(Clone, Debug)]
pub struct Track {
    /// Complete bounded TrackEntry payload, retaining extensions not interpreted yet.
    pub source_metadata: Vec<u8>,
    pub number: u64,
    pub uid: u64,
    pub kind: u64,
    pub codec: String,
    pub codec_private: Vec<u8>,
    pub name: String,
    pub language: String,
    pub default: bool,
    pub forced: bool,
    pub default_duration_ns: Option<u64>,
    pub codec_delay_ns: u64,
    pub seek_preroll_ns: u64,
    pub width: u64,
    pub height: u64,
    pub sample_rate: f64,
    pub channels: u64,
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
    position: u64,
    cluster_end: Option<u64>,
    cluster_time: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct SeekReceipt {
    pub requested_ns: i64,
    pub actual_ns: i64,
    pub scanned_bytes: u64,
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
}

impl<R: Read> Demuxer<R> {
    pub fn open(reader: R, limits: Limits) -> Result<Self> {
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
                position: 0,
                cluster_end: None,
                cluster_time: None,
            },
            metadata_used: header.len(),
            failed: false,
            opaque_metadata: Vec::new(),
        };
        loop {
            let h = this.input.header()?.ok_or(Error::Truncated)?;
            this.check_parent(h, this.segment_end)?;
            if h.id == CLUSTER {
                if this.tracks.is_empty() {
                    return Err(Error::Unsupported("Tracks must precede first Cluster"));
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
        match h.id {
            INFO | TRACKS | 0x1254c367 => {
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
                if h.id == INFO {
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
                            let t = parse_track(b)?;
                            if self.tracks.iter().any(|p| p.number == t.number) {
                                return Err(Error::Invalid("duplicate track number"));
                            }
                            self.tracks.push(t);
                        }
                        Ok(())
                    })?;
                } else {
                    self.opaque_metadata.push((h.id, data));
                }
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
                        position: h.start,
                        cluster_end: self.cluster_end,
                        cluster_time: self.cluster_time,
                    };
                    self.block(b, h.id == 0xa3)?;
                }
                _ => self.input.skip(size, self.limits.scan_bytes)?,
            }
        }
    }

    fn block(&mut self, bytes: Vec<u8>, simple: bool) -> Result<()> {
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
            self.pending.push_back(packet(
                track,
                base_timing,
                keyframe,
                discard,
                PacketData {
                    storage: bytes,
                    range,
                },
            )?);
            return Ok(());
        }
        let frames = lace(&b[w + 3..], (flags >> 1) & 3)?;
        let count = frames.len() as u16;
        for (n, frame) in frames.into_iter().enumerate() {
            let timing = BlockTiming {
                frame: n as u16,
                frames: count,
                ..base_timing
            };
            let mut data = Vec::new();
            data.try_reserve_exact(frame.len())
                .map_err(|_| Error::Limit("packet allocation"))?;
            data.extend_from_slice(frame);
            let range = 0..data.len();
            self.pending.push_back(packet(
                track,
                timing,
                keyframe,
                discard,
                PacketData {
                    storage: data,
                    range,
                },
            )?);
        }
        Ok(())
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
    /// Bounded previous-keyframe scan. No fabricated landing on a non-keyframe or EOF.
    /// On failure callers must explicitly seek/reset again before consuming more packets.
    pub fn seek_previous(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
    ) -> Result<SeekReceipt> {
        self.failed = true;
        let result = self.seek_scan(track, requested_ns, cancel);
        if result.is_ok() {
            self.failed = false;
        }
        result
    }

    fn seek_scan(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
    ) -> Result<SeekReceipt> {
        if self.tracks.get(track).is_none_or(|t| t.kind != 1) {
            return Err(Error::Unsupported("seek requires video track"));
        }
        self.input
            .reader
            .seek(SeekFrom::Start(self.first_cluster))?;
        self.input.position = self.first_cluster;
        self.input.elements_left = self.limits.elements;
        self.in_cluster = false;
        self.cluster_end = None;
        self.pending_header = None;
        self.pending.clear();
        let mut best = None;
        let mut previous_block = None;
        self.input.bytes_left = self.limits.scan_bytes;
        while let Some(p) = self.read_packet(cancel)? {
            let first_in_block = previous_block != Some(self.location.position);
            previous_block = Some(self.location.position);
            if self.input.position - self.first_cluster > self.limits.scan_bytes {
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
        let scanned_bytes = self.input.position - self.first_cluster;
        let (actual_ns, loc) = best.ok_or(Error::Unsupported("no previous random-access block"))?;
        self.input.reader.seek(SeekFrom::Start(loc.position))?;
        self.input.position = loc.position;
        self.input.elements_left = self.limits.elements;
        self.in_cluster = true;
        self.cluster_end = loc.cluster_end;
        self.cluster_time = loc.cluster_time;
        self.pending.clear();
        self.pending_header = None;
        Ok(SeekReceipt {
            requested_ns,
            actual_ns,
            scanned_bytes,
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

fn parse_track(data: &[u8]) -> Result<Track> {
    let mut t = Track {
        source_metadata: data.to_vec(),
        number: 0,
        uid: 0,
        kind: 0,
        codec: String::new(),
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
    };
    ebml::children(data, |id, b| {
        match id {
            0xd7 => t.number = ebml::uint(b)?,
            0x73c5 => t.uid = ebml::uint(b)?,
            0x83 => t.kind = ebml::uint(b)?,
            0x86 => t.codec = string(b)?,
            0x63a2 => t.codec_private = b.to_vec(),
            0x536e => t.name = string(b)?,
            0x22b59c | 0x22b59d => t.language = string(b)?,
            0x88 => t.default = ebml::uint(b)? != 0,
            0x55aa => t.forced = ebml::uint(b)? != 0,
            0x23e383 => t.default_duration_ns = Some(ebml::uint(b)?),
            0x56aa => t.codec_delay_ns = ebml::uint(b)?,
            0x56bb => t.seek_preroll_ns = ebml::uint(b)?,
            0x23314f if b != 1_f64.to_be_bytes() && b != 1_f32.to_be_bytes() => {
                return Err(Error::Unsupported("TrackTimestampScale"));
            }
            0x6d80 => return Err(Error::Unsupported("track compression/encryption")),
            0xe0 => ebml::children(b, |id, b| {
                match id {
                    0xb0 => t.width = ebml::uint(b)?,
                    0xba => t.height = ebml::uint(b)?,
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
    if t.number == 0 || t.uid == 0 || t.kind == 0 || t.codec.is_empty() {
        return Err(Error::Invalid("missing track fields"));
    }
    Ok(t)
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
