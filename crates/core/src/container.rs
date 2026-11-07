//! Format-neutral container vocabulary.
//!
//! Readers keep their own rich packet and track types; consumers see them
//! through the [`PacketInfo`] and [`TrackInfo`] views, so nothing is converted
//! or copied per packet and format detail is never reduced to a common
//! denominator. These semantic rules hold for every implementation:
//! no fabricated values (`None`/[`RandomAccess::Unknown`] unless the source or a
//! documented rule supplies them), no sentinels, and specification defaults only
//! with an [`Origin`].
use crate::{
    Backpressure, InputRequest, InputToken, InputWindow, Result, SourceCapabilities, SourceFailure,
    SourceId, Step, Time, TimeBase, WorkBudget,
};
use std::sync::atomic::AtomicBool;

/// Stable track identity within one reader generation: the Matroska
/// TrackNumber, the ISO BMFF track_ID or the MPEG-TS elementary PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Video,
    Audio,
    Subtitle,
    Data,
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
    H264,
    Hevc,
    Av1,
    Vp9,
    /// Uncompressed video samples.
    RawVideo,
    Aac,
    /// AAC carried in LATM/LOAS, which has its own framing.
    AacLatm,
    Ac3,
    Eac3,
    /// DTS signalled with a core; extension-only entries stay [`CodecId::Other`].
    Dts,
    Vorbis,
    Opus,
    Flac,
    /// MPEG-1, MPEG-2 or MPEG-2.5 audio, Layer I, II or III; each frame header names its
    /// layer.
    MpegAudio,
    /// Uncompressed integer audio samples.
    Pcm,
    /// UTF-8 subtitle text without styling markup.
    Utf8Text,
    Ass,
    Ssa,
    WebVtt,
    /// 3GPP timed text.
    Tx3g,
    VobSub,
    DvbSubtitle,
    /// EN 300 472 teletext, including its subtitle pages.
    DvbTeletext,
    /// CEA-608 byte pairs as their own track (QuickTime `c608`).
    Eia608,
    Pgs,
    /// Not named above; the same bytes as [`TrackInfo::codec_tag`].
    Other(&'a [u8]),
}

/// Whether decoding can start at a packet. `Unknown` when the source does not say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RandomAccess {
    Sync,
    NotSync,
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

/// A value with its provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sourced<T> {
    pub value: T,
    pub origin: Origin,
}

impl<T> Sourced<T> {
    pub const fn new(value: T, origin: Origin) -> Self {
        Self { value, origin }
    }
}

/// Samples to discard at the start and end of a packet's decoded output, in
/// the track time base.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Trim {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

/// The neutral view of a demuxed packet. Timestamps are ticks in
/// [`PacketInfo::time_base`]; exact [`Time`] is derived on demand.
pub trait PacketInfo {
    fn track(&self) -> TrackId;
    fn payload(&self) -> &[u8];
    fn time_base(&self) -> TimeBase;
    fn pts(&self) -> Option<i64>;
    /// `None` where the container carries no decode time (Matroska).
    fn dts(&self) -> Option<i64>;
    fn duration(&self) -> Option<i64>;
    fn random_access(&self) -> RandomAccess;
    fn trim(&self) -> Trim {
        Trim::default()
    }
    fn pts_time(&self) -> Result<Option<Time>> {
        self.pts()
            .map(|ticks| Time::from_ticks(i128::from(ticks), self.time_base()))
            .transpose()
    }
}

/// The neutral view of a track. Container detail stays on the format type.
pub trait TrackInfo {
    fn id(&self) -> TrackId;
    fn kind(&self) -> MediaKind;
    /// The container's own codec identifier (CodecID string, sample entry
    /// four-character code, or stream_type), unnormalized.
    fn codec_tag(&self) -> &[u8];
    /// The codec this track's reader identifies from [`TrackInfo::codec_tag`].
    fn codec(&self) -> CodecId<'_>;
    /// Codec configuration bytes (CodecPrivate, decoder configuration record).
    fn configuration(&self) -> &[u8];
    fn language(&self) -> Option<&str> {
        None
    }
    fn default_track(&self) -> Option<bool> {
        None
    }
    fn forced(&self) -> Option<bool> {
        None
    }
    fn colour(&self) -> Option<ColourDescription> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SeekPolicy {
    /// Land on the latest random-access point at or before the target.
    PreviousRandomAccess,
    /// Also decode from that point and discard output before the target.
    PlaybackPreroll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekRequest {
    pub track: TrackId,
    pub target: Time,
    pub policy: SeekPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SeekMethod {
    Index,
    BoundedScan,
    Restart,
}

/// What a completed seek did. Format-specific detail (sample ordinals, Cue
/// positions) stays on the reader's own receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekReceipt {
    pub requested: Time,
    /// Time of the random-access point delivery restarts from.
    pub landing: Time,
    /// Decode but do not present output before this time.
    pub discard_until: Time,
    pub method: SeekMethod,
    pub scanned_bytes: u64,
    /// Delivery restarts at the first Cluster/fragment of the source.
    pub replays_from_start: bool,
}

/// ITU-T H.273 colour range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColourRange {
    Unspecified,
    Limited,
    Full,
}

/// SMPTE ST 2086 mastering display volume. Chromaticities are CIE 1931 xy in
/// R, G, B, white order; luminance in cd/m².
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MasteringDisplay {
    pub primaries: [[f64; 2]; 3],
    pub white_point: [f64; 2],
    pub luminance_max: f64,
    pub luminance_min: f64,
}

/// CTA-861.3 content light level in cd/m².
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContentLight {
    pub max_cll: u64,
    pub max_fall: u64,
}

/// One answer to "what colour is this video", whichever source supplied it.
/// Codes are ITU-T H.273 (CICP) code points and stay numeric, including
/// values a future registry adds. Container-specific detail (Matroska
/// BitsPerChannel, subsampling) stays on the format type. Conflicts between
/// container and bitstream are reported as two descriptions, never resolved here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColourDescription {
    pub primaries: Sourced<u64>,
    pub transfer: Sourced<u64>,
    pub matrix: Sourced<u64>,
    pub range: Sourced<ColourRange>,
    /// Horizontal and vertical chroma siting codes as the source states them.
    pub chroma_siting: Option<[u64; 2]>,
    pub mastering: Option<MasteringDisplay>,
    pub content_light: Option<ContentLight>,
}

/// Events a container reader delivers. `P` is the reader's packet type and
/// `X` carries its other typed events (configuration, documents).
#[derive(Clone, Debug)]
pub enum Event<P, X> {
    Packet(P),
    Seek(SeekReceipt),
    Other(X),
}

/// What one [`ContainerReader::step`] returns.
pub type ReaderStep<R> =
    Step<Event<<R as ContainerReader>::Packet, <R as ContainerReader>::Other>, Backpressure>;

/// The one resumable reader interface. Readers are concrete types used through
/// generics or one closed enum at the consumer boundary; there is no registry
/// and no dynamic dispatch on the packet path.
pub trait ContainerReader {
    type Packet: PacketInfo;
    /// `dyn TrackInfo` for a closed enum over several formats: track metadata
    /// may use dynamic dispatch, the packet path never does.
    type Track: TrackInfo + ?Sized;
    type Other;

    fn provide(&mut self, window: &InputWindow) -> Result<()>;
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()>;
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()>;
    /// Restart from the source start, keeping only what the reader documents.
    fn reset(&mut self) -> Result<InputRequest>;
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>>;
    /// Start a seek; the returned request names the input the reader needs next.
    fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest>;
    fn track_count(&self) -> usize;
    fn track(&self, index: usize) -> Option<&Self::Track>;
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
