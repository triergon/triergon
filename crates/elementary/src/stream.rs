//! The packets, track and limits every elementary reader shares.
use crate::syntax::SeekBasis;
use crate::syntax::TrackFacts;
use triergon_core::container::{
    CodecId, MediaKind, PacketInfo, RandomAccess, SeekReceipt, TrackId, TrackInfo, Trim,
};
use triergon_core::{Bytes, TimeBase};

/// The one track of an elementary stream: index 0, as FFmpeg numbers it.
pub const TRACK: TrackId = TrackId(0);

/// Resource limits of a reader. Every input-dependent allocation is charged to the
/// reader's `Budget`; these bound what it may hold or scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Pool size a [`crate::BlockingReader`] creates when it is not given one.
    pub live_storage_bytes: usize,
    /// Largest access unit: a FLAC frame (at most about 2.2 MB for 65535 samples of
    /// eight 32-bit channels coded verbatim), an MPEG audio, ADTS or AC-3 frame, or
    /// the frames of one E-AC-3 access unit.
    pub frame_bytes: usize,
    /// Total bytes of tags and metadata blocks kept; anything beyond is recorded by
    /// its length only.
    pub retained_metadata_bytes: usize,
    /// Bytes searched for a frame sync before the first frame, after a seek and
    /// between damaged frames.
    pub sync_scan_bytes: u64,
    /// Largest input window requested.
    pub input_window: usize,
    /// FLAC seek points kept from a SEEKTABLE; a longer table is ignored.
    pub seek_points: usize,
    /// Jumps a bisecting seek may make.
    pub seek_probes: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            live_storage_bytes: 64 << 20,
            frame_bytes: 4 << 20,
            retained_metadata_bytes: 4 << 20,
            sync_scan_bytes: 1 << 20,
            input_window: 64 << 10,
            seek_points: 1 << 16,
            seek_probes: 64,
        }
    }
}

/// The elementary stream's track.
#[derive(Debug)]
pub struct Track {
    pub(crate) facts: TrackFacts,
    pub(crate) time_base: TimeBase,
}

impl Track {
    /// Samples per second of the first frame; the packet time base is its inverse.
    pub fn sample_rate(&self) -> u32 {
        self.facts.sample_rate
    }
    /// Channels the stream header or first frame states; `None` when a program
    /// configuration inside the payload states them (ADTS channel configuration 0).
    pub fn channels(&self) -> Option<u8> {
        self.facts.channels
    }
    /// Samples the stream states it holds (FLAC STREAMINFO, Xing or VBRI frames less
    /// the LAME delay and padding), `None` when it states none.
    pub fn duration(&self) -> Option<u64> {
        self.facts.duration
    }
    pub fn time_base(&self) -> TimeBase {
        self.time_base
    }
}

impl TrackInfo for Track {
    fn id(&self) -> TrackId {
        TRACK
    }
    fn kind(&self) -> MediaKind {
        MediaKind::Audio
    }
    /// A tag naming the stream format: `fLaC`, `mp1`/`mp2`/`mp3`, `adts`, `ac-3` or
    /// `ec-3`. An elementary stream carries no container tag of its own.
    fn codec_tag(&self) -> &[u8] {
        self.facts.codec_tag
    }
    fn codec(&self) -> CodecId<'_> {
        self.facts.codec
    }
    /// FLAC: `fLaC` and the STREAMINFO block, as FFmpeg keeps it. Empty for the other
    /// formats, whose frames describe themselves.
    fn configuration(&self) -> &[u8] {
        &self.facts.configuration
    }
}

/// One access unit.
///
/// Times are samples (`1/sample_rate`). Audio frames decode in presentation order, so
/// the decode time is the presentation time.
#[derive(Clone, Debug)]
pub struct Packet {
    pub(crate) data: Bytes,
    pub(crate) offset: u64,
    pub(crate) pts: i64,
    pub(crate) duration: u32,
    pub(crate) time_base: TimeBase,
    pub(crate) trim: Trim,
    pub(crate) complete: bool,
}

impl Packet {
    /// Source offset of the first byte.
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn bytes(&self) -> &Bytes {
        &self.data
    }
    /// `false` for the last packet when the input ended inside its frame, or when it
    /// holds bytes after the last frame that start no frame (FFmpeg delivers both).
    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

impl PacketInfo for Packet {
    #[inline]
    fn track(&self) -> TrackId {
        TRACK
    }
    #[inline]
    fn payload(&self) -> &[u8] {
        &self.data
    }
    #[inline]
    fn time_base(&self) -> TimeBase {
        self.time_base
    }
    #[inline]
    fn pts(&self) -> Option<i64> {
        Some(self.pts)
    }
    #[inline]
    fn dts(&self) -> Option<i64> {
        Some(self.pts)
    }
    #[inline]
    fn duration(&self) -> Option<i64> {
        Some(i64::from(self.duration))
    }
    /// Every frame of these formats starts a decode.
    #[inline]
    fn random_access(&self) -> RandomAccess {
        RandomAccess::Sync
    }
    #[inline]
    fn trim(&self) -> Trim {
        self.trim
    }
}

/// Events other than packets and seek receipts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderEvent {
    /// The track is known; it comes before the first packet.
    Opened,
}

/// What the last seek did, beyond the neutral receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekDetail {
    pub receipt: SeekReceipt,
    /// Whether the landing time is exact or estimated, and from what.
    pub basis: SeekBasis,
    /// Jumps the seek made.
    pub probes: u32,
}
