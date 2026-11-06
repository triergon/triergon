//! Blocking Matroska packet demuxer, specified by
//! `harness/features/matroska-demux/contract.json`.
//!
//! Streaming `Read` is sufficient for demuxing. Seeking additionally requires `Seek`
//! and supports validated Cues, bounded fallback scans and all-track preroll. A
//! resumable envelope reader is separate. This is not a full Matroska implementation.
use crate::Result;
use crate::storage::{SharedBytes, SharedText};
use crate::timing::BlockTiming;

#[derive(Clone, Debug)]
pub struct Track {
    packet_prefix: SharedBytes,
    packet_decoder: Option<crate::content_encoding::Decoder>,
    /// Complete bounded TrackEntry payload, retaining extensions not interpreted yet.
    pub source_metadata: SharedBytes,
    pub number: u64,
    pub uid: u64,
    pub kind: u64,
    pub codec: SharedText,
    /// Original or restored codec configuration. An original source view can
    /// retain the whole charged Tracks allocation; len() is its logical size.
    pub codec_private: SharedBytes,
    pub name: SharedText,
    /// Legacy Matroska language. Use effective_language() for source BCP47 precedence.
    pub language: SharedText,
    pub language_bcp47: Option<SharedText>,
    pub enabled: bool,
    pub lacing: bool,
    pub hearing_impaired: Option<bool>,
    pub visual_impaired: Option<bool>,
    pub text_descriptions: Option<bool>,
    pub original: Option<bool>,
    pub commentary: Option<bool>,
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

impl Track {
    pub fn effective_language(&self) -> &str {
        self.language_bcp47.as_deref().unwrap_or(&self.language)
    }
}

#[derive(Clone, Debug)]
pub struct PacketData {
    storage: SharedBytes,
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
    /// Share the restored frame in its original charged allocation.
    pub fn shared_bytes(&self) -> Result<Option<crate::storage::Bytes>> {
        self.storage
            .as_core()
            .map(|storage| storage.slice(self.range.clone()))
            .transpose()
    }

    pub fn retained_capacity(&self) -> usize {
        self.storage.retained_capacity()
    }
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        self.storage.shares_storage_with(&other.storage)
    }
}

#[derive(Clone, Debug)]
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

/// All fallible packet-clock projection precedes transferring a ready frame.
/// This is the same source clock used by the blocking and incremental adapters.
struct PacketFields {
    timing: BlockTiming,
    track: usize,
    pts_ns: Option<i64>,
    duration_ns: Option<u64>,
    keyframe: bool,
    discard_padding_ns: i64,
}
impl PacketFields {
    #[inline(always)]
    fn new(
        track: usize,
        timing: BlockTiming,
        keyframe: bool,
        discard_padding_ns: i64,
    ) -> Result<Self> {
        let source = timing.source()?;
        Ok(Self {
            timing,
            track,
            pts_ns: source.pts.map(|p| p.rounded_ns()).transpose()?,
            duration_ns: source
                .duration
                .map(|d| d.rounded_duration_ns())
                .transpose()?,
            keyframe,
            discard_padding_ns,
        })
    }
    #[inline(always)]
    fn finish(self, data: PacketData) -> Packet {
        Packet {
            timing: self.timing,
            track: self.track,
            pts_ns: self.pts_ns,
            dts_ns: None,
            duration_ns: self.duration_ns,
            keyframe: self.keyframe,
            discard_padding_ns: self.discard_padding_ns,
            data,
        }
    }
}

fn static_metadata_text(bytes: &[u8]) -> Option<&'static str> {
    Some(match bytes {
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
        _ => return None,
    })
}

pub(crate) mod typed;
