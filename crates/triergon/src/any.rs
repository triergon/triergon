//! One closed reader enum at the consumer boundary.
//!
//! A player may receive Matroska directly and TS or fragmented MP4 through HLS,
//! so the reader is chosen at open time from probed bytes. Each variant sits behind its format
//! feature; forwarding is one predictable `match` per step with no allocation
//! and no dynamic dispatch on the packet path.
// With no format feature the enums are empty and their forwarding is dead code.
#![cfg_attr(
    not(any(
        feature = "matroska",
        feature = "isobmff",
        feature = "mpegts",
        feature = "ogg",
        feature = "elementary"
    )),
    allow(unused_imports, unused_variables, unreachable_code)
)]
use crate::probe::{Prefix, Probe, ProbeBuffer, probe};
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
#[cfg(any(
    feature = "matroska",
    feature = "isobmff",
    feature = "mpegts",
    feature = "ogg",
    feature = "elementary"
))]
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
            #[cfg(not(any(
                feature = "matroska",
                feature = "isobmff",
                feature = "mpegts",
                feature = "ogg",
                feature = "elementary"
            )))]
            _ => unreachable!("no format feature: the enum is uninhabited"),
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
            #[cfg(not(any(
                feature = "matroska",
                feature = "isobmff",
                feature = "mpegts",
                feature = "ogg",
                feature = "elementary"
            )))]
            _ => unreachable!("no format feature: the enum is uninhabited"),
        }
    };
}

/// Wrap one reader's step in the closed enums.
#[cfg(any(
    feature = "matroska",
    feature = "isobmff",
    feature = "mpegts",
    feature = "ogg",
    feature = "elementary"
))]
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
    /// Open the reader selected by [`probe`] with the format's default limits.
    /// The prefix selects the format. Feed it to the reader with the rest of the source.
    ///
    /// Returns an error if the prefix cannot select an enabled reader, the source
    /// lacks capabilities that reader requires, or the budget cannot hold its state.
    pub fn open(
        prefix: &[u8],
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
    ) -> Result<Self> {
        Self::from_probe(probe(prefix), pool, source, capabilities)
    }

    fn from_probe(
        kind: Probe,
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
    ) -> Result<Self> {
        match kind {
            #[cfg(feature = "matroska")]
            Probe::Matroska => Ok(Self::Matroska(triergon_matroska::Reader::new(
                pool,
                source,
                capabilities,
                triergon_matroska::Limits::default(),
                WINDOW,
            )?)),
            #[cfg(feature = "isobmff")]
            Probe::Isobmff => Ok(Self::Isobmff(triergon_isobmff::Reader::new(
                pool,
                source,
                capabilities,
                triergon_isobmff::ReaderLimits::default(),
                WINDOW,
            )?)),
            #[cfg(feature = "ogg")]
            Probe::Ogg => Ok(Self::Ogg(triergon_ogg::Reader::new(
                pool,
                source,
                capabilities,
                triergon_ogg::Limits::default(),
                WINDOW,
            )?)),
            #[cfg(feature = "elementary")]
            Probe::Flac => Ok(Self::Flac(triergon_elementary::Reader::new(
                pool,
                source,
                capabilities,
                triergon_elementary::Limits::default(),
                WINDOW,
            )?)),
            #[cfg(feature = "elementary")]
            Probe::MpegAudio => Ok(Self::MpegAudio(triergon_elementary::Reader::new(
                pool,
                source,
                capabilities,
                triergon_elementary::Limits::default(),
                WINDOW,
            )?)),
            #[cfg(feature = "elementary")]
            Probe::Adts => Ok(Self::Adts(triergon_elementary::Reader::new(
                pool,
                source,
                capabilities,
                triergon_elementary::Limits::default(),
                WINDOW,
            )?)),
            #[cfg(feature = "elementary")]
            Probe::Ac3 => Ok(Self::Ac3(triergon_elementary::Reader::new(
                pool,
                source,
                capabilities,
                triergon_elementary::Limits::default(),
                WINDOW,
            )?)),
            #[cfg(feature = "mpegts")]
            kind @ (Probe::Ts | Probe::M2ts) => Ok(Self::MpegTs(triergon_mpegts::Reader::new(
                pool,
                source,
                capabilities,
                triergon_mpegts::ReaderLimits {
                    format: if kind == Probe::Ts {
                        triergon_mpegts::PacketFormat::Ts188
                    } else {
                        triergon_mpegts::PacketFormat::M2ts192
                    },
                    ..triergon_mpegts::ReaderLimits::default()
                },
                WINDOW,
            )?)),
            #[cfg(not(feature = "mpegts"))]
            Probe::Ts | Probe::M2ts => Err(Error::Unsupported("MPEG-TS reader feature disabled")),
            _ => {
                let _ = (pool, source, capabilities);
                Err(Error::Unsupported("source format not recognised"))
            }
        }
    }
}

/// Probe up to 64 KiB of a seekable source, then feed its reader from the start.
///
/// Returns an error if reading, seeking, format selection or reader construction fails.
pub fn open_blocking<S: std::io::Read + std::io::Seek>(
    mut source: S,
    pool: &Budget,
) -> Result<triergon_core::blocking::Blocking<AnyReader, S>> {
    let capabilities = triergon_core::blocking::seekable_capabilities(&mut source)?;
    let kind = ProbeBuffer::new(pool).identify(&mut source, capabilities.length);
    source.rewind().map_err(|e| Error::Io(e.kind()))?;
    let reader = AnyReader::from_probe(kind?, pool, SourceId(1), capabilities)?;
    Ok(triergon_core::blocking::Blocking::seekable(
        reader, source, pool,
    ))
}

/// Probe up to 64 KiB of a sequential source and replay the consumed prefix.
/// The reader sees a pipe with unknown length and no random access.
///
/// Returns an error if the budget cannot hold the probe or reader, reading fails,
/// or the prefix cannot select an enabled reader that supports sequential input.
pub fn open_sequential<S: std::io::Read>(
    mut source: S,
    pool: &Budget,
) -> Result<triergon_core::blocking::Blocking<AnyReader, Replayed<S>>> {
    let mut prefix = ProbeBuffer::new(pool);
    let kind = prefix.identify_pipe(&mut source)?;
    let capabilities = SourceCapabilities {
        length: None,
        random_access: false,
    };
    let reader = AnyReader::from_probe(kind, pool, SourceId(1), capabilities)?;
    let source = Replayed {
        prefix: Some(prefix.into_replay()?),
        at: 0,
        rest: source,
    };
    Ok(triergon_core::blocking::Blocking::sequential(
        reader, source, pool,
    ))
}

/// A sequential source with its probed prefix put back in front; the prefix is freed
/// once it has been read.
#[derive(Debug)]
pub struct Replayed<S> {
    prefix: Option<Prefix>,
    at: usize,
    rest: S,
}

impl<S: std::io::Read> std::io::Read for Replayed<S> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if let Some(prefix) = &self.prefix {
            let count = out.len().min(prefix.bytes.len() - self.at);
            out[..count].copy_from_slice(&prefix.bytes[self.at..self.at + count]);
            self.at += count;
            if self.at == prefix.bytes.len() {
                self.prefix = None;
            }
            return Ok(count);
        }
        self.rest.read(out)
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
            #[cfg(not(any(
                feature = "matroska",
                feature = "isobmff",
                feature = "mpegts",
                feature = "ogg",
                feature = "elementary"
            )))]
            _ => unreachable!("no format feature: the enum is uninhabited"),
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
