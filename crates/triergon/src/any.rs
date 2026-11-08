//! One closed reader enum at the consumer boundary.
//!
//! A player may receive Matroska directly and TS or fragmented MP4 through HLS,
//! so the reader is chosen at open time from probed bytes. Each variant sits behind its format
//! feature; forwarding is one predictable `match` per step with no allocation
//! and no dynamic dispatch on the packet path. MPEG-TS joins once its reader
//! implements [`ContainerReader`]; until then [`probe`] reports it and
//! [`AnyReader::open`] refuses it explicitly.
// With no format feature the enums are empty and their forwarding is dead code.
#![cfg_attr(
    not(any(
        feature = "matroska",
        feature = "isobmff",
        feature = "ogg",
        feature = "elementary"
    )),
    allow(unused_imports, unused_variables, unreachable_code)
)]
use std::io::Read as _;
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
    feature = "ogg",
    feature = "elementary"
))]
const WINDOW: usize = 64 * 1024;

/// What the first bytes of a source look like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// An EBML header (Matroska or WebM).
    Matroska,
    /// An ISO BMFF box sequence (`ftyp`, `styp`, `moov`, `moof`, ...).
    Isobmff,
    /// 188-byte MPEG transport packets.
    Ts,
    /// An Ogg page (`OggS`).
    Ogg,
    /// Native FLAC (`fLaC`), possibly after an ID3v2 tag.
    Flac,
    /// Two consecutive MPEG audio frames, possibly after an ID3v2 tag.
    MpegAudio,
    /// Two consecutive ADTS frames.
    Adts,
    /// Two consecutive AC-3 or E-AC-3 syncframes.
    Ac3,
    /// Not recognised from these bytes.
    Unknown,
}

/// Classify a source from its first bytes. A short prefix may give `Unknown`: an
/// elementary stream (MPEG audio, ADTS, AC-3) is recognised only once the prefix holds
/// the start of its second frame, and an ID3v2 tag must fit in it whole.
pub fn probe(bytes: &[u8]) -> Probe {
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Probe::Matroska;
    }
    if let Some(kind) = bytes.get(4..8)
        && matches!(
            kind,
            b"ftyp" | b"styp" | b"moov" | b"moof" | b"mdat" | b"free" | b"skip" | b"sidx"
        )
    {
        return Probe::Isobmff;
    }
    if bytes.starts_with(b"OggS\0") {
        return Probe::Ogg;
    }
    if bytes.len() > 188 * 2 && bytes[0] == 0x47 && bytes[188] == 0x47 && bytes[376] == 0x47 {
        return Probe::Ts;
    }
    elementary(bytes)
}

/// The elementary stream `bytes` starts, when that reader is built in.
fn elementary(bytes: &[u8]) -> Probe {
    #[cfg(feature = "elementary")]
    {
        use triergon_elementary::Kind;
        match triergon_elementary::probe(bytes) {
            Some(Kind::Flac) => Probe::Flac,
            Some(Kind::MpegAudio) => Probe::MpegAudio,
            Some(Kind::Adts) => Probe::Adts,
            Some(Kind::Ac3) => Probe::Ac3,
            None => Probe::Unknown,
        }
    }
    #[cfg(not(feature = "elementary"))]
    {
        let _ = bytes;
        Probe::Unknown
    }
}

/// Every reader the facade can open, one variant per enabled format.
// Stored inline: a box would add an allocation outside the caller's budget.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum AnyReader {
    #[cfg(feature = "matroska")]
    Matroska(triergon_matroska::typed::Reader),
    #[cfg(feature = "isobmff")]
    Isobmff(triergon_isobmff::Reader),
    #[cfg(feature = "ogg")]
    Ogg(triergon_ogg::Reader),
    #[cfg(feature = "elementary")]
    Flac(triergon_elementary::FlacReader),
    #[cfg(feature = "elementary")]
    MpegAudio(triergon_elementary::MpegAudioReader),
    #[cfg(feature = "elementary")]
    Adts(triergon_elementary::AdtsReader),
    #[cfg(feature = "elementary")]
    Ac3(triergon_elementary::Ac3Reader),
}

/// The packet of whichever reader produced it.
// Stored inline: a box would allocate per packet, outside the caller's budget.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum AnyPacket {
    #[cfg(feature = "matroska")]
    Matroska(triergon_matroska::typed::Packet),
    #[cfg(feature = "isobmff")]
    Isobmff(triergon_isobmff::Packet),
    #[cfg(feature = "ogg")]
    Ogg(triergon_ogg::Packet),
    /// Every elementary stream reader's packet.
    #[cfg(feature = "elementary")]
    Elementary(triergon_elementary::Packet),
}

/// Non-packet events of whichever reader produced them.
// Stored inline for the same reason as `AnyPacket`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum AnyOther {
    #[cfg(feature = "matroska")]
    Matroska(triergon_matroska::typed::Event),
    #[cfg(feature = "isobmff")]
    Isobmff(triergon_isobmff::ReaderEvent),
    #[cfg(feature = "ogg")]
    Ogg(triergon_ogg::ReaderEvent),
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
            #[cfg(feature = "ogg")]
            Self::Ogg($inner) => $body,
            #[cfg(feature = "elementary")]
            Self::Elementary($inner) => $body,
            #[cfg(not(any(
                feature = "matroska",
                feature = "isobmff",
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
    /// Open the reader `probe` selects for `prefix`, the first bytes of the source.
    pub fn open(
        prefix: &[u8],
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
    ) -> Result<Self> {
        match probe(prefix) {
            #[cfg(feature = "matroska")]
            Probe::Matroska => Ok(Self::Matroska(triergon_matroska::typed::Reader::new(
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
            )?)),
            #[cfg(feature = "elementary")]
            Probe::MpegAudio => Ok(Self::MpegAudio(triergon_elementary::Reader::new(
                pool,
                source,
                capabilities,
                triergon_elementary::Limits::default(),
            )?)),
            #[cfg(feature = "elementary")]
            Probe::Adts => Ok(Self::Adts(triergon_elementary::Reader::new(
                pool,
                source,
                capabilities,
                triergon_elementary::Limits::default(),
            )?)),
            #[cfg(feature = "elementary")]
            Probe::Ac3 => Ok(Self::Ac3(triergon_elementary::Reader::new(
                pool,
                source,
                capabilities,
                triergon_elementary::Limits::default(),
            )?)),
            Probe::Ts => Err(Error::Unsupported(
                "the MPEG-TS reader does not implement ContainerReader yet",
            )),
            _ => {
                let _ = (pool, source, capabilities);
                Err(Error::Unsupported("source format not recognised"))
            }
        }
    }
}

/// A reader for whichever format `source` holds, fed from it: the first 64 KiB are
/// probed, then the source is read from its start.
pub fn open_blocking<S: std::io::Read + std::io::Seek>(
    mut source: S,
    pool: &Budget,
) -> Result<triergon_core::blocking::Blocking<AnyReader, S>> {
    let capabilities = triergon_core::blocking::seekable_capabilities(&mut source)?;
    let mut prefix = Vec::with_capacity(PROBE_BYTES);
    std::io::Read::by_ref(&mut source)
        .take(PROBE_BYTES as u64)
        .read_to_end(&mut prefix)
        .map_err(|e| Error::Io(e.kind()))?;
    source.rewind().map_err(|e| Error::Io(e.kind()))?;
    let reader = AnyReader::open(&prefix, pool, SourceId(1), capabilities)?;
    Ok(triergon_core::blocking::Blocking::seekable(
        reader, source, pool,
    ))
}

/// Bytes [`open_blocking`] probes.
const PROBE_BYTES: usize = 64 * 1024;

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
