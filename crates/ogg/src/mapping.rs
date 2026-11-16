//! Codec mappings: what a logical stream's first packet identifies, which packets are
//! headers, the configuration they form, and how long each media packet is.
//!
//! Ogg has no configuration field of its own: a mapping's header packets are the
//! configuration. They are rebuilt into each codec's canonical form, the one
//! Matroska and ISO BMFF carry too:
//!
//! | Mapping | Headers | Configuration |
//! |---|---|---|
//! | Vorbis | identification, comment, setup | the three packets Xiph-laced |
//! | Opus (RFC 7845) | `OpusHead`, `OpusTags` | the `OpusHead` packet |
//! | FLAC (Ogg FLAC 1.0) | `0x7f "FLAC"` with STREAMINFO, then metadata blocks | `fLaC` and every metadata block |
//! | Theora (Theora I, appendix A) | identification, comment, setup | the three packets Xiph-laced |
//! | Speex (Speex manual, Ogg file format) | header, comment, `extra_headers` more | the header packet |
//! | anything else | none | the first packet |
use core::ops::Range;
use triergon_core::container::{CodecId, MediaKind, RandomAccess};
use triergon_core::{Budget, BudgetVec, Error, Result, TimeBase};
use triergon_framing::{
    FlacFrameHeader, FlacStreamInfo, OPUS_RATE, OpusHead, VorbisIdentification, VorbisModes,
    opus_packet_samples, vorbis_header_type,
};

/// The mapping a logical stream's first packet identifies.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mapping {
    Vorbis,
    Opus,
    Flac,
    Theora,
    Speex,
    /// A mapping this reader does not interpret; [`Track::codec_tag`](crate::Track)
    /// holds the start of its first packet.
    Other(MediaKind),
}

/// Identification magic of mappings carried as [`Mapping::Other`], with what they code.
const OTHER: &[(&[u8], MediaKind)] = &[
    (b"\x80daala", MediaKind::Video),
    (b"OVP80", MediaKind::Video),
    (b"BBCD\0", MediaKind::Video),
    (b"CELT    ", MediaKind::Audio),
    (b"PCM     ", MediaKind::Audio),
    (b"\x80kate\0\0\0", MediaKind::Subtitle),
    (b"fishead\0", MediaKind::Data),
    (b"fisbone\0", MediaKind::Data),
    (b"\x01video\0\0\0", MediaKind::Video),
    (b"\x01audio\0\0\0", MediaKind::Audio),
    (b"\x01text\0\0\0\0", MediaKind::Subtitle),
];

/// The Theora identification header's type and magic (Theora I section 6.1).
const THEORA: &[u8] = b"\x80theora";
/// The Speex header's `speex_string`.
const SPEEX: &[u8] = b"Speex   ";

/// Bytes of a first packet an unknown mapping keeps as its tag.
const UNKNOWN_TAG: usize = 8;

impl Mapping {
    /// The mapping and its identifying magic (the codec tag) of a first packet.
    pub fn identify(first: &[u8]) -> (Mapping, &[u8]) {
        if vorbis_header_type(first) == Some(1) {
            return (Mapping::Vorbis, &first[..7]);
        }
        if first.starts_with(b"OpusHead") {
            return (Mapping::Opus, &first[..8]);
        }
        if first.starts_with(b"\x7fFLAC") {
            return (Mapping::Flac, &first[..5]);
        }
        if first.starts_with(THEORA) {
            return (Mapping::Theora, &first[..THEORA.len()]);
        }
        if first.starts_with(SPEEX) {
            return (Mapping::Speex, &first[..SPEEX.len()]);
        }
        for &(magic, kind) in OTHER {
            if first.starts_with(magic) {
                return (Mapping::Other(kind), magic);
            }
        }
        (
            Mapping::Other(MediaKind::Unknown),
            &first[..first.len().min(UNKNOWN_TAG)],
        )
    }

    pub fn kind(self) -> MediaKind {
        match self {
            Mapping::Vorbis | Mapping::Opus | Mapping::Flac | Mapping::Speex => MediaKind::Audio,
            Mapping::Theora => MediaKind::Video,
            Mapping::Other(kind) => kind,
        }
    }

    /// The neutral codec; `tag` is the track's codec tag.
    pub fn codec(self, tag: &[u8]) -> CodecId<'_> {
        match self {
            Mapping::Vorbis => CodecId::Vorbis,
            Mapping::Opus => CodecId::Opus,
            Mapping::Flac => CodecId::Flac,
            Mapping::Theora => CodecId::Theora,
            Mapping::Speex => CodecId::Speex,
            Mapping::Other(_) => CodecId::Other(tag),
        }
    }
}

/// What the headers say about packet durations and granule positions.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Timeline {
    Vorbis {
        blocksize: [u16; 2],
        modes: VorbisModes,
    },
    Opus {
        pre_skip: u16,
    },
    Flac,
    /// One frame per packet, at the identification header's frame rate. A granule
    /// position is split at `shift` bits into the frame count up to the last keyframe and
    /// the frames since (Theora I appendix A.2.3); `count` is 1 where it counts frame
    /// indices instead (bitstream versions before 3.2.1).
    Theora {
        shift: u8,
        count: u8,
    },
    /// `samples` per packet: the header's frame size times its frames per packet.
    Speex {
        samples: u32,
    },
    /// Packets have no known duration; granule positions are not interpreted.
    Opaque,
}

/// The block size of the previous Vorbis packet, which a packet's duration depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Previous {
    /// No audio packet yet: the first one completes no samples.
    Start,
    /// Reading resumed mid-stream.
    Unknown,
    Block(u16),
}

/// A media packet's duration, in samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Durations {
    /// The samples the packet completes.
    pub exact: Option<u32>,
    /// The duration the packet states on its own, without the previous packet: a short
    /// Vorbis block counts as following a short one.
    pub standalone: Option<u32>,
    /// The duration a parser carrying state from the start of reading gives: `exact`,
    /// except that the first packet takes `standalone`.
    pub carried: Option<u32>,
}

impl Durations {
    fn uniform(samples: Option<u32>) -> Durations {
        Durations {
            exact: samples,
            standalone: samples,
            carried: samples,
        }
    }
}

/// Opus decoders converge within 80 ms (RFC 7845 section 4.6).
const OPUS_PREROLL: i64 = 3840;

impl Timeline {
    pub(crate) fn durations(&self, packet: &[u8], previous: &mut Previous) -> Durations {
        match *self {
            Timeline::Vorbis { blocksize, modes } => {
                let Ok(block) = modes.block(packet) else {
                    *previous = Previous::Unknown;
                    return Durations::default();
                };
                let size = block.size(blocksize);
                let flagged = block.previous_long.map(|long| blocksize[usize::from(long)]);
                let standalone = block.samples_after(flagged.unwrap_or(blocksize[0]), blocksize);
                let after = |previous: u16| block.samples_after(previous, blocksize);
                let durations = match *previous {
                    Previous::Start => Durations {
                        exact: Some(0),
                        standalone: Some(standalone),
                        carried: Some(standalone),
                    },
                    Previous::Unknown => Durations {
                        exact: flagged.map(after),
                        standalone: Some(standalone),
                        carried: Some(standalone),
                    },
                    Previous::Block(last) => Durations {
                        exact: Some(after(last)),
                        standalone: Some(standalone),
                        carried: Some(after(last)),
                    },
                };
                *previous = Previous::Block(size);
                durations
            }
            Timeline::Opus { .. } => Durations::uniform(opus_packet_samples(packet).ok()),
            Timeline::Flac => {
                Durations::uniform(FlacFrameHeader::parse(packet).ok().map(|h| h.block_size))
            }
            Timeline::Theora { .. } => Durations::uniform(Some(1)),
            Timeline::Speex { samples } => Durations::uniform(Some(samples)),
            Timeline::Opaque => Durations::default(),
        }
    }

    /// Whether decoding can start at a media packet: a Theora frame whose type is intra
    /// (Theora I section 7.1; an empty packet repeats the previous frame), and every
    /// packet of the timed audio mappings.
    pub(crate) fn random_access(&self, packet: &[u8]) -> RandomAccess {
        match self {
            Timeline::Theora { .. } => match packet.first() {
                Some(&first) if first & 0x40 == 0 => RandomAccess::Sync,
                _ => RandomAccess::NotSync,
            },
            Timeline::Opaque => RandomAccess::Unknown,
            _ => RandomAccess::Sync,
        }
    }

    /// Whether only some packets are random-access points.
    pub(crate) fn keyframes(&self) -> bool {
        matches!(self, Timeline::Theora { .. })
    }

    /// The tick the last keyframe up to a granule position starts at, for mappings
    /// whose granule positions name it; `None` where that tick is out of range.
    pub(crate) fn keyframe(&self, granule: i64) -> Option<i64> {
        match *self {
            Timeline::Theora { shift, count } => (granule >> shift)
                .checked_add(i64::from(count))?
                .checked_sub(1),
            _ => None,
        }
    }

    /// Ticks of the track time base where the packets a granule position ends end:
    /// Opus granules count the pre-skip, which presentation time does not, and Theora's
    /// split granules add up to a frame count.
    pub(crate) fn ticks(&self, granule: i64) -> i64 {
        match *self {
            Timeline::Opus { pre_skip } => granule.saturating_sub(i64::from(pre_skip)),
            Timeline::Theora { shift, count } => (granule >> shift)
                .saturating_add(granule & ((1 << shift) - 1))
                .saturating_add(i64::from(count)),
            _ => granule,
        }
    }

    pub(crate) fn timed(&self) -> bool {
        !matches!(self, Timeline::Opaque)
    }

    /// Whether granule positions trim the start and end: Vorbis I section A.2, RFC 7845
    /// section 4, and Speex, whose granule position is the last sample a packet encodes
    /// (Speex manual, Ogg file format), so coded samples past the last one are not.
    pub(crate) fn trims(&self) -> bool {
        matches!(
            self,
            Timeline::Vorbis { .. } | Timeline::Opus { .. } | Timeline::Speex { .. }
        )
    }

    /// Samples to decode before a target for converged output.
    pub(crate) fn preroll(&self) -> i64 {
        match self {
            Timeline::Opus { .. } => OPUS_PREROLL,
            _ => 0,
        }
    }

    /// Whether a packet's output needs the previous packet decoded first (Vorbis
    /// overlaps each block with the one before).
    pub(crate) fn primes(&self) -> bool {
        matches!(self, Timeline::Vorbis { .. })
    }

    /// The longest packet duration, so a search lands early enough for [`Self::primes`].
    pub(crate) fn longest_overlap(&self) -> i64 {
        match self {
            Timeline::Vorbis { blocksize, .. } => i64::from(blocksize[1]) / 2,
            _ => 0,
        }
    }
}

/// Which header packets a mapping still expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Headers {
    /// Vorbis comment and setup headers; the count already read, 1 or 2.
    Vorbis(u8),
    OpusTags,
    /// FLAC metadata block packets; `None` when the mapping header does not say how
    /// many, so they run to the block marked last.
    Flac(Option<u16>),
    /// Theora comment and setup headers; the count already read, 1 or 2.
    Theora(u8),
    /// The Speex comment header, then this many extra headers.
    SpeexComment(u32),
    /// Speex extra headers still to come.
    SpeexExtra(u32),
    Done,
}

/// Whether a packet is a header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeaderStep {
    /// A header, and more follow.
    More,
    /// The last header.
    Last,
    /// Not a header: the headers have ended and this is media.
    Media,
}

/// What a track's header packets hold.
#[derive(Debug)]
pub(crate) struct Configuration {
    /// The canonical configuration, then any header kept beside it (`OpusTags`).
    pub bytes: BudgetVec<u8>,
    pub configuration: Range<usize>,
    /// The comment header: the Vorbis comment packet, `OpusTags`, or FLAC's
    /// VORBIS_COMMENT block body.
    pub comments: Option<Range<usize>>,
    pub timeline: Timeline,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    /// Theora's frame rate, numerator and denominator.
    pub frame_rate: Option<(u32, u32)>,
    /// Raw Vorbis or Theora identification and comment packets until the setup header
    /// arrives.
    pending: Range<usize>,
    max_bytes: usize,
}

impl Configuration {
    /// The configuration a first packet starts.
    pub(crate) fn first(
        budget: &Budget,
        mapping: Mapping,
        packet: &[u8],
        max_bytes: usize,
    ) -> Result<Configuration> {
        let mut config = Configuration::opaque(budget, max_bytes);
        match mapping {
            Mapping::Vorbis => {
                let id = VorbisIdentification::parse(packet)?;
                config.sample_rate = Some(id.sample_rate);
                config.channels = Some(id.channels);
                config.append(packet)?;
                config.pending = 0..packet.len();
            }
            Mapping::Opus => {
                let head = OpusHead::parse(packet)?;
                config.sample_rate = Some(OPUS_RATE);
                config.channels = Some(head.channels);
                config.append(packet)?;
                config.configuration = 0..packet.len();
                config.timeline = Timeline::Opus {
                    pre_skip: head.pre_skip,
                };
            }
            Mapping::Flac => {
                let (native, _) = ogg_flac_first(packet)?;
                let info = FlacStreamInfo::parse(&native[8..])?;
                config.sample_rate = Some(info.sample_rate);
                config.channels = Some(info.channels);
                config.append(native)?;
                config.configuration = 0..native.len();
                config.timeline = Timeline::Flac;
            }
            Mapping::Theora => {
                let id = TheoraIdentification::parse(packet)?;
                config.frame_rate = Some((id.frame_numerator, id.frame_denominator));
                config.append(packet)?;
                config.pending = 0..packet.len();
                config.timeline = Timeline::Theora {
                    shift: id.keyframe_shift,
                    count: u8::from(id.revision == 0),
                };
            }
            Mapping::Speex => {
                let header = SpeexHeader::parse(packet)?;
                config.sample_rate = Some(header.rate);
                config.channels = Some(header.channels);
                config.append(packet)?;
                config.configuration = 0..packet.len();
                config.timeline = Timeline::Speex {
                    samples: header.samples,
                };
            }
            Mapping::Other(_) => {
                config.append(packet)?;
                config.configuration = 0..packet.len();
            }
        }
        Ok(config)
    }

    /// No configuration bytes and no timeline, as for a track whose first packet
    /// could not be read.
    pub(crate) fn opaque(budget: &Budget, max_bytes: usize) -> Configuration {
        Configuration {
            bytes: BudgetVec::new(budget),
            configuration: 0..0,
            comments: None,
            timeline: Timeline::Opaque,
            sample_rate: None,
            channels: None,
            frame_rate: None,
            pending: 0..0,
            max_bytes,
        }
    }

    fn append(&mut self, packet: &[u8]) -> Result<()> {
        let length = self.bytes.len().saturating_add(packet.len());
        if length > self.max_bytes {
            return Err(Error::Limit {
                requested: length,
                maximum: self.max_bytes,
            });
        }
        self.bytes.try_extend_from_slice(packet)
    }

    /// Store a header packet `headers` classified, and complete the configuration with
    /// the last one. Fails without change.
    pub(crate) fn header(&mut self, headers: Headers, packet: &[u8]) -> Result<()> {
        match headers {
            Headers::Vorbis(1) | Headers::Theora(1) => {
                let start = self.bytes.len();
                self.append(packet)?;
                self.pending.end = self.bytes.len();
                self.comments = Some(start..self.bytes.len());
            }
            Headers::Vorbis(_) => {
                let modes = VorbisModes::from_setup(packet)?;
                let id = VorbisIdentification::parse(&self.bytes[..])?;
                self.lace_xiph(packet)?;
                self.timeline = Timeline::Vorbis {
                    blocksize: id.blocksize,
                    modes,
                };
            }
            Headers::Theora(_) => self.lace_xiph(packet)?,
            Headers::OpusTags | Headers::SpeexComment(_) => {
                let start = self.bytes.len();
                self.append(packet)?;
                self.comments = Some(start..self.bytes.len());
            }
            Headers::Flac(_) => {
                let start = self.bytes.len();
                self.append(packet)?;
                if packet[0] & 0x7f == triergon_framing::FLAC_VORBIS_COMMENT {
                    self.comments = Some(start + 4..self.bytes.len());
                }
                self.configuration = 0..self.bytes.len();
            }
            Headers::SpeexExtra(_) | Headers::Done => {}
        }
        Ok(())
    }

    /// Replace the raw identification and comment packets with the three headers
    /// Xiph-laced.
    fn lace_xiph(&mut self, setup: &[u8]) -> Result<()> {
        let comments = self
            .comments
            .clone()
            .ok_or(Error::Invalid("Xiph comment header"))?;
        let identification = self.pending.start..comments.start;
        let lace = |size: usize| size / 255 + 1;
        let prefix = 1 + lace(identification.len()) + lace(comments.len());
        let total = prefix + self.pending.len() + setup.len();
        if total > self.max_bytes {
            return Err(Error::Limit {
                requested: total,
                maximum: self.max_bytes,
            });
        }
        let mut laced = BudgetVec::new(self.bytes.budget());
        laced.try_reserve_exact(total)?;
        laced.try_extend_from_slice(&[2])?;
        for size in [identification.len(), comments.len()] {
            for _ in 0..size / 255 {
                laced.try_extend_from_slice(&[255])?;
            }
            laced.try_extend_from_slice(&[(size % 255) as u8])?;
        }
        laced.try_extend_from_slice(&self.bytes[self.pending.clone()])?;
        laced.try_extend_from_slice(setup)?;
        self.comments = Some(prefix + comments.start..prefix + comments.end);
        self.configuration = 0..laced.len();
        self.bytes = laced;
        self.pending = 0..0;
        Ok(())
    }

    pub(crate) fn time_base(&self) -> TimeBase {
        let base = match self.frame_rate {
            Some((numerator, denominator)) => {
                TimeBase::new(u64::from(denominator), u64::from(numerator))
            }
            None => self
                .sample_rate
                .map_or(Ok(UNTIMED), |rate| TimeBase::new(1, u64::from(rate))),
        };
        base.ok()
            .filter(|_| self.timeline.timed())
            .unwrap_or(UNTIMED)
    }
}

/// The time base of a track whose packets carry no time.
pub(crate) const UNTIMED: TimeBase = TimeBase::NANOSECONDS;

impl Headers {
    /// The header packets that follow a mapping's first packet.
    pub(crate) fn after_first(mapping: Mapping, first: &[u8]) -> Headers {
        match mapping {
            Mapping::Vorbis => Headers::Vorbis(1),
            Mapping::Opus => Headers::OpusTags,
            Mapping::Flac => match ogg_flac_first(first) {
                // The STREAMINFO block header's flag says whether it is the last block.
                Ok((native, 0)) if native[4] & 0x80 != 0 => Headers::Done,
                Ok((_, count)) => Headers::Flac((count != 0).then_some(count)),
                Err(_) => Headers::Done,
            },
            Mapping::Theora => Headers::Theora(1),
            Mapping::Speex => match SpeexHeader::parse(first) {
                Ok(header) => Headers::SpeexComment(header.extra_headers),
                Err(_) => Headers::Done,
            },
            Mapping::Other(_) => Headers::Done,
        }
    }

    /// Whether `packet` is the next header, and checks it as far as the mapping says.
    pub(crate) fn classify(self, packet: &[u8]) -> Result<HeaderStep> {
        match self {
            Headers::Vorbis(read) => {
                let expected = if read == 1 { 3 } else { 5 };
                if vorbis_header_type(packet) != Some(expected) {
                    return Err(Error::Invalid("Vorbis header packet order"));
                }
                Ok(if read == 1 {
                    HeaderStep::More
                } else {
                    HeaderStep::Last
                })
            }
            Headers::OpusTags => {
                if !packet.starts_with(b"OpusTags") {
                    return Err(Error::Invalid("Opus comment header"));
                }
                Ok(HeaderStep::Last)
            }
            Headers::Flac(remaining) => {
                // A frame starts with its sync code; a metadata block never has type 127.
                let Some(&kind) = packet.first() else {
                    return Err(Error::Invalid("FLAC metadata block"));
                };
                if kind == 0xff || kind & 0x7f == 0x7f || packet.len() < 4 {
                    return match remaining {
                        None => Ok(HeaderStep::Media),
                        Some(_) => Err(Error::Invalid("FLAC metadata block")),
                    };
                }
                let last = match remaining {
                    Some(count) => count <= 1,
                    None => kind & 0x80 != 0,
                };
                Ok(if last {
                    HeaderStep::Last
                } else {
                    HeaderStep::More
                })
            }
            Headers::Theora(read) => {
                let expected = if read == 1 { 0x81 } else { 0x82 };
                if packet.first() != Some(&expected) || packet.get(1..7) != Some(&THEORA[1..]) {
                    return Err(Error::Invalid("Theora header packet order"));
                }
                Ok(if read == 1 {
                    HeaderStep::More
                } else {
                    HeaderStep::Last
                })
            }
            // The manual gives Speex headers no magic: they are the packets before media.
            Headers::SpeexComment(0) => Ok(HeaderStep::Last),
            Headers::SpeexComment(_) => Ok(HeaderStep::More),
            Headers::SpeexExtra(remaining) => Ok(if remaining <= 1 {
                HeaderStep::Last
            } else {
                HeaderStep::More
            }),
            Headers::Done => Ok(HeaderStep::Media),
        }
    }

    /// The state after a header packet.
    pub(crate) fn next(self) -> Headers {
        match self {
            Headers::Vorbis(read) if read < 2 => Headers::Vorbis(read + 1),
            Headers::Theora(read) if read < 2 => Headers::Theora(read + 1),
            Headers::SpeexComment(extra) if extra > 0 => Headers::SpeexExtra(extra),
            Headers::SpeexExtra(remaining) if remaining > 1 => Headers::SpeexExtra(remaining - 1),
            Headers::Flac(Some(count)) if count > 1 => Headers::Flac(Some(count - 1)),
            Headers::Flac(None) => Headers::Flac(None),
            _ => Headers::Done,
        }
    }
}

/// What the Theora identification header (Theora I section 6.2) says about timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TheoraIdentification {
    revision: u8,
    frame_numerator: u32,
    frame_denominator: u32,
    keyframe_shift: u8,
}

impl TheoraIdentification {
    /// The header's 42 bytes: magic, VMAJ, VMIN, VREV, the frame and picture sizes, FRN
    /// at byte 22, FRD at 26, and KFGSHIFT in bits 9..5 of the 16 bits at 40.
    const BYTES: usize = 42;

    fn parse(packet: &[u8]) -> Result<TheoraIdentification> {
        let header = packet.get(..Self::BYTES).ok_or(Error::Truncated)?;
        if !header.starts_with(THEORA) {
            return Err(Error::Invalid("Theora identification header"));
        }
        if header[7] != 3 || header[8] != 2 {
            return Err(Error::Unsupported("Theora bitstream version"));
        }
        let word = |at: usize| {
            u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
        };
        let (frame_numerator, frame_denominator) = (word(22), word(26));
        if frame_numerator == 0 || frame_denominator == 0 {
            return Err(Error::Invalid("Theora frame rate"));
        }
        Ok(TheoraIdentification {
            revision: header[9],
            frame_numerator,
            frame_denominator,
            keyframe_shift: (u16::from_be_bytes([header[40], header[41]]) >> 5 & 0x1f) as u8,
        })
    }
}

/// What the Speex header (Speex manual, Ogg file format, table 2) says: its
/// little-endian `rate` at byte 36, `nb_channels` at 48, `frame_size` at 56,
/// `frames_per_packet` at 64 and `extra_headers` at 68.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpeexHeader {
    rate: u32,
    channels: u8,
    /// Samples per packet.
    samples: u32,
    extra_headers: u32,
}

impl SpeexHeader {
    const BYTES: usize = 80;

    fn parse(packet: &[u8]) -> Result<SpeexHeader> {
        let header = packet.get(..Self::BYTES).ok_or(Error::Truncated)?;
        if !header.starts_with(SPEEX) {
            return Err(Error::Invalid("Speex header"));
        }
        let word = |at: usize| {
            u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
        };
        let (rate, channels, frame_size, frames) = (word(36), word(48), word(56), word(64));
        let samples = frame_size
            .checked_mul(frames)
            .filter(|&samples| samples > 0)
            .ok_or(Error::Invalid("Speex frame size"))?;
        if rate == 0 {
            return Err(Error::Invalid("Speex rate"));
        }
        Ok(SpeexHeader {
            rate,
            channels: u8::try_from(channels)
                .ok()
                .filter(|&c| c > 0)
                .ok_or(Error::Invalid("Speex channels"))?,
            samples,
            extra_headers: word(68),
        })
    }
}

/// The native FLAC stream header inside an Ogg FLAC first packet, and the number of
/// header packets that follow it (0: not stated).
fn ogg_flac_first(packet: &[u8]) -> Result<(&[u8], u16)> {
    let rest = packet
        .strip_prefix(b"\x7fFLAC")
        .ok_or(Error::Invalid("Ogg FLAC mapping header"))?;
    let header = rest.get(..4).ok_or(Error::Truncated)?;
    if header[0] != 1 {
        return Err(Error::Unsupported("Ogg FLAC mapping version"));
    }
    let native = &rest[4..];
    if !native.starts_with(b"fLaC")
        || native.get(4).map(|kind| kind & 0x7f) != Some(triergon_framing::FLAC_STREAMINFO)
        || native.len() < 8 + FlacStreamInfo::BYTES
    {
        return Err(Error::Invalid("Ogg FLAC STREAMINFO"));
    }
    Ok((native, u16::from_be_bytes([header[2], header[3]])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_packets_name_their_mapping() {
        assert_eq!(Mapping::identify(b"\x01vorbis\0\0\0\0").0, Mapping::Vorbis);
        assert_eq!(Mapping::identify(b"OpusHead\x01").1, b"OpusHead");
        assert_eq!(Mapping::identify(b"\x7fFLAC\x01\0").0, Mapping::Flac);
        let (theora, tag) = Mapping::identify(b"\x80theora\x03\x02");
        assert_eq!((theora.kind(), tag), (MediaKind::Video, &b"\x80theora"[..]));
        assert_eq!(theora.codec(tag), CodecId::Theora);
        let (speex, tag) = Mapping::identify(b"Speex   1.2");
        assert_eq!(
            (speex.kind(), speex.codec(tag)),
            (MediaKind::Audio, CodecId::Speex)
        );
        let (daala, tag) = Mapping::identify(b"\x80daala\0");
        assert_eq!(daala.codec(tag), CodecId::Other(b"\x80daala"));
        let (unknown, tag) = Mapping::identify(b"abcdefghijkl");
        assert_eq!(
            (unknown, tag),
            (Mapping::Other(MediaKind::Unknown), &b"abcdefgh"[..])
        );
        assert_eq!(Mapping::identify(b"").1, b"");
    }

    /// A Theora identification header: version 3.2.`revision`, `rate` frames per
    /// `per` seconds, keyframe granule shift `shift`.
    pub(crate) fn theora_identification(revision: u8, rate: u32, per: u32, shift: u8) -> Vec<u8> {
        let mut header = b"\x80theora\x03\x02".to_vec();
        header.push(revision);
        header.extend_from_slice(&[0, 20, 0, 15]); // 320 x 240 in macroblocks
        header.extend_from_slice(&[0, 1, 64, 0, 0, 240, 0, 0]); // picture size and offset
        header.extend_from_slice(&rate.to_be_bytes());
        header.extend_from_slice(&per.to_be_bytes());
        header.extend_from_slice(&[0, 0, 1, 0, 0, 1, 0, 0, 0, 0]); // aspect, colour, bitrate
        let bits = u16::from(shift) << 5 | 0b10 << 3; // quality 0, 4:2:0
        header.extend_from_slice(&bits.to_be_bytes());
        assert_eq!(header.len(), TheoraIdentification::BYTES);
        header
    }

    /// A Speex header for `rate`, `frame_size` samples a frame, `frames` a packet and
    /// `extra` extra headers.
    pub(crate) fn speex_header(rate: u32, frame_size: u32, frames: u32, extra: u32) -> Vec<u8> {
        let mut header = b"Speex   1.2.1".to_vec();
        header.resize(28, 0);
        for value in [
            1,
            80,
            rate,
            1,
            4,
            1,
            u32::MAX,
            frame_size,
            0,
            frames,
            extra,
            0,
            0,
        ] {
            header.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(header.len(), SpeexHeader::BYTES);
        header
    }

    #[test]
    fn theora_keyframes_of_extreme_granules_are_none_not_wrapped() {
        let budget = Budget::new(1 << 20).unwrap();
        for (revision, shift) in [(0, 0), (1, 0), (1, 6), (1, 31)] {
            let identification = theora_identification(revision, 25, 1, shift);
            let config =
                Configuration::first(&budget, Mapping::Theora, &identification, 1 << 20).unwrap();
            let timeline = config.timeline;
            for granule in [i64::MIN, i64::MIN + 1, -1, 0, i64::MAX] {
                // Neither panics; a keyframe tick, where there is one, is in range.
                let _ = timeline.ticks(granule);
                let _ = timeline.keyframe(granule);
            }
        }
        // Bitstream 3.2.1 counts frames from one: keyframe count i64::MIN names tick
        // i64::MIN - 1, which is out of range.
        let identification = theora_identification(1, 25, 1, 0);
        let timeline = Configuration::first(&budget, Mapping::Theora, &identification, 1 << 20)
            .unwrap()
            .timeline;
        assert_eq!(timeline.keyframe(i64::MIN), None);
        assert_eq!(timeline.keyframe(i64::MIN + 1), Some(i64::MIN));
    }

    #[test]
    fn theora_granules_split_at_the_keyframe_shift() {
        let budget = Budget::new(1 << 20).unwrap();
        let first = theora_identification(1, 30000, 1001, 6);
        let mut config = Configuration::first(&budget, Mapping::Theora, &first, 1 << 20).unwrap();
        assert_eq!(config.frame_rate, Some((30000, 1001)));
        assert_eq!(config.time_base(), TimeBase::new(1001, 30000).unwrap());
        let timeline = config.timeline;
        // Keyframe count 1234, 35 frames since: frame 1269 ends there.
        let granule = 1234 << 6 | 0b10_0011;
        assert_eq!(timeline.ticks(granule), 1269);
        assert_eq!(timeline.keyframe(granule), Some(1233));
        // The first frame, a keyframe, is 1|0 and ends at tick 1.
        assert_eq!(timeline.ticks(1 << 6), 1);
        assert_eq!(timeline.keyframe(1 << 6), Some(0));
        assert_eq!(timeline.random_access(&[0x00, 1]), RandomAccess::Sync);
        assert_eq!(timeline.random_access(&[0x40, 1]), RandomAccess::NotSync);
        assert_eq!(timeline.random_access(&[]), RandomAccess::NotSync);
        let mut previous = Previous::Start;
        assert_eq!(timeline.durations(&[0x40], &mut previous).exact, Some(1));
        // Bitstream 3.2.0 counts frame indices: the first frame is 0|0.
        let old = Configuration::first(
            &budget,
            Mapping::Theora,
            &theora_identification(0, 25, 1, 6),
            1 << 20,
        )
        .unwrap();
        assert_eq!(old.timeline.ticks(0), 1);
        assert_eq!(old.timeline.keyframe(0), Some(0));
        // The comment and setup headers follow, and are laced with the first.
        let headers = Headers::after_first(Mapping::Theora, &first);
        assert_eq!(headers, Headers::Theora(1));
        assert_eq!(
            headers.classify(b"\x81theora\0\0").unwrap(),
            HeaderStep::More
        );
        config.header(headers, b"\x81theora\0\0").unwrap();
        assert!(headers.next().classify(b"\x81theora").is_err());
        assert_eq!(
            headers.next().classify(b"\x82theora\x01").unwrap(),
            HeaderStep::Last
        );
        config.header(headers.next(), b"\x82theora\x01").unwrap();
        let laced = &config.bytes[config.configuration.clone()];
        assert_eq!(&laced[..3], &[2, 42, 9]);
        assert_eq!(laced.len(), 3 + 42 + 9 + 8);
        let bad = theora_identification(1, 0, 1, 6);
        assert!(Configuration::first(&budget, Mapping::Theora, &bad, 1 << 20).is_err());
    }

    #[test]
    fn speex_packets_last_the_header_frames() {
        let budget = Budget::new(1 << 20).unwrap();
        let first = speex_header(16000, 320, 2, 1);
        let config = Configuration::first(&budget, Mapping::Speex, &first, 1 << 20).unwrap();
        assert_eq!(
            (config.sample_rate, config.channels),
            (Some(16000), Some(1))
        );
        assert_eq!(config.time_base(), TimeBase::new(1, 16000).unwrap());
        assert_eq!(&config.bytes[config.configuration.clone()], &first[..]);
        let mut previous = Previous::Start;
        assert_eq!(
            config.timeline.durations(b"any", &mut previous).exact,
            Some(640)
        );
        assert!(config.timeline.trims());
        // The comment header and one extra header precede media.
        let headers = Headers::after_first(Mapping::Speex, &first);
        assert_eq!(headers, Headers::SpeexComment(1));
        assert_eq!(headers.classify(b"comment").unwrap(), HeaderStep::More);
        assert_eq!(headers.next(), Headers::SpeexExtra(1));
        assert_eq!(headers.next().classify(b"extra").unwrap(), HeaderStep::Last);
        let plain = Headers::after_first(Mapping::Speex, &speex_header(8000, 160, 1, 0));
        assert_eq!(plain.classify(b"comment").unwrap(), HeaderStep::Last);
        let empty = speex_header(8000, 160, 0, 0);
        assert!(Configuration::first(&budget, Mapping::Speex, &empty, 1 << 20).is_err());
    }

    #[test]
    fn flac_headers_run_to_the_count_or_the_last_block() {
        assert_eq!(
            Headers::Flac(Some(2)).classify(&[4, 0, 0, 0]).unwrap(),
            HeaderStep::More
        );
        assert_eq!(
            Headers::Flac(Some(1)).classify(&[4, 0, 0, 0]).unwrap(),
            HeaderStep::Last
        );
        assert_eq!(
            Headers::Flac(None).classify(&[0x84, 0, 0, 0]).unwrap(),
            HeaderStep::Last
        );
        assert_eq!(
            Headers::Flac(None).classify(&[0xff, 0xf8, 0, 0]).unwrap(),
            HeaderStep::Media
        );
        assert!(Headers::Flac(Some(1)).classify(&[0xff, 0xf8]).is_err());
        assert_eq!(Headers::Flac(Some(2)).next(), Headers::Flac(Some(1)));
        assert_eq!(Headers::Vorbis(1).next(), Headers::Vorbis(2));
        assert_eq!(Headers::Vorbis(2).next(), Headers::Done);
    }
}
