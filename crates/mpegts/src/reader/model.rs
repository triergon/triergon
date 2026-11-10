use crate::clock::{ClockKey, SourceTimestamp};
use crate::compat::pes::PrivateAudioCarriage;
use crate::psi::{ElementaryStream, ProgramMap};
use crate::{SourceEpochs, TraceEvent};
use std::alloc::Layout;
use std::ops::{Deref, Range};
use std::sync::{Arc, atomic::AtomicUsize};
use triergon_core::container::{CodecId, MediaKind, PacketInfo, RandomAccess, TrackId, TrackInfo};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Reservation, Result, Time, TimeBase};
use triergon_framing::{AacAdtsCarrier, AacAdtsFrame, Ac3Frame, Ac3FrameFacts, RawAccessUnit};

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

#[derive(Debug)]
pub(super) struct DependentSyncframe {
    pub facts: Ac3FrameFacts,
    // Released as soon as this syncframe is copied into the group.
    pub pending_bytes: Option<Bytes>,
}

/// A complete AC-3 syncframe or one grouped E-AC-3 access unit.
/// Clones share the charged bytes and syncframe facts.
#[derive(Clone, Debug)]
pub struct Ac3AccessUnit {
    pub(super) first: Ac3FrameFacts,
    pub(super) bytes: Bytes,
    pub(super) dependents: Option<Shared<BudgetVec<DependentSyncframe>>>,
}

impl Ac3AccessUnit {
    pub(super) fn single(frame: Ac3Frame) -> Self {
        Self {
            first: frame.facts(),
            bytes: frame.into_bytes(),
            dependents: None,
        }
    }

    /// Exact source bytes of the complete unit, in syncframe order.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }

    /// Facts of the primary syncframe, which supplies the unit's sample span.
    pub const fn facts(&self) -> Ac3FrameFacts {
        self.first
    }

    /// Facts of every syncframe, including the primary, in source order.
    pub fn syncframes(&self) -> impl Iterator<Item = Ac3FrameFacts> + '_ {
        std::iter::once(self.first).chain(
            self.dependents
                .iter()
                .flat_map(|frames| frames.as_slice().iter().map(|frame| frame.facts)),
        )
    }

    /// The primary syncframe's raw trailing word.
    pub fn crc2_word(&self) -> u16 {
        let end = self.first.header.frame_bytes;
        u16::from_be_bytes([self.bytes[end - 2], self.bytes[end - 1]])
    }
}

/// Complete segment envelopes carried by one DVB subtitle PES.
/// The packet's source ranges preserve the original PES, including its wrapper.
#[derive(Clone, Debug)]
pub struct DvbSubtitleUnit {
    pub(super) bytes: Bytes,
    pub(super) range: Range<u64>,
    pub(super) pes_end: u64,
    pub(super) segment_count: usize,
    pub(super) truncated: bool,
}

impl DvbSubtitleUnit {
    /// Segment headers and bodies in wire order.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub const fn segment_count(&self) -> usize {
        self.segment_count
    }

    /// Range of the unfinished segment in the reader's elementary coordinates.
    pub fn truncated_tail(&self) -> Option<Range<u64>> {
        self.truncated.then_some(self.range.end..self.pes_end)
    }
}

/// One byte-aligned LOAS AudioSyncStream envelope with an opaque LATM element.
#[derive(Debug)]
pub struct LoasFrame {
    pub(super) bytes: Bytes,
    pub(super) offset: u64,
}

impl LoasFrame {
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub fn multiplex_element(&self) -> &[u8] {
        &self.bytes.as_ref()[3..]
    }

    pub fn elementary_range(&self) -> Range<u64> {
        self.offset..self.offset + self.bytes.len() as u64
    }
}

/// Original framed bytes and syntax facts, before decoder-feed conversion.
/// Framer offsets use a private concatenated elementary view (`SourceId(0)`);
/// physical transport provenance is retained by [`Packet::sources`].
#[derive(Debug)]
pub enum AccessUnit {
    Video(RawAccessUnit),
    Adts(AacAdtsFrame),
    Loas(LoasFrame),
    /// An opaque ADTS parser observation with its original source diagnostic.
    AdtsFragment(AacAdtsCarrier),
    Ac3(Ac3AccessUnit),
    DvbSubtitle(DvbSubtitleUnit),
    /// Unfinished audio at logical EOF. This is a source fragment, not an AU.
    IncompleteAudio {
        range: Range<u64>,
        bytes: Bytes,
        reason: Error,
    },
}

impl AccessUnit {
    fn stored_bytes(&self) -> &Bytes {
        match self {
            Self::Adts(frame) => frame.source_bytes(),
            Self::AdtsFragment(fragment) => fragment.source_bytes(),
            Self::Loas(frame) => frame.bytes(),
            Self::Ac3(frame) => frame.bytes(),
            Self::DvbSubtitle(unit) => unit.bytes(),
            Self::IncompleteAudio { bytes, .. } => bytes,
            Self::Video(_) => unreachable!("video access units use assembled bytes"),
        }
    }

    pub(super) fn range(&self) -> Range<u64> {
        match self {
            Self::Video(unit) => unit.range.clone(),
            Self::Adts(frame) => {
                frame.source().offset..frame.source().offset + frame.source_bytes().len() as u64
            }
            Self::Loas(frame) => frame.elementary_range(),
            Self::AdtsFragment(fragment) => {
                let start = fragment.source().offset;
                start..start + fragment.source_bytes().len() as u64
            }
            Self::Ac3(frame) => {
                frame.facts().offset..frame.facts().offset + frame.bytes().len() as u64
            }
            Self::DvbSubtitle(unit) => unit.range.clone(),
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
            Self::Video(_)
            | Self::Loas(_)
            | Self::DvbSubtitle(_)
            | Self::AdtsFragment(_)
            | Self::IncompleteAudio { .. } => Ok(None),
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
    pub video_bytes: Option<Bytes>,
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
        self.0
            .video_bytes
            .as_ref()
            .unwrap_or_else(|| self.0.unit.stored_bytes())
    }
}

impl PacketInfo for Packet {
    fn track(&self) -> TrackId {
        self.0.track.id()
    }
    fn payload(&self) -> &[u8] {
        self.bytes().as_ref()
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

#[derive(Debug)]
pub(super) struct UnframedData {
    pub track: Track,
    pub range: Range<u64>,
    pub bytes: Bytes,
    pub sources: BudgetVec<PayloadSource>,
    pub clock: Option<PacketClock>,
    pub epochs: SourceEpochs,
}

/// A validated AVC or HEVC PES byte run with invalid leading bytes and no
/// Annex B start code. Its original payload and first source anchor are
/// retained at logical EOF. Clones share the charged storage.
#[derive(Clone, Debug)]
pub struct IncompleteVideo(pub(super) Shared<UnframedData>);

impl IncompleteVideo {
    pub fn track_info(&self) -> &Track {
        &self.0.track
    }
    pub fn elementary_range(&self) -> Range<u64> {
        self.0.range.clone()
    }
    pub fn bytes(&self) -> &Bytes {
        &self.0.bytes
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
}

/// Opaque bytes from one validated private-audio PES with no syncword.
/// Clones share the charged bytes, track, original PES ranges and clock.
#[derive(Clone, Debug)]
pub struct UnframedAudio {
    pub(super) data: Shared<UnframedData>,
    pub(super) carriage: PrivateAudioCarriage,
}

impl UnframedAudio {
    /// Carriage selected by the compatibility policy.
    pub fn carriage(&self) -> PrivateAudioCarriage {
        self.carriage
    }
    pub fn track_info(&self) -> &Track {
        &self.data.track
    }
    pub fn elementary_range(&self) -> Range<u64> {
        self.data.range.clone()
    }
    pub fn bytes(&self) -> &Bytes {
        &self.data.bytes
    }
    pub fn sources(&self) -> &[PayloadSource] {
        self.data.sources.as_slice()
    }
    pub fn clock(&self) -> Option<&PacketClock> {
        self.data.clock.as_ref()
    }
    pub fn epochs(&self) -> SourceEpochs {
        self.data.epochs
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
    /// The final unframed video run under the explicit EOF reporting policy.
    IncompleteVideo(IncompleteVideo),
    /// Original LATM PES bytes under the explicit PES packet policy.
    LatmPes(crate::compat::latm::PesPacket),
    /// Opaque ADTS bytes delimited by the compatibility parser. The packet
    /// retains its PES extents and any one-use clock without a sample duration.
    AdtsFragment(Packet),
    /// Complete segments before an overlong final segment in the source PES.
    DvbSubtitlePrefix(Packet),
    /// An opaque private-audio PES admitted by the explicit EOF policy.
    UnframedAudio(UnframedAudio),
}
