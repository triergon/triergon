use crate::clock::{ClockKey, SourceTimestamp};
use crate::psi::{ElementaryStream, ProgramMap};
use crate::{SourceEpochs, TraceEvent};
use std::alloc::Layout;
use std::ops::{Deref, Range};
use std::sync::{Arc, atomic::AtomicUsize};
use triergon_core::container::{CodecId, MediaKind, PacketInfo, RandomAccess, TrackId, TrackInfo};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Reservation, Result, Time, TimeBase};
use triergon_framing::{AacAdtsFrame, Ac3Frame, RawAccessUnit};

#[derive(Debug)]
struct Owned<T> {
    value: T,
    _charge: Reservation,
}

#[derive(Debug)]
pub(super) struct Shared<T>(Arc<Owned<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.value
    }
}

impl<T> Shared<T> {
    pub(super) fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(super) fn allocation_bytes() -> Result<usize> {
        let (layout, _) = Layout::new::<[AtomicUsize; 2]>()
            .extend(Layout::new::<Owned<T>>())
            .map_err(|_| Error::Overflow)?;
        Ok(layout.pad_to_align().size())
    }
    pub(super) fn reserve(pool: &Budget) -> Result<Reservation> {
        pool.reserve(Self::allocation_bytes()?)
    }
    pub(super) fn new(value: T, charge: Reservation) -> Self {
        Self(Arc::new(Owned {
            value,
            _charge: charge,
        }))
    }
}

/// An original transport event. Clones share its charged storage, including
/// every ordered descriptor, source range, rejected carrier and PES span.
#[derive(Clone, Debug)]
pub struct SourceEvent(pub(super) Shared<TraceEvent>);

impl SourceEvent {
    pub fn event(&self) -> &TraceEvent {
        &self.0
    }
    pub(super) fn map(&self) -> &ProgramMap {
        match self.event() {
            TraceEvent::ProgramMap { map, .. } => map,
            _ => unreachable!("track owns a program map"),
        }
    }
    pub(super) fn pes(&self) -> &crate::Pes {
        match self.event() {
            TraceEvent::Pes(pes) => pes,
            _ => unreachable!("payload source owns a PES"),
        }
    }
}

/// One stream in an active PMT. Packets retain the exact map which described
/// them, even after reconfiguration, reset or reader closure.
#[derive(Clone, Debug)]
pub struct Track {
    pub(super) source: SourceEvent,
    pub(super) index: usize,
    pub(super) epoch: u64,
}

impl Track {
    pub fn stream(&self) -> &ElementaryStream {
        &self.source.map().streams.as_slice()[self.index]
    }
    pub fn program_map(&self) -> &ProgramMap {
        self.source.map()
    }
    pub fn program(&self) -> u16 {
        self.program_map().header.extension
    }
    /// The elementary route's configuration epoch. Reused routes keep it across
    /// PMT version or PCR binding changes. In-band parameter sets remain source
    /// NALs and do not advance this value.
    pub fn configuration_epoch(&self) -> u64 {
        self.epoch
    }
}

impl TrackInfo for Track {
    fn id(&self) -> TrackId {
        self.stream().id()
    }
    fn kind(&self) -> MediaKind {
        self.stream().kind()
    }
    fn codec_tag(&self) -> &[u8] {
        self.stream().codec_tag()
    }
    fn codec(&self) -> CodecId<'_> {
        TrackInfo::codec(self.stream())
    }
    fn configuration(&self) -> &[u8] {
        self.stream().configuration()
    }
}

/// Intersection of an AU with an original PES in this reader's concatenated
/// elementary-byte coordinates. The PES retains its own physical ranges,
/// elementary offset and uninterrupted-byte-run epoch unchanged.
#[derive(Clone, Debug)]
pub struct PayloadSource {
    pub elementary_range: Range<u64>,
    /// Exact intersection within the original PES payload, independent of
    /// the elementary view's restart position.
    pub payload_range: Range<usize>,
    pub pes: SourceEvent,
}

/// The most recent applicable PES anchor at an AU's first start-code byte
/// (or audio sync byte). An anchor is used once; one inside a frame waits for
/// the next frame start. Missing DTS is never inferred from PTS.
#[derive(Clone, Debug)]
pub struct PacketClock {
    pub key: ClockKey,
    /// Invalid source signalling quarantined interpretation at this PES.
    /// Raw fields remain available, while lifted timestamps stay unresolved.
    pub quarantined: bool,
    pub pts: SourceTimestamp,
    pub dts: SourceTimestamp,
    pub pes: SourceEvent,
}

#[derive(Clone, Copy)]
pub(super) struct ClockContext {
    pub key: ClockKey,
    pub quarantined: bool,
}

/// Original framed bytes and syntax facts, before decoder-feed conversion.
/// Framer offsets use a private concatenated elementary view (`SourceId(0)`);
/// physical transport provenance is retained by [`Packet::sources`].
#[derive(Debug)]
pub enum AccessUnit {
    Video(RawAccessUnit),
    Adts(AacAdtsFrame),
    Ac3(Ac3Frame),
    /// Unfinished audio at logical EOF. This is a source fragment, not an AU.
    IncompleteAudio {
        range: Range<u64>,
        bytes: Bytes,
        reason: Error,
    },
}

impl AccessUnit {
    pub(super) fn range(&self) -> Range<u64> {
        match self {
            Self::Video(unit) => unit.range.clone(),
            Self::Adts(frame) => {
                frame.source().offset..frame.source().offset + frame.source_bytes().len() as u64
            }
            Self::Ac3(frame) => {
                frame.facts().offset..frame.facts().offset + frame.bytes().len() as u64
            }
            Self::IncompleteAudio { range, .. } => range.clone(),
        }
    }
    pub(super) fn anchor_offset(&self) -> u64 {
        match self {
            Self::Video(unit) => {
                unit.nals()
                    .first()
                    .map_or(unit.range.start, |nal| match nal.facts().framing {
                        triergon_framing::NalFraming::AnnexB { prefix_offset, .. } => prefix_offset,
                        triergon_framing::NalFraming::LengthPrefixed { .. } => {
                            unreachable!("raw reader uses Annex B")
                        }
                    })
            }
            _ => self.range().start,
        }
    }
    pub(super) fn duration(&self) -> Result<Option<Time>> {
        match self {
            Self::Video(_) | Self::IncompleteAudio { .. } => Ok(None),
            Self::Adts(frame) => {
                let rate = frame
                    .header()
                    .coded_frequency_hz()
                    .ok_or(Error::Invalid("ADTS frequency"))?;
                Ok(Some(Time::new(1024, u128::from(rate))?))
            }
            Self::Ac3(frame) => Ok(Some(frame.facts().header.sample_span()?)),
        }
    }
}

#[derive(Debug)]
pub(super) struct PacketData {
    pub track: Track,
    pub unit: AccessUnit,
    pub bytes: Bytes,
    pub sources: BudgetVec<PayloadSource>,
    pub clock: Option<PacketClock>,
    pub epochs: SourceEpochs,
    pub duration: Option<Time>,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration90: Option<i64>,
}

/// One retained framed source unit. The neutral packet stream contains only
/// complete AUs; header-only units use [`ReaderEvent::VideoHeaders`] and unfinished
/// audio uses [`ReaderEvent::IncompleteAudio`]. Cloning
/// shares charged storage, without cloning an input-sized vector or losing its charge.
#[derive(Clone, Debug)]
pub struct Packet(pub(super) Shared<PacketData>);

impl Packet {
    pub fn track_info(&self) -> &Track {
        &self.0.track
    }
    pub fn access_unit(&self) -> &AccessUnit {
        &self.0.unit
    }
    pub fn sources(&self) -> &[PayloadSource] {
        self.0.sources.as_slice()
    }
    pub fn clock(&self) -> Option<&PacketClock> {
        self.0.clock.as_ref()
    }
    pub fn epochs(&self) -> SourceEpochs {
        self.0.epochs
    }
    pub fn elementary_range(&self) -> Range<u64> {
        self.0.unit.range()
    }
    /// Exact coded audio duration, including rates not integral in 90 kHz.
    pub fn sample_duration(&self) -> Option<Time> {
        self.0.duration
    }
    pub fn bytes(&self) -> &Bytes {
        &self.0.bytes
    }
}

impl PacketInfo for Packet {
    fn track(&self) -> TrackId {
        self.0.track.id()
    }
    fn payload(&self) -> &[u8] {
        self.0.bytes.as_ref()
    }
    fn time_base(&self) -> TimeBase {
        TimeBase::new(1, 90_000).expect("nonzero TS clock")
    }
    fn pts(&self) -> Option<i64> {
        self.0.pts
    }
    fn dts(&self) -> Option<i64> {
        self.0.dts
    }
    fn duration(&self) -> Option<i64> {
        self.0.duration90
    }
    fn random_access(&self) -> RandomAccess {
        match &self.0.unit {
            AccessUnit::Video(unit) if unit.random_access => RandomAccess::Sync,
            AccessUnit::Video(unit) if unit.has_vcl => RandomAccess::NotSync,
            _ => RandomAccess::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ReaderEvent {
    Source(SourceEvent),
    /// A completed PES whose source header predates a program boundary.
    /// Both source events remain available for recovery without asserting an AU
    /// association across the changed map or clock binding.
    InterruptedPes {
        source: SourceEvent,
        boundary: SourceEvent,
    },
    /// No complete AU framer is admitted for this stream. Its PMT and PES
    /// still surface as source events; no PES is relabelled as an AU.
    Unsupported {
        track: Track,
        error: Error,
    },
    /// A PMT, clock or byte-run boundary interrupted unpublished elementary
    /// bytes. Their original PES events were delivered unchanged. A loss
    /// boundary is not logical EOF and never certifies a partial AU.
    Interrupted {
        track: Track,
        elementary_range: Range<u64>,
        cause: SourceEvent,
    },
    /// An Annex-B source group without an admitted primary/base-layer picture.
    /// Its original NALs remain inspectable outside the neutral packet stream.
    VideoHeaders(Packet),
    /// Original unfinished audio bytes at logical EOF, with validated PES
    /// provenance and an optional one-use source clock. No complete AU is asserted.
    IncompleteAudio(Packet),
}
