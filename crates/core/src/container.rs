//! The container model every reader shares.
//!
//! A reader keeps its own packet and track types and shows them through the
//! [`PacketInfo`] and [`TrackInfo`] views: nothing is converted or copied per
//! packet, and format detail stays on the format's type. Every implementation
//! follows the same rules. No value is made up: it is `None` or
//! [`RandomAccess::Unknown`] unless the source or a documented rule supplies
//! it. No value is a sentinel. A specification default is marked as one by its
//! [`Origin`].
use crate::{
    Backpressure, InputRequest, InputToken, InputWindow, Result, SourceCapabilities, SourceFailure,
    SourceId, Step, Time, TimeBase, WorkBudget,
};
use std::sync::atomic::AtomicBool;

/// Stable track identity within one reader generation: the Matroska
/// TrackNumber, the ISO BMFF track_ID or the MPEG-TS elementary PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackId(pub u64);

/// What a track carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// Coded pictures.
    Video,
    /// Coded audio.
    Audio,
    /// Text or bitmap subtitles.
    Subtitle,
    /// Something else the container declares, such as timed metadata.
    Data,
    /// The container does not say.
    Unknown,
}

/// A codec, named the same way by every reader.
///
/// Each reader owns one mapping from its native identifier (Matroska CodecID,
/// ISO BMFF handler and sample entry, MPEG-TS stream_type). A name appears only
/// where the source identifies that codec; anything else is [`CodecId::Other`]
/// with the container's own tag, never a guess. Framing, profile and sample
/// format stay on the format type: [`CodecId::Aac`] covers raw and ADTS access
/// units, and [`CodecId::Pcm`] says nothing about width, order or sign.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodecId<'a> {
    /// ISO/IEC 13818-2 video, which includes ISO/IEC 11172-2 (MPEG-1) video.
    Mpeg2Video,
    /// ITU-T H.264 (AVC).
    H264,
    /// ITU-T H.265 (HEVC).
    Hevc,
    /// AOMedia Video 1.
    Av1,
    /// VP9.
    Vp9,
    /// Theora (Xiph.Org Theora I).
    Theora,
    /// Uncompressed video samples.
    RawVideo,
    /// AAC (ISO/IEC 13818-7 or 14496-3) as raw access units or in ADTS.
    Aac,
    /// AAC carried in LATM/LOAS, which has its own framing.
    AacLatm,
    /// AC-3 (ATSC A/52).
    Ac3,
    /// Enhanced AC-3 (ATSC A/52 Annex E).
    Eac3,
    /// DTS signalled with a core; extension-only entries stay [`CodecId::Other`].
    Dts,
    /// Vorbis I.
    Vorbis,
    /// Opus (RFC 6716).
    Opus,
    /// FLAC (RFC 9639).
    Flac,
    /// Speex (Xiph.Org Speex).
    Speex,
    /// MPEG-1, MPEG-2 or MPEG-2.5 audio, Layer I, II or III; each frame header names its
    /// layer.
    MpegAudio,
    /// Uncompressed integer audio samples.
    Pcm,
    /// UTF-8 subtitle text without styling markup.
    Utf8Text,
    /// Advanced SubStation Alpha scripts (version 4+).
    Ass,
    /// SubStation Alpha scripts (version 4).
    Ssa,
    /// WebVTT cues.
    WebVtt,
    /// 3GPP timed text.
    Tx3g,
    /// DVD subpictures.
    VobSub,
    /// EN 300 743 DVB subtitles.
    DvbSubtitle,
    /// EN 300 472 teletext, including its subtitle pages.
    DvbTeletext,
    /// CEA-608 byte pairs as their own track (QuickTime `c608`).
    Eia608,
    /// Blu-ray presentation graphics.
    Pgs,
    /// Not named above; the same bytes as [`TrackInfo::codec_tag`].
    Other(&'a [u8]),
}

/// Whether decoding can start at a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RandomAccess {
    /// It can.
    Sync,
    /// It cannot: the packet depends on earlier ones.
    NotSync,
    /// The source does not say.
    Unknown,
}

/// Where a value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// Stated by the container.
    Container,
    /// Stated by the coded bitstream (VUI, SEI, codec configuration).
    Bitstream,
    /// Absent from the source; the specification's default applies.
    SpecificationDefault,
}

/// A value with where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sourced<T> {
    /// The value.
    pub value: T,
    /// Its origin.
    pub origin: Origin,
}

impl<T> Sourced<T> {
    /// `value` from `origin`.
    pub const fn new(value: T, origin: Origin) -> Self {
        Self { value, origin }
    }
}

/// Decoded output to discard at the start and end of a packet, in the track
/// time base. `None` where the source states none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Trim {
    /// Amount to discard from the start.
    pub start: Option<i64>,
    /// Amount to discard from the end.
    pub end: Option<i64>,
}

/// The neutral view of a demuxed packet. Timestamps are ticks in
/// [`PacketInfo::time_base`]; exact [`Time`] is derived on demand.
pub trait PacketInfo {
    /// The track the packet belongs to.
    fn track(&self) -> TrackId;
    /// The coded bytes, as the container frames them.
    fn payload(&self) -> &[u8];
    /// The length of one tick of this packet's timestamps.
    fn time_base(&self) -> TimeBase;
    /// Presentation time in ticks; `None` when the source carries none.
    fn pts(&self) -> Option<i64>;
    /// Decode time in ticks; `None` when the source carries none, as in
    /// Matroska.
    fn dts(&self) -> Option<i64>;
    /// Duration in ticks; `None` when the source carries none.
    fn duration(&self) -> Option<i64>;
    /// Whether decoding can start here.
    fn random_access(&self) -> RandomAccess;
    /// Decoded output to discard; nothing unless the format says so.
    fn trim(&self) -> Trim {
        Trim::default()
    }
    /// [`pts`](Self::pts) as exact time.
    fn pts_time(&self) -> Result<Option<Time>> {
        self.pts()
            .map(|ticks| Time::from_ticks(i128::from(ticks), self.time_base()))
            .transpose()
    }
}

/// The neutral view of a track. Container detail stays on the format type.
pub trait TrackInfo {
    /// The track's identity, as its packets name it.
    fn id(&self) -> TrackId;
    /// What the track carries.
    fn kind(&self) -> MediaKind;
    /// The container's own codec identifier (CodecID string, sample entry
    /// four-character code, or stream_type), unnormalized.
    fn codec_tag(&self) -> &[u8];
    /// The codec this track's reader identifies from [`TrackInfo::codec_tag`].
    fn codec(&self) -> CodecId<'_>;
    /// Codec configuration bytes (CodecPrivate, decoder configuration record).
    fn configuration(&self) -> &[u8];
    /// The language tag as the container writes it; `None` when it has none.
    fn language(&self) -> Option<&str> {
        None
    }
    /// The container's default-track flag; `None` when it has none.
    fn default_track(&self) -> Option<bool> {
        None
    }
    /// The container's forced-track flag; `None` when it has none.
    fn forced(&self) -> Option<bool> {
        None
    }
    /// The colour the container describes for a video track.
    fn colour(&self) -> Option<ColourDescription> {
        None
    }
}

/// What a seek prepares for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SeekPolicy {
    /// Land on the latest random-access point at or before the target.
    PreviousRandomAccess,
    /// Also decode from that point and discard output before the target.
    PlaybackPreroll,
}

/// A seek to a time on one track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekRequest {
    /// The track whose random-access points decide the landing.
    pub track: TrackId,
    /// The presentation time to reach.
    pub target: Time,
    /// What the seek prepares for.
    pub policy: SeekPolicy,
}

/// How a reader found the landing point of a seek.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SeekMethod {
    /// From an index the container carries.
    Index,
    /// By scanning a bounded part of the source.
    BoundedScan,
    /// By starting again from the beginning.
    Restart,
}

/// What a completed seek did. Format-specific detail (sample ordinals, Cue
/// positions) stays on the reader's own receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekReceipt {
    /// The target of the request.
    pub requested: Time,
    /// Time of the random-access point delivery restarts from.
    pub landing: Time,
    /// Decode but do not present output before this time.
    pub discard_until: Time,
    /// How the landing point was found.
    pub method: SeekMethod,
    /// Source bytes read to find it.
    pub scanned_bytes: u64,
    /// Delivery restarts at the first Cluster/fragment of the source.
    pub replays_from_start: bool,
}

/// ITU-T H.273 colour range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColourRange {
    /// The source does not say.
    Unspecified,
    /// Studio range: 8-bit luma 16 to 235, chroma 16 to 240.
    Limited,
    /// Every code is a level: 8-bit 0 to 255.
    Full,
}

/// SMPTE ST 2086 mastering display volume. Chromaticities are CIE 1931 xy in
/// R, G, B, white order; luminance in cd/m².
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MasteringDisplay {
    /// The xy chromaticity of the red, green and blue primaries.
    pub primaries: [[f64; 2]; 3],
    /// The xy chromaticity of the white point.
    pub white_point: [f64; 2],
    /// Greatest display luminance.
    pub luminance_max: f64,
    /// Least display luminance.
    pub luminance_min: f64,
}

/// CTA-861.3 content light level in cd/m².
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContentLight {
    /// Maximum content light level: the brightest pixel.
    pub max_cll: u64,
    /// Maximum frame-average light level.
    pub max_fall: u64,
}

/// One answer to "what colour is this video", whichever source supplied it.
/// Codes are ITU-T H.273 (CICP) code points and stay numeric, including
/// values a future registry adds. Container-specific detail (Matroska
/// BitsPerChannel, subsampling) stays on the format type. Conflicts between
/// container and bitstream are reported as two descriptions, never resolved here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColourDescription {
    /// ColourPrimaries code.
    pub primaries: Sourced<u64>,
    /// TransferCharacteristics code.
    pub transfer: Sourced<u64>,
    /// MatrixCoefficients code.
    pub matrix: Sourced<u64>,
    /// Sample range.
    pub range: Sourced<ColourRange>,
    /// Horizontal and vertical chroma siting codes as the source states them.
    pub chroma_siting: Option<[u64; 2]>,
    /// Mastering display volume, when the source carries one.
    pub mastering: Option<MasteringDisplay>,
    /// Content light level, when the source carries one.
    pub content_light: Option<ContentLight>,
}

/// Events a container reader delivers. `P` is the reader's packet type and
/// `X` carries its other typed events (configuration, documents).
#[derive(Clone, Debug)]
pub enum Event<P, X> {
    /// The next packet, in its track's decode order.
    Packet(P),
    /// A seek completed; packets after this come from its landing point.
    Seek(SeekReceipt),
    /// An event of the reader's own type.
    Other(X),
}

/// What one [`ContainerReader::step`] returns.
pub type ReaderStep<R> =
    Step<Event<<R as ContainerReader>::Packet, <R as ContainerReader>::Other>, Backpressure>;

/// The one resumable reader interface. Readers are concrete types used through
/// generics or one closed enum at the consumer boundary; there is no registry
/// and no dynamic dispatch on the packet path.
pub trait ContainerReader {
    /// The reader's packet.
    type Packet: PacketInfo;
    /// The reader's track. `dyn TrackInfo` for a closed enum over several
    /// formats: track metadata may use dynamic dispatch, the packet path
    /// never does.
    type Track: TrackInfo + ?Sized;
    /// The reader's other events; see [`Event::Other`].
    type Other;

    /// Answers the request of the last [`Step::NeedInput`] with bytes.
    fn provide(&mut self, window: &InputWindow) -> Result<()>;
    /// Answers it with the end of the source.
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()>;
    /// Answers it with a failure; every later step returns that failure.
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()>;
    /// Restart from the source start, keeping only what the reader documents.
    fn reset(&mut self) -> Result<InputRequest>;
    /// Does at most `work` units of parsing and returns the next event, or why
    /// there is none yet. Fails with [`Error::Cancelled`](crate::Error::Cancelled)
    /// once `cancel` is set.
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>>;
    /// Start a seek; the returned request names the input the reader needs next.
    fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest>;
    /// Number of tracks known so far.
    fn track_count(&self) -> usize;
    /// The track at `index`, below [`track_count`](Self::track_count).
    fn track(&self, index: usize) -> Option<&Self::Track>;
    /// The capabilities of the source being read.
    fn capabilities(&self) -> SourceCapabilities;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;
    impl PacketInfo for Fixed {
        fn track(&self) -> TrackId {
            TrackId(1)
        }
        fn payload(&self) -> &[u8] {
            &[]
        }
        fn time_base(&self) -> TimeBase {
            TimeBase::new(1, 90_000).unwrap()
        }
        fn pts(&self) -> Option<i64> {
            Some(-90_000)
        }
        fn dts(&self) -> Option<i64> {
            None
        }
        fn duration(&self) -> Option<i64> {
            None
        }
        fn random_access(&self) -> RandomAccess {
            RandomAccess::Unknown
        }
    }

    #[test]
    fn packet_time_is_exact_and_never_fabricated() {
        let p = Fixed;
        assert_eq!(p.pts_time().unwrap(), Some(Time::new(-1, 1).unwrap()));
        assert_eq!(p.trim(), Trim::default());
        assert_eq!(p.dts(), None);
    }

    #[test]
    fn an_unnamed_codec_keeps_the_tag_it_was_given() {
        let tag = *b"xyz1";
        assert_eq!(CodecId::Other(&tag), CodecId::Other(b"xyz1"));
        assert_ne!(CodecId::Other(b""), CodecId::H264);
    }

    #[test]
    fn neutral_values_stay_small_on_32_bit_targets() {
        assert!(size_of::<Trim>() <= 32);
        assert!(size_of::<Sourced<u64>>() <= 16);
        assert_eq!(size_of::<TrackId>(), 8);
        assert!(size_of::<CodecId<'_>>() <= 3 * size_of::<usize>());
    }
}
