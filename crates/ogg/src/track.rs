//! Tracks and packets, and their neutral views.
use crate::compat::ReferenceTiming;
use crate::mapping::{Configuration, Mapping, Timeline};
use triergon_core::container::{
    CodecId, MediaKind, PacketInfo, RandomAccess, TrackId, TrackInfo, Trim,
};
use triergon_core::{Bytes, Error, TimeBase};

/// One logical stream: a track of one link of the physical stream.
///
/// The track identity is the serial number in the low 32 bits and the chain link
/// (0 for the first) above them, so a link that reuses a serial is still a new track.
#[derive(Debug)]
pub struct Track {
    pub(crate) id: TrackId,
    pub(crate) serial: u32,
    pub(crate) link: u32,
    /// Position among the tracks of its link.
    pub(crate) position: u32,
    pub(crate) mapping: Mapping,
    tag: [u8; 8],
    tag_len: u8,
    pub(crate) config: Configuration,
    pub(crate) time_base: TimeBase,
    /// Ticks from this link's time zero to the physical stream's.
    pub(crate) offset: i64,
    pub(crate) configured: bool,
    pub(crate) header_error: Option<Error>,
    pub(crate) end: Option<i64>,
    /// Where the reference clock ended this track, for the next link.
    pub(crate) reference_end: Option<i64>,
    pub(crate) reference_shift: i64,
}

impl Track {
    pub(crate) fn new(
        serial: u32,
        link: u32,
        position: u32,
        mapping: Mapping,
        tag: &[u8],
        config: Configuration,
        offset: i64,
    ) -> Track {
        let mut bytes = [0; 8];
        let tag_len = tag.len().min(8);
        bytes[..tag_len].copy_from_slice(&tag[..tag_len]);
        Track {
            id: TrackId(u64::from(link) << 32 | u64::from(serial)),
            serial,
            link,
            position,
            mapping,
            tag: bytes,
            tag_len: tag_len as u8,
            time_base: config.time_base(),
            config,
            offset,
            configured: false,
            header_error: None,
            end: None,
            reference_end: None,
            reference_shift: 0,
        }
    }

    pub(crate) fn timeline(&self) -> &Timeline {
        &self.config.timeline
    }

    /// The logical stream's serial number.
    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// The chain link the track belongs to, 0 for the first.
    pub fn link(&self) -> u32 {
        self.link
    }

    pub fn mapping(&self) -> Mapping {
        self.mapping
    }

    /// Whether every header packet has been read (or the mapping has none).
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// Why the header packets could not be read; the track's packets then carry no time.
    pub fn header_error(&self) -> Option<Error> {
        self.header_error
    }

    /// The sample rate the identification header states.
    pub fn sample_rate(&self) -> Option<u32> {
        self.config.sample_rate
    }

    pub fn channels(&self) -> Option<u8> {
        self.config.channels
    }

    /// The comment header: the Vorbis comment packet, the `OpusTags` packet or the
    /// body of FLAC's VORBIS_COMMENT block.
    pub fn comments(&self) -> Option<&[u8]> {
        self.config
            .comments
            .clone()
            .map(|range| &self.config.bytes[range])
    }

    /// Ticks this link's times are moved by: the end of the previous link, so chained
    /// links follow each other on one timeline.
    pub fn offset(&self) -> i64 {
        self.offset
    }

    /// Where the track ends, from the last granule position near the end of a seekable
    /// source, in the track time base; `None` when that scan did not run or found none.
    pub fn end(&self) -> Option<i64> {
        self.end
    }

    pub fn time_base(&self) -> TimeBase {
        self.time_base
    }
}

impl TrackInfo for Track {
    fn id(&self) -> TrackId {
        self.id
    }
    fn kind(&self) -> MediaKind {
        self.mapping.kind()
    }
    /// The mapping's identifying magic: `"\x01vorbis"`, `"OpusHead"`, `"\x7fFLAC"`, a
    /// known magic of another mapping, or the first eight bytes of an unknown one.
    fn codec_tag(&self) -> &[u8] {
        &self.tag[..usize::from(self.tag_len)]
    }
    fn codec(&self) -> CodecId<'_> {
        self.mapping.codec(self.codec_tag())
    }
    /// The canonical configuration the header packets form (see [`crate::mapping`]);
    /// for an unknown mapping, its first packet.
    fn configuration(&self) -> &[u8] {
        &self.config.bytes[self.config.configuration.clone()]
    }
}

/// One media packet.
///
/// Times are ticks of the track time base, 1/sample rate (Opus 1/48000), on the
/// physical stream's timeline: a packet starts where the previous one's samples end,
/// the duration counts every sample the packet completes, and [`PacketInfo::trim`]
/// says how many of them are not presented. Decode order is presentation order, so
/// the DTS is the PTS. A packet of an uninterpreted mapping has no time.
#[derive(Clone, Debug)]
pub struct Packet {
    pub(crate) track: TrackId,
    pub(crate) index: usize,
    pub(crate) payload: Bytes,
    pub(crate) time_base: TimeBase,
    pub(crate) pts: Option<i64>,
    pub(crate) duration: Option<i64>,
    pub(crate) trim: Trim,
    pub(crate) granule: Option<i64>,
    pub(crate) page: u64,
    pub(crate) timed: bool,
    pub(crate) reference: ReferenceTiming,
}

impl Packet {
    /// The index of the packet's track.
    pub fn track_index(&self) -> usize {
        self.index
    }

    /// Shared payload storage, without copying.
    pub fn bytes(&self) -> &Bytes {
        &self.payload
    }

    /// The granule position of the page this packet ends, when it is the last packet
    /// completed there.
    pub fn granule(&self) -> Option<i64> {
        self.granule
    }

    /// Source offset of the page that completes the packet.
    pub fn page_offset(&self) -> u64 {
        self.page
    }

    /// Where the packet's samples end.
    pub fn end(&self) -> Option<i64> {
        self.pts
            .zip(self.duration)
            .map(|(pts, duration)| pts + duration)
    }
}

impl PacketInfo for Packet {
    fn track(&self) -> TrackId {
        self.track
    }
    fn payload(&self) -> &[u8] {
        self.payload.as_ref()
    }
    fn time_base(&self) -> TimeBase {
        self.time_base
    }
    fn pts(&self) -> Option<i64> {
        self.pts
    }
    fn dts(&self) -> Option<i64> {
        self.pts
    }
    fn duration(&self) -> Option<i64> {
        self.duration
    }
    /// Every Vorbis, Opus and FLAC packet can start decoding; a seek receipt states the
    /// preroll their output needs.
    fn random_access(&self) -> RandomAccess {
        if self.timed {
            RandomAccess::Sync
        } else {
            RandomAccess::Unknown
        }
    }
    fn trim(&self) -> Trim {
        self.trim
    }
}
