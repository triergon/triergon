//! One closed reader enum at the consumer boundary, built with at least one format.
//!
//! A player may receive Matroska directly and TS or fragmented MP4 through HLS,
//! so the reader is chosen at open time from probed bytes. Each variant sits behind its format
//! feature; forwarding is one predictable `match` per step with no allocation
//! and no dynamic dispatch on the packet path.
mod open;

pub use open::{Replayed, open_blocking, open_sequential};

use crate::probe::{Probed, SourceFormat, probe, probe_whole};
use std::sync::atomic::AtomicBool;
use triergon_core::container::{
    ContainerReader, Event, PacketInfo, RandomAccess, ReaderStep, SeekRequest, TrackId, TrackInfo,
    Trim,
};
use triergon_core::{
    Budget, Error, InputRequest, InputToken, InputWindow, Result, SourceCapabilities,
    SourceFailure, SourceId, Step, TimeBase, WorkBudget,
};

/// The largest input window a reader asks for at once.
const WINDOW: usize = 64 * 1024;

/// Every reader the facade can open, one variant per enabled format.
// Stored inline: a box would add an allocation outside the caller's budget.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum AnyReader {
    /// Matroska or WebM with track configuration and block side data.
    #[cfg(feature = "matroska")]
    Matroska(triergon_matroska::Reader),
    /// ISO BMFF samples from a seekable source.
    #[cfg(feature = "isobmff")]
    Isobmff(triergon_isobmff::Reader),
    /// Access units and transport events from TS188 or M2TS192.
    #[cfg(feature = "mpegts")]
    MpegTs(triergon_mpegts::Reader),
    /// Packets from the logical streams of an Ogg source.
    #[cfg(feature = "ogg")]
    Ogg(triergon_ogg::Reader),
    /// FLAC frames with or without a native stream header.
    #[cfg(feature = "elementary")]
    Flac(triergon_elementary::FlacReader),
    /// Frames from an MPEG audio elementary stream.
    #[cfg(feature = "elementary")]
    MpegAudio(triergon_elementary::MpegAudioReader),
    /// AAC audio from an ADTS elementary stream.
    #[cfg(feature = "elementary")]
    Adts(triergon_elementary::AdtsReader),
    /// Syncframes from an AC-3 or E-AC-3 elementary stream.
    #[cfg(feature = "elementary")]
    Ac3(triergon_elementary::Ac3Reader),
}

/// The packet of whichever reader produced it.
// Stored inline: a box would allocate per packet, outside the caller's budget.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum AnyPacket {
    /// One Matroska block frame with its track configuration and side data.
    #[cfg(feature = "matroska")]
    Matroska(triergon_matroska::TypedPacket),
    /// One ISO BMFF sample with its track timing.
    #[cfg(feature = "isobmff")]
    Isobmff(triergon_isobmff::Packet),
    /// One framed access unit from an MPEG transport stream.
    #[cfg(feature = "mpegts")]
    MpegTs(triergon_mpegts::Packet),
    /// One packet assembled from the pages of an Ogg logical stream.
    #[cfg(feature = "ogg")]
    Ogg(triergon_ogg::Packet),
    /// One coded audio packet from an elementary stream.
    #[cfg(feature = "elementary")]
    Elementary(triergon_elementary::Packet),
}

/// Format-specific payloads of [`Event::Other`].
// Stored inline for the same reason as `AnyPacket`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum AnyOther {
    /// Matroska structure, track configuration or a validated document.
    #[cfg(feature = "matroska")]
    Matroska(triergon_matroska::Event),
    /// An ISO BMFF root box or notification that the movie index is ready.
    #[cfg(feature = "isobmff")]
    Isobmff(triergon_isobmff::ReaderEvent),
    /// Original transport or PES data, or an outcome of continuity or framing.
    #[cfg(feature = "mpegts")]
    MpegTs(triergon_mpegts::ReaderEvent),
    /// Track readiness, a new chained link, skipped bytes or lost pages.
    #[cfg(feature = "ogg")]
    Ogg(triergon_ogg::ReaderEvent),
    /// Notification that an elementary stream's track is ready.
    #[cfg(feature = "elementary")]
    Elementary(triergon_elementary::ReaderEvent),
}

macro_rules! each {
    ($value:expr, $inner:ident => $body:expr) => {
        match $value {
            #[cfg(feature = "matroska")]
            Self::Matroska($inner) => $body,
            #[cfg(feature = "isobmff")]
            Self::Isobmff($inner) => $body,
            #[cfg(feature = "mpegts")]
            Self::MpegTs($inner) => $body,
            #[cfg(feature = "ogg")]
            Self::Ogg($inner) => $body,
            #[cfg(feature = "elementary")]
            Self::Flac($inner) => $body,
            #[cfg(feature = "elementary")]
            Self::MpegAudio($inner) => $body,
            #[cfg(feature = "elementary")]
            Self::Adts($inner) => $body,
            #[cfg(feature = "elementary")]
            Self::Ac3($inner) => $body,
        }
    };
}

macro_rules! each_packet {
    ($value:expr, $inner:ident => $body:expr) => {
        match $value {
            #[cfg(feature = "matroska")]
            Self::Matroska($inner) => $body,
            #[cfg(feature = "isobmff")]
            Self::Isobmff($inner) => $body,
            #[cfg(feature = "mpegts")]
            Self::MpegTs($inner) => $body,
            #[cfg(feature = "ogg")]
            Self::Ogg($inner) => $body,
            #[cfg(feature = "elementary")]
            Self::Elementary($inner) => $body,
        }
    };
}

/// Wrap one reader's step in the closed enums.
macro_rules! neutral_step {
    ($step:expr, $variant:ident) => {
        match $step {
            Step::Ready(Event::Packet(p)) => Step::Ready(Event::Packet(AnyPacket::$variant(p))),
            Step::Ready(Event::Seek(receipt)) => Step::Ready(Event::Seek(receipt)),
            Step::Ready(Event::Other(o)) => Step::Ready(Event::Other(AnyOther::$variant(o))),
            Step::NeedInput(request) => Step::NeedInput(request),
            Step::Yield => Step::Yield,
            Step::Backpressure(pressure) => Step::Backpressure(pressure),
            Step::End(location) => Step::End(location),
        }
    };
}

impl AnyReader {
    /// Open the reader of the format `prefix` starts, with the format's default limits;
    /// feed it the source from its start. A prefix of the length `capabilities` give is
    /// the whole source ([`probe_whole`]); any other is the start of a longer one
    /// ([`probe`]).
    ///
    /// Fails with [`Error::Unsupported`] when the prefix selects no format, including one
    /// [`probe`] needs more bytes of, or a format whose reader feature is not enabled; and
    /// with the reader's error when the budget cannot hold its state.
    // Out of line: inlined, the probe's code cost a caller's packet loop around it up to
    // 0.065% of its instructions (subtitle worker, Matroska inputs).
    #[inline(never)]
    pub fn open(
        prefix: &[u8],
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
    ) -> Result<Self> {
        let format = if capabilities.length == Some(prefix.len() as u64) {
            probe_whole(prefix)
        } else if let Probed::Format(format) = probe(prefix) {
            Some(format)
        } else {
            None
        };
        Self::of(format, pool, source, capabilities)
    }

    /// The reader of `format`, or the refusal of a source no format the facade opens starts.
    pub(crate) fn of(
        format: Option<SourceFormat>,
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
    ) -> Result<Self> {
        let format = format.ok_or(Error::Unsupported("source format not recognised"))?;
        // The reader of one variant with its crate's limits.
        macro_rules! reader {
            ($variant:ident, $reader:path, $limits:expr) => {
                Ok(Self::$variant(<$reader>::new(
                    pool,
                    source,
                    capabilities,
                    $limits,
                    WINDOW,
                )?))
            };
        }
        match format {
            #[cfg(feature = "matroska")]
            SourceFormat::Matroska => reader!(
                Matroska,
                triergon_matroska::Reader,
                triergon_matroska::Limits::default()
            ),
            #[cfg(feature = "isobmff")]
            SourceFormat::Isobmff => reader!(
                Isobmff,
                triergon_isobmff::Reader,
                triergon_isobmff::ReaderLimits::default()
            ),
            #[cfg(feature = "ogg")]
            SourceFormat::Ogg => {
                reader!(Ogg, triergon_ogg::Reader, triergon_ogg::Limits::default())
            }
            #[cfg(feature = "elementary")]
            SourceFormat::Flac => {
                reader!(
                    Flac,
                    triergon_elementary::FlacReader,
                    triergon_elementary::Limits::default()
                )
            }
            #[cfg(feature = "elementary")]
            SourceFormat::MpegAudio => reader!(
                MpegAudio,
                triergon_elementary::MpegAudioReader,
                triergon_elementary::Limits::default()
            ),
            #[cfg(feature = "elementary")]
            SourceFormat::Adts => {
                reader!(
                    Adts,
                    triergon_elementary::AdtsReader,
                    triergon_elementary::Limits::default()
                )
            }
            #[cfg(feature = "elementary")]
            SourceFormat::Ac3 => reader!(
                Ac3,
                triergon_elementary::Ac3Reader,
                triergon_elementary::Limits::default()
            ),
            #[cfg(feature = "mpegts")]
            kind @ (SourceFormat::Ts | SourceFormat::M2ts) => reader!(
                MpegTs,
                triergon_mpegts::Reader,
                triergon_mpegts::ReaderLimits {
                    format: if kind == SourceFormat::Ts {
                        triergon_mpegts::PacketFormat::Ts188
                    } else {
                        triergon_mpegts::PacketFormat::M2ts192
                    },
                    ..triergon_mpegts::ReaderLimits::default()
                }
            ),
            #[allow(
                unreachable_patterns,
                reason = "every format whose feature is enabled has its arm"
            )]
            _ => Err(Error::Unsupported("reader feature not enabled")),
        }
    }
}

impl PacketInfo for AnyPacket {
    #[inline]
    fn track(&self) -> TrackId {
        each_packet!(self, p => p.track())
    }
    #[inline]
    fn payload(&self) -> &[u8] {
        each_packet!(self, p => p.payload())
    }
    #[inline]
    fn time_base(&self) -> TimeBase {
        each_packet!(self, p => p.time_base())
    }
    #[inline]
    fn pts(&self) -> Option<i64> {
        each_packet!(self, p => p.pts())
    }
    #[inline]
    fn dts(&self) -> Option<i64> {
        each_packet!(self, p => p.dts())
    }
    #[inline]
    fn duration(&self) -> Option<i64> {
        each_packet!(self, p => p.duration())
    }
    #[inline]
    fn random_access(&self) -> RandomAccess {
        each_packet!(self, p => p.random_access())
    }
    #[inline]
    fn trim(&self) -> Trim {
        each_packet!(self, p => p.trim())
    }
}

impl ContainerReader for AnyReader {
    type Packet = AnyPacket;
    type Track = dyn TrackInfo;
    type Other = AnyOther;

    fn provide(&mut self, window: &InputWindow) -> Result<()> {
        each!(self, r => r.provide(window))
    }
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        each!(self, r => r.finish(source, token))
    }
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()> {
        each!(self, r => r.fail(source, token, failure))
    }
    fn reset(&mut self) -> Result<InputRequest> {
        each!(self, r => r.reset())
    }
    #[inline]
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        Ok(match self {
            #[cfg(feature = "matroska")]
            Self::Matroska(reader) => {
                neutral_step!(ContainerReader::step(reader, work, cancel)?, Matroska)
            }
            #[cfg(feature = "isobmff")]
            Self::Isobmff(reader) => {
                neutral_step!(ContainerReader::step(reader, work, cancel)?, Isobmff)
            }
            #[cfg(feature = "mpegts")]
            Self::MpegTs(reader) => {
                neutral_step!(ContainerReader::step(reader, work, cancel)?, MpegTs)
            }
            #[cfg(feature = "ogg")]
            Self::Ogg(reader) => neutral_step!(ContainerReader::step(reader, work, cancel)?, Ogg),
            #[cfg(feature = "elementary")]
            Self::Flac(reader) => {
                neutral_step!(ContainerReader::step(reader, work, cancel)?, Elementary)
            }
            #[cfg(feature = "elementary")]
            Self::MpegAudio(reader) => {
                neutral_step!(ContainerReader::step(reader, work, cancel)?, Elementary)
            }
            #[cfg(feature = "elementary")]
            Self::Adts(reader) => {
                neutral_step!(ContainerReader::step(reader, work, cancel)?, Elementary)
            }
            #[cfg(feature = "elementary")]
            Self::Ac3(reader) => {
                neutral_step!(ContainerReader::step(reader, work, cancel)?, Elementary)
            }
        })
    }
    fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        each!(self, r => r.request_seek(request))
    }
    fn track_count(&self) -> usize {
        each!(self, r => r.track_count())
    }
    fn track(&self, index: usize) -> Option<&(dyn TrackInfo + 'static)> {
        each!(self, r => ContainerReader::track(r, index).map(|t| t as &(dyn TrackInfo + 'static)))
    }
    fn capabilities(&self) -> SourceCapabilities {
        each!(self, r => ContainerReader::capabilities(r))
    }
}
