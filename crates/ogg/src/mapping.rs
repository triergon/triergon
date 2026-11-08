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
//! | anything else | none | the first packet |
use core::ops::Range;
use triergon_core::container::{CodecId, MediaKind};
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
    /// A mapping this reader does not interpret; [`Track::codec_tag`](crate::Track)
    /// holds the start of its first packet.
    Other(MediaKind),
}

/// Identification magic of mappings carried as [`Mapping::Other`], with what they code.
const OTHER: &[(&[u8], MediaKind)] = &[
    (b"\x80theora", MediaKind::Video),
    (b"\x80daala", MediaKind::Video),
    (b"OVP80", MediaKind::Video),
    (b"BBCD\0", MediaKind::Video),
    (b"Speex   ", MediaKind::Audio),
    (b"CELT    ", MediaKind::Audio),
    (b"PCM     ", MediaKind::Audio),
    (b"\x80kate\0\0\0", MediaKind::Subtitle),
    (b"fishead\0", MediaKind::Data),
    (b"fisbone\0", MediaKind::Data),
    (b"\x01video\0\0\0", MediaKind::Video),
    (b"\x01audio\0\0\0", MediaKind::Audio),
    (b"\x01text\0\0\0\0", MediaKind::Subtitle),
];

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
            Mapping::Vorbis | Mapping::Opus | Mapping::Flac => MediaKind::Audio,
            Mapping::Other(kind) => kind,
        }
    }

    /// The neutral codec; `tag` is the track's codec tag.
    pub fn codec(self, tag: &[u8]) -> CodecId<'_> {
        match self {
            Mapping::Vorbis => CodecId::Vorbis,
            Mapping::Opus => CodecId::Opus,
            Mapping::Flac => CodecId::Flac,
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
            Timeline::Opaque => Durations::default(),
        }
    }

    /// Ticks of the track time base at a granule position: Opus granules count the
    /// pre-skip, which presentation time does not.
    pub(crate) fn ticks(&self, granule: i64) -> i64 {
        match *self {
            Timeline::Opus { pre_skip } => granule.saturating_sub(i64::from(pre_skip)),
            _ => granule,
        }
    }

    /// The granule position of a tick.
    pub(crate) fn granule(&self, ticks: i64) -> i64 {
        match *self {
            Timeline::Opus { pre_skip } => ticks.saturating_add(i64::from(pre_skip)),
            _ => ticks,
        }
    }

    pub(crate) fn timed(&self) -> bool {
        !matches!(self, Timeline::Opaque)
    }

    /// Whether the mapping lets granule positions trim the start and end (Vorbis I
    /// section A.2, RFC 7845 section 4).
    pub(crate) fn trims(&self) -> bool {
        matches!(self, Timeline::Vorbis { .. } | Timeline::Opus { .. })
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
    /// Raw Vorbis identification and comment packets until the setup header arrives.
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
            Headers::Vorbis(1) => {
                let start = self.bytes.len();
                self.append(packet)?;
                self.pending.end = self.bytes.len();
                self.comments = Some(start..self.bytes.len());
            }
            Headers::Vorbis(_) => {
                let modes = VorbisModes::from_setup(packet)?;
                let id = VorbisIdentification::parse(&self.bytes[..])?;
                self.lace_vorbis(packet)?;
                self.timeline = Timeline::Vorbis {
                    blocksize: id.blocksize,
                    modes,
                };
            }
            Headers::OpusTags => {
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
            Headers::Done => {}
        }
        Ok(())
    }

    /// Replace the raw identification and comment packets with the three headers
    /// Xiph-laced.
    fn lace_vorbis(&mut self, setup: &[u8]) -> Result<()> {
        let comments = self
            .comments
            .clone()
            .ok_or(Error::Invalid("Vorbis comment header"))?;
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
        self.sample_rate
            .filter(|_| self.timeline.timed())
            .and_then(|rate| TimeBase::new(1, u64::from(rate)).ok())
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
            Headers::Done => Ok(HeaderStep::Media),
        }
    }

    /// The state after a header packet.
    pub(crate) fn next(self) -> Headers {
        match self {
            Headers::Vorbis(read) if read < 2 => Headers::Vorbis(read + 1),
            Headers::Flac(Some(count)) if count > 1 => Headers::Flac(Some(count - 1)),
            Headers::Flac(None) => Headers::Flac(None),
            _ => Headers::Done,
        }
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
        assert_eq!(theora.codec(tag), CodecId::Other(b"\x80theora"));
        let (unknown, tag) = Mapping::identify(b"abcdefghijkl");
        assert_eq!(
            (unknown, tag),
            (Mapping::Other(MediaKind::Unknown), &b"abcdefgh"[..])
        );
        assert_eq!(Mapping::identify(b"").1, b"");
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
