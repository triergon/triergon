//! Feed packets for an explicitly selected Matroska track.

mod aac_clock;
mod bitmap;
mod bitmap_probe;
mod discovery;
mod hevc_dolby;
mod probe_clock;
mod video_clock;
mod video_seek;
use crate::{Delivery, Output, RawEvent, Result};
pub use bitmap::{
    MatroskaBitmapCodec, MatroskaBitmapSource, MatroskaBitmapTrack, MatroskaDvbPacket,
    MatroskaDvbStep,
};
pub use bitmap_probe::{MatroskaBitmapClockProbe, MatroskaBitmapLiveClock};
pub use discovery::{
    MatroskaCompactDiscovered, MatroskaCompactDiscovery, MatroskaDiscovered, MatroskaDiscovery,
    MatroskaDiscoveryCutoff, MatroskaDiscoveryLimits, MatroskaDiscoveryStep, MatroskaReplayPrefix,
    MatroskaStartupSource,
};
pub use hevc_dolby::{
    MatroskaDolbyLimits, MatroskaHevcClock, MatroskaHevcDolbyFeed, MatroskaHevcDolbyJob,
    MatroskaHevcDolbyLimits, MatroskaHevcDolbyPacket, MatroskaHevcDolbyPayload,
    MatroskaHevcDolbySetup,
};
pub use probe_clock::{
    MatroskaClock, MatroskaClockPrefix, MatroskaClockProbe, MatroskaPacketTiming,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    AacAdtsParameters, AacAscParser, AacConfigLimits, AacSource, AvcAnnexBAccessUnit,
    AvcAnnexBConfig, AvcAnnexBJob, AvcAnnexBLimits, AvcConfigLimits, AvcConfigParser,
    AvcConfigSource, BuildStep,
};
use triergon_matroska::typed::{Configuration, Event, Packet};
pub use video_clock::MatroskaAvcClock;
pub use video_seek::MatroskaVideoSeekGate;

#[derive(Clone, Copy, Debug)]
pub struct MatroskaFeedLimits {
    pub avc_config: AvcConfigLimits,
    pub avc_packet: AvcAnnexBLimits,
    pub aac_config: AacConfigLimits,
    pub aac_output_bytes: usize,
}

fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        return Err(Error::Limit { requested, maximum });
    }
    Ok(())
}

struct Association {
    configuration: Configuration,
    track: usize,
}

impl Association {
    fn matches(&self, packet: &Packet) -> bool {
        let a = &self.configuration;
        let b = &packet.configuration;
        self.track == packet.packet.track
            && a.generation() == b.generation()
            && a.revision() == b.revision()
            && std::ptr::eq(a.tracks(), b.tracks())
    }

    fn selected_packet<'a>(&self, output: &'a Output) -> Result<Option<&'a Packet>> {
        let RawEvent::Matroska(Event::Packet(packet)) = output.event() else {
            return Ok(None);
        };
        self.selected_source(packet, output.delivery())
    }

    fn selected_source<'a>(
        &self,
        packet: &'a Packet,
        delivery: Delivery,
    ) -> Result<Option<&'a Packet>> {
        if delivery == Delivery::Unselected {
            return Ok(None);
        }
        if !self.matches(packet) {
            return Err(Error::Invalid("Matroska feed configuration changed"));
        }
        Ok(Some(packet))
    }
}

enum Parser {
    Avc(AvcConfigParser),
    Aac(AacAscParser),
}

enum FeedCodec {
    Avc(AvcAnnexBConfig),
    Aac(AacAdtsParameters),
}

/// Prepare a feed from the selected track's effective initialization. A later
/// CodecState or seek generation can require a fresh setup.
pub struct MatroskaFeedSetup {
    pending: Option<(Association, Parser)>,
    limits: MatroskaFeedLimits,
}

impl MatroskaFeedSetup {
    /// `source` is the player's input identity. Framing parsers address the
    /// isolated initialization record from offset zero within that record.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        configuration: &Configuration,
        track: usize,
        limits: MatroskaFeedLimits,
    ) -> Result<Self> {
        if !configuration.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let descriptor = configuration
            .tracks()
            .get(track)
            .ok_or(Error::Invalid("Matroska feed track index"))?;
        let codec = descriptor.codec.as_str();
        let maximum_record_bytes = match (descriptor.kind, codec) {
            (1, "V_MPEG4/ISO/AVC") => limits.avc_config.maximum_record_bytes,
            (2, "A_AAC") => limits.aac_config.maximum_record_bytes,
            _ => return Err(Error::Unsupported("Matroska feed codec")),
        };
        let init = configuration
            .initialization(track)
            .ok_or(Error::Invalid("missing Matroska initialization"))?;
        if init.data.len() > maximum_record_bytes {
            return Err(Error::Limit {
                requested: init.data.len(),
                maximum: maximum_record_bytes,
            });
        }
        let record = match configuration.initialization_bytes(track) {
            Some(record) => record,
            None => Bytes::copy_from_slice(pool, init.data)?,
        };
        let parser = if codec == "V_MPEG4/ISO/AVC" {
            Parser::Avc(AvcConfigParser::new(
                pool,
                AvcConfigSource {
                    source,
                    generation: init.generation,
                    offset: 0,
                },
                record,
                limits.avc_config,
            )?)
        } else {
            if limits.aac_output_bytes < 7 {
                return Err(Error::Invalid("Matroska AAC output limit"));
            }
            Parser::Aac(AacAscParser::new(
                pool,
                AacSource {
                    source,
                    generation: init.generation,
                    offset: 0,
                },
                record,
                limits.aac_config,
            )?)
        };
        Ok(Self {
            pending: Some((
                Association {
                    configuration: configuration.clone(),
                    track,
                },
                parser,
            )),
            limits,
        })
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<MatroskaTrackFeed>> {
        let Some((_, parser)) = &mut self.pending else {
            return Ok(BuildStep::Complete);
        };
        let result = parse(parser, work, cancel);
        match result {
            Ok(BuildStep::Yield) => Ok(BuildStep::Yield),
            Ok(BuildStep::Ready(codec)) => {
                let (association, _) = self
                    .pending
                    .take()
                    .ok_or(Error::Invalid("AVC/AAC setup consumed"))?;
                Ok(BuildStep::Ready(MatroskaTrackFeed {
                    association,
                    codec,
                    limits: self.limits,
                }))
            }
            Ok(BuildStep::Complete) => Err(Error::Invalid("AVC/AAC setup lost its configuration")),
            Err(error) => {
                if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                    self.pending = None;
                }
                Err(error)
            }
        }
    }
}

fn parse(
    parser: &mut Parser,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> Result<BuildStep<FeedCodec>> {
    match parser {
        Parser::Avc(parser) => Ok(match parser.step(work, cancel)? {
            BuildStep::Ready(record) => {
                BuildStep::Ready(FeedCodec::Avc(AvcAnnexBConfig::new(record)?))
            }
            BuildStep::Yield => BuildStep::Yield,
            BuildStep::Complete => BuildStep::Complete,
        }),
        Parser::Aac(parser) => Ok(match parser.step(work, cancel)? {
            BuildStep::Ready(config) => {
                let params = AacAdtsParameters::from_asc(config.facts())?;
                if params.channel_configuration == 0 {
                    return Err(Error::Unsupported(
                        "Matroska AAC feed with program configuration element",
                    ));
                }
                BuildStep::Ready(FeedCodec::Aac(params))
            }
            BuildStep::Yield => BuildStep::Yield,
            BuildStep::Complete => BuildStep::Complete,
        }),
    }
}

pub struct MatroskaTrackFeed {
    association: Association,
    codec: FeedCodec,
    limits: MatroskaFeedLimits,
}

impl MatroskaTrackFeed {
    /// Use the player's current-generation and pause checks before submitting.
    /// Unselected source packets remain available without producing feed work.
    pub fn packet<'a>(&'a self, output: &'a Output) -> Result<Option<MatroskaFeedJob<'a>>> {
        self.association
            .selected_packet(output)?
            .map(|source| self.prepare_packet(source))
            .transpose()
    }

    fn prepare_packet<'a>(&'a self, source: &'a Packet) -> Result<MatroskaFeedJob<'a>> {
        let payload = source.packet.data.as_ref();
        let codec = match &self.codec {
            FeedCodec::Avc(config) => {
                PacketJob::Avc(AvcAnnexBJob::new(config, payload, self.limits.avc_packet)?)
            }
            FeedCodec::Aac(params) => {
                if payload.is_empty() {
                    return Err(Error::Invalid("empty AAC access unit"));
                }
                let total = payload.len().checked_add(7).ok_or(Error::Overflow)?;
                let maximum = self.limits.aac_output_bytes;
                if total > maximum {
                    return Err(Error::Limit {
                        requested: total,
                        maximum,
                    });
                }
                PacketJob::Aac(Some(params.header_for_payload(payload.len())?))
            }
        };
        Ok(MatroskaFeedJob {
            source,
            codec,
            cancelled: false,
        })
    }
}

enum PacketJob<'a> {
    Avc(AvcAnnexBJob<'a>),
    Aac(Option<[u8; 7]>),
}

pub struct MatroskaFeedJob<'a> {
    source: &'a Packet,
    codec: PacketJob<'a>,
    cancelled: bool,
}

impl<'a> MatroskaFeedJob<'a> {
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<MatroskaFeedPacket<'a>>> {
        let payload = match &mut self.codec {
            PacketJob::Avc(job) => match job.step(work, cancel)? {
                BuildStep::Ready(unit) => MatroskaFeedPayload::Avc(unit),
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Complete => return Ok(BuildStep::Complete),
            },
            PacketJob::Aac(header) => {
                if header.is_none() {
                    return Ok(BuildStep::Complete);
                }
                if self.cancelled || cancel.load(Ordering::Relaxed) {
                    self.cancelled = true;
                    return Err(Error::Cancelled);
                }
                if !work.try_charge(1) {
                    return Ok(BuildStep::Yield);
                }
                MatroskaFeedPayload::Aac {
                    header: header.take().ok_or(Error::Invalid("AAC feed consumed"))?,
                    raw: self.source.packet.data.as_ref(),
                }
            }
        };
        Ok(BuildStep::Ready(MatroskaFeedPacket {
            source: self.source,
            payload,
        }))
    }
}

pub struct MatroskaFeedPacket<'a> {
    pub source: &'a Packet,
    pub payload: MatroskaFeedPayload<'a>,
}

pub enum MatroskaFeedPayload<'a> {
    Avc(AvcAnnexBAccessUnit<'a>),
    Aac { header: [u8; 7], raw: &'a [u8] },
}

impl MatroskaFeedPayload<'_> {
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Avc(unit) => unit.byte_len(),
            Self::Aac { header, raw } => header.len() + raw.len(),
        }
    }

    /// The sink can consume the spans or gather them into its owned feed buffer.
    #[inline]
    pub fn segments(&self) -> impl Iterator<Item = &[u8]> {
        match self {
            Self::Avc(unit) => FeedSegments::Avc(unit.segments()),
            Self::Aac { header, raw } => FeedSegments::Aac([header.as_slice(), *raw].into_iter()),
        }
    }
}

enum FeedSegments<'a, Avc> {
    Avc(Avc),
    Aac(std::array::IntoIter<&'a [u8], 2>),
}

impl<'a, Avc: Iterator<Item = &'a [u8]>> Iterator for FeedSegments<'a, Avc> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Avc(iter) => iter.next(),
            Self::Aac(iter) => iter.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Avc(iter) => iter.size_hint(),
            Self::Aac(iter) => iter.size_hint(),
        }
    }
}
