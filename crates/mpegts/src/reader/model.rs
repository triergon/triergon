use crate::SourceEpochs;
use crate::clock::{ClockKey, SourceTimestamp};
use crate::compat::pes::PrivateAudioCarriage;
use crate::psi::{ElementaryStream, ProgramMap};
use crate::stream::{CarrierObservation, TraceEvent};
use crate::transport::{ContinuityOutcome, TransportPacket};
use std::alloc::Layout;
use std::ops::{Deref, Range};
use std::sync::{Arc, atomic::AtomicUsize};
use triergon_core::container::{CodecId, MediaKind, PacketInfo, RandomAccess, TrackId, TrackInfo};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Reservation, Result, Time, TimeBase};
use triergon_framing::{
    aac::{AacAdtsCarrier, AacAdtsFrame},
    ac3::{Ac3Frame, Ac3FrameFacts},
    mpeg_audio::MpegAudioHeader,
    nal::RawAccessUnit,
};

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

/// An unsplit PES observation with its exact source and clock facts.
#[derive(Clone, Debug)]
pub struct LatmPes {
    track: Track,
    source: SourceEvent,
    clock_key: ClockKey,
    clock_quarantined: bool,
}

impl LatmPes {
    pub(crate) fn new(
        track: Track,
        source: SourceEvent,
        clock_key: ClockKey,
        clock_quarantined: bool,
    ) -> Self {
        Self {
            track,
            source,
            clock_key,
            clock_quarantined,
        }
    }

    /// The retained PMT entry that identifies this LATM elementary stream.
    pub fn track_info(&self) -> &Track {
        &self.track
    }

    /// The source event owning the original LATM header, payload spans and byte ranges.
    pub fn source(&self) -> &SourceEvent {
        &self.source
    }

    /// Program, PCR PID and source epoch that apply to the encoded timestamps.
    pub fn clock_key(&self) -> ClockKey {
        self.clock_key
    }

    /// Whether the program clock was quarantined at this PES, leaving its timestamps
    /// unresolved.
    pub fn clock_is_quarantined(&self) -> bool {
        self.clock_quarantined
    }

    /// The complete validated LATM PES, with its wire header and source ranges.
    // Typed only with the PES shared apart from `TraceEvent::Pes`, as `SourceEvent::pes`.
    pub fn pes(&self) -> &crate::pes::Pes {
        let TraceEvent::Pes(pes) = self.source.event() else {
            unreachable!("LATM packet owns its source PES");
        };
        pes
    }
}

/// A shared original source event. Clones share its charged storage, including
/// every ordered descriptor, source range, rejected carrier and PES span.
#[derive(Clone, Debug)]
pub struct SourceEvent(pub(super) Shared<TraceEvent>);

impl SourceEvent {
    /// Borrow the event and its original source facts for as long as this handle is held.
    pub fn event(&self) -> &TraceEvent {
        &self.0
    }
    // The typed form shares the map apart from its event: `TraceEvent::ProgramMap` (public) holding
    // it behind a charged handle, or a second charged copy of every accepted map; not built.
    pub(super) fn map(&self) -> &ProgramMap {
        match self.event() {
            TraceEvent::ProgramMap { map, .. } => map,
            _ => unreachable!("track owns a program map"),
        }
    }
    // A checked PES view measured +0.70%, a validated PES source beside the event +0.040% (one
    // match moves to admission; the typed form shares the PES apart from `TraceEvent::Pes`).
    #[expect(
        clippy::expect_used,
        reason = "pieces and payload sources are built only from PES events"
    )]
    pub(super) fn pes(&self) -> &crate::pes::Pes {
        self.event()
            .pes_source()
            .expect("payload source owns a PES")
    }
}

/// One stream in an active PMT. Packets retain the exact map which described
/// them, even after reconfiguration, reset or reader closure.
#[derive(Clone, Debug)]
pub struct Track {
    pub(super) source: SourceEvent,
    pub(super) index: u16,
    pub(super) epoch: u64,
    pub(super) unlabelled_dts_core: bool,
}

impl Track {
    /// The elementary PID, stream type and descriptors that configure this track.
    pub fn stream(&self) -> &ElementaryStream {
        &self.source.map().streams.as_slice()[usize::from(self.index)]
    }
    /// The retained program map for the track’s configuration, valid after reconfiguration.
    pub fn program_map(&self) -> &ProgramMap {
        self.source.map()
    }
    /// The 16-bit program number from the retained PMT table extension.
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
        if self.unlabelled_dts_core {
            MediaKind::Audio
        } else {
            self.stream().kind()
        }
    }
    fn codec_tag(&self) -> &[u8] {
        self.stream().codec_tag()
    }
    fn codec(&self) -> CodecId {
        if self.unlabelled_dts_core {
            CodecId::Dts
        } else {
            TrackInfo::codec(self.stream())
        }
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
    /// Half-open byte range in the track’s concatenated PES payload.
    pub elementary_range: Range<u64>,
    /// Half-open byte range relative to the original PES payload’s first byte.
    pub payload_range: Range<usize>,
    /// Retains the PES header, source ranges and epoch of these payload bytes.
    pub pes: SourceEvent,
}

#[derive(Debug)]
pub(super) enum PayloadSources {
    Inline(Option<PayloadSource>),
    Table(BudgetVec<PayloadSource>),
}

impl PayloadSources {
    pub(super) fn as_slice(&self) -> &[PayloadSource] {
        match self {
            Self::Inline(source) => source.as_slice(),
            Self::Table(sources) => sources.as_slice(),
        }
    }

    // Cloning both sources into a reserved table measured +0.069 to +0.109%: `BudgetVec::try_push`
    // leaves the lane step. The infallible push into reserved capacity this needs is not in core.
    #[expect(
        clippy::expect_used,
        reason = "pushing the two sources one by one measured +0.08% (BudgetVec::try_push out of line)"
    )]
    pub(super) fn try_push(&mut self, pool: &Budget, source: PayloadSource) -> Result<()> {
        match self {
            Self::Inline(first @ None) => *first = Some(source),
            Self::Inline(first) => {
                // Reserve both entries before moving the retained first source.
                let mut table = BudgetVec::new(pool);
                let mut second = Some(source);
                table.try_extend_with(2, |index| {
                    if index == 0 {
                        first.take().expect("first PES source")
                    } else {
                        second.take().expect("second PES source")
                    }
                })?;
                *self = Self::Table(table);
            }
            Self::Table(table) => {
                table.try_reserve_exact(1)?;
                table.try_push(source).map_err(|error| error.error)?;
            }
        }
        Ok(())
    }
}

/// The most recent applicable PES anchor at the first byte of a source unit.
/// An anchor is used once; one inside a frame waits for
/// the next frame start. Missing DTS is never inferred from PTS.
#[derive(Clone, Debug)]
pub struct PacketClock {
    /// Program, PCR PID and epoch used to interpret both timestamps.
    pub key: ClockKey,
    /// Invalid source signalling quarantined interpretation at this PES.
    /// Raw fields remain available, while lifted timestamps stay unresolved.
    pub quarantined: bool,
    /// The anchoring PTS, lifted under the reader's time policy.
    pub pts: SourceTimestamp,
    /// The anchoring DTS, lifted under the reader's time policy; never inferred from the PTS.
    pub dts: SourceTimestamp,
    /// The PES whose header supplied the timestamps.
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

/// A complete AC-3 syncframe or an E-AC-3 access-unit group.
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

    /// Facts of the first primary syncframe.
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

    /// Combined primary sample span; dependent substreams add no time.
    pub fn sample_span(&self) -> Result<Time> {
        if self.first.header.blocks == 6 {
            return self.first.header.sample_span();
        }
        let blocks: u16 = self
            .syncframes()
            .filter(|facts| {
                !matches!(
                    facts.header.details,
                    triergon_framing::ac3::Ac3FrameDetails::Eac3 { stream_type: 1, .. }
                )
            })
            .map(|facts| u16::from(facts.header.blocks))
            .sum();
        Time::new(
            i128::from(blocks) * 256,
            u128::from(self.first.header.sample_rate_hz),
        )
    }
}

/// The complete segment envelopes carried by one DVB subtitle PES.
/// The original wrapper and any trailing bytes remain in the packet's PES source.
#[derive(Clone, Debug)]
pub struct DvbSubtitleUnit {
    pub(super) bytes: Bytes,
    pub(super) range: Range<u64>,
    pub(super) pes_end: u64,
    pub(super) segment_count: usize,
    pub(super) truncated: bool,
}

impl DvbSubtitleUnit {
    /// Complete segment headers and bodies in wire order.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Complete DVB segment envelopes, excluding the wrapper, end marker and unfinished tail.
    pub const fn segment_count(&self) -> usize {
        self.segment_count
    }

    /// Half-open byte range of the unfinished final segment, when the tail policy reports one.
    /// Offsets are relative to the track’s concatenated PES payload.
    pub fn truncated_tail(&self) -> Option<Range<u64>> {
        self.truncated.then_some(self.range.end..self.pes_end)
    }
}

/// One byte-aligned LOAS AudioSyncStream envelope. Its LATM multiplex element
/// is opaque, so no decoder configuration, sample count or duration is inferred.
#[derive(Debug)]
pub struct LoasFrame {
    pub(super) bytes: Bytes,
    pub(super) offset: u64,
}

impl LoasFrame {
    /// The whole envelope: the 3-byte sync and length header, then the multiplex element.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// The LATM multiplex element after the 3-byte envelope header.
    pub fn multiplex_element(&self) -> &[u8] {
        &self.bytes.as_ref()[3..]
    }

    /// Half-open byte range in the track’s concatenated PES payload.
    pub fn elementary_range(&self) -> Range<u64> {
        self.offset..self.offset + self.bytes.len() as u64
    }
}

/// One MPEG-1 or MPEG-2 audio frame from an audio PES stream.
#[derive(Debug)]
pub struct MpegAudioFrame {
    pub(super) bytes: Bytes,
    pub(super) offset: u64,
    pub(super) header: MpegAudioHeader,
}

impl MpegAudioFrame {
    /// Complete frame, including its MPEG audio header.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Header facts including layer, sampling rate in hertz and coded samples per frame.
    pub const fn header(&self) -> &MpegAudioHeader {
        &self.header
    }

    /// Half-open byte range in the track’s concatenated PES payload.
    pub fn elementary_range(&self) -> Range<u64> {
        self.offset..self.offset + self.bytes.len() as u64
    }
}

/// One complete core-bearing DTS access unit from a DVB-signalled stream.
#[derive(Debug)]
pub struct DtsAudioFrame {
    pub(super) bytes: Bytes,
    pub(super) core_bytes: usize,
    pub(super) offset: u64,
    pub(super) duration: Time,
}

impl DtsAudioFrame {
    /// Complete access unit, including any extension substreams.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Exact source bytes of the core substream.
    pub fn core_bytes(&self) -> &[u8] {
        &self.bytes.as_ref()[..self.core_bytes]
    }

    /// Exact duration of the core coding window.
    pub const fn sample_span(&self) -> Time {
        self.duration
    }

    /// Half-open byte range in the track’s concatenated PES payload.
    pub fn elementary_range(&self) -> Range<u64> {
        self.offset..self.offset + self.bytes.len() as u64
    }
}

/// One DTS-HD access unit containing extension substreams without a DTS core.
#[derive(Debug)]
pub struct DtsExtensionFrame {
    pub(super) bytes: Bytes,
    pub(super) offset: u64,
    pub(super) duration: Option<Time>,
}

impl DtsExtensionFrame {
    /// Original extension headers, alignment bytes and asset payloads.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Duration from the lowest-index extension's current static metadata, if known.
    pub const fn sample_span(&self) -> Option<Time> {
        self.duration
    }

    /// Half-open byte range in the track’s concatenated PES payload.
    pub fn elementary_range(&self) -> Range<u64> {
        self.offset..self.offset + self.bytes.len() as u64
    }
}

/// Picture coding type from H.262 section 6.2.3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MpegPictureCoding {
    /// An independently coded I picture.
    #[default]
    Intra,
    /// A P picture using forward prediction.
    Predictive,
    /// A B picture using bidirectional prediction.
    Bidirectional,
}

/// A picture with sequence setup and its original coded slices.
#[derive(Debug)]
pub struct MpegVideoPicture {
    pub(super) bytes: Bytes,
    pub(super) range: Range<u64>,
    /// Coded width in luma samples.
    pub width: u16,
    /// Coded height in luma samples.
    pub height: u16,
    /// Ten-bit display-order reference within the group.
    pub temporal_reference: u16,
    /// Prediction class declared by the picture header.
    pub coding: MpegPictureCoding,
    /// Picture start-code offset in the private elementary view.
    pub picture_offset: u64,
    /// Number of coded slice start codes.
    pub slice_count: u8,
    /// Whether the sequence uses MPEG-2 syntax.
    pub mpeg2: bool,
    pub(super) duration: Time,
}

impl MpegVideoPicture {
    /// Original picture and sequence bytes, without the PES header.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// Byte range of the picture in the reader’s elementary view.
    pub fn elementary_range(&self) -> Range<u64> {
        self.range.clone()
    }
}

/// Original framed bytes and syntax facts, before decoder-feed conversion.
/// Framer offsets use a private concatenated elementary view (`SourceId(0)`);
/// physical transport provenance is retained by [`Packet::sources`].
#[derive(Debug)]
pub enum AccessUnit {
    /// An AVC or HEVC access unit as framed from Annex B.
    Video(RawAccessUnit),
    /// One MPEG-1 or MPEG-2 picture with sequence setup.
    MpegVideo(MpegVideoPicture),
    /// An AAC frame with its ADTS header.
    Adts(AacAdtsFrame),
    /// A LOAS envelope with an opaque LATM multiplex element.
    Loas(LoasFrame),
    /// An MPEG-1 or MPEG-2 audio frame.
    MpegAudio(MpegAudioFrame),
    /// A DTS access unit with a core substream.
    Dts(DtsAudioFrame),
    /// A DTS-HD access unit of extension substreams without a core.
    DtsExtension(DtsExtensionFrame),
    /// An opaque ADTS parser observation with its original source diagnostic.
    AdtsFragment(AacAdtsCarrier),
    /// An AC-3 syncframe or a DVB E-AC-3 syncframe group.
    Ac3(Ac3AccessUnit),
    /// The complete segments of one DVB subtitle PES.
    DvbSubtitle(DvbSubtitleUnit),
    /// Unfinished audio at logical EOF. This is a source fragment, not an AU.
    IncompleteAudio {
        /// Extent of the unfinished bytes in the track's concatenated PES payload.
        range: Range<u64>,
        /// The unfinished bytes.
        bytes: Bytes,
        /// The framer's error that the bytes would otherwise have raised.
        reason: Error,
    },
}

impl AccessUnit {
    // Holding every packet's bytes in `PacketData` (an `Arc` clone per audio frame) measured
    // +0.63% on DTS-HD grouped reads.
    fn stored_bytes(&self) -> &Bytes {
        match self {
            Self::MpegVideo(frame) => frame.bytes(),
            Self::Adts(frame) => frame.source_bytes(),
            Self::AdtsFragment(fragment) => fragment.source_bytes(),
            Self::Loas(frame) => frame.bytes(),
            Self::MpegAudio(frame) => frame.bytes(),
            Self::Dts(frame) => frame.bytes(),
            Self::DtsExtension(frame) => frame.bytes(),
            Self::Ac3(frame) => frame.bytes(),
            Self::DvbSubtitle(unit) => unit.bytes(),
            Self::IncompleteAudio { bytes, .. } => bytes,
            Self::Video(_) => unreachable!("video access units use assembled bytes"),
        }
    }

    pub(super) fn range(&self) -> Range<u64> {
        match self {
            Self::Video(unit) => unit.range.clone(),
            Self::MpegVideo(unit) => unit.range.clone(),
            Self::Adts(frame) => {
                frame.source().offset..frame.source().offset + frame.source_bytes().len() as u64
            }
            Self::AdtsFragment(fragment) => {
                let start = fragment.source().offset;
                start..start + fragment.source_bytes().len() as u64
            }
            Self::Loas(frame) => frame.elementary_range(),
            Self::MpegAudio(frame) => frame.elementary_range(),
            Self::Dts(frame) => frame.elementary_range(),
            Self::DtsExtension(frame) => frame.elementary_range(),
            Self::Ac3(frame) => {
                frame.facts().offset..frame.facts().offset + frame.bytes().len() as u64
            }
            Self::DvbSubtitle(unit) => unit.range.clone(),
            Self::IncompleteAudio { range, .. } => range.clone(),
        }
    }
    pub(super) fn anchor_offset(&self) -> u64 {
        match self {
            Self::Video(unit) => unit
                .nals()
                .first()
                .map_or(unit.range.start, |nal| nal.facts().framing.offset),
            _ => self.range().start,
        }
    }
    pub(super) fn duration(&self) -> Result<Option<Time>> {
        match self {
            Self::MpegVideo(unit) => Ok(Some(unit.duration)),
            Self::Video(_)
            | Self::Loas(_)
            | Self::DvbSubtitle(_)
            | Self::IncompleteAudio { .. }
            | Self::AdtsFragment(_) => Ok(None),
            Self::Adts(frame) => {
                let rate = frame
                    .header()
                    .coded_frequency_hz()
                    .ok_or(Error::Invalid("ADTS frequency"))?;
                Ok(Some(Time::new(1024, u128::from(rate))?))
            }
            Self::Ac3(frame) => Ok(Some(frame.sample_span()?)),
            Self::Dts(frame) => Ok(Some(frame.sample_span())),
            Self::DtsExtension(frame) => Ok(frame.sample_span()),
            Self::MpegAudio(frame) => Ok(Some(Time::new(
                i128::from(frame.header.samples_per_frame()),
                u128::from(frame.header.sample_rate),
            )?)),
        }
    }
}

#[derive(Debug)]
pub(super) struct PacketData {
    pub track: Track,
    pub unit: AccessUnit,
    pub video_bytes: Option<Bytes>,
    pub sources: PayloadSources,
    pub clock: Option<PacketClock>,
    pub epochs: SourceEpochs,
    pub duration: Option<Time>,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration90: Option<i64>,
    pub recovered_adts: bool,
}

/// One retained framed source unit. The neutral packet stream contains only
/// complete AUs; header-only units use [`ReaderEvent::VideoHeaders`] and unfinished
/// audio and DVB subtitle prefixes use [`ReaderEvent::IncompleteAudio`] and
/// [`ReaderEvent::DvbSubtitlePrefix`], respectively. Cloning
/// shares charged storage, without cloning an input-sized vector or losing its charge.
#[derive(Clone, Debug)]
pub struct Packet(pub(super) Shared<PacketData>);

impl Packet {
    /// The track configuration that applies to this packet, including its retained PMT.
    pub fn track_info(&self) -> &Track {
        &self.0.track
    }
    /// Framing syntax and source payload for this unit, before decoder-feed conversion.
    pub fn access_unit(&self) -> &AccessUnit {
        &self.0.unit
    }
    /// Intersections of the unit with its original PES payloads, in elementary byte order.
    pub fn sources(&self) -> &[PayloadSource] {
        self.0.sources.as_slice()
    }
    /// The applicable PES anchor, preserving whether its header had PTS and DTS.
    pub fn clock(&self) -> Option<&PacketClock> {
        self.0.clock.as_ref()
    }
    /// The clock and configuration epochs in force for this unit.
    pub fn epochs(&self) -> SourceEpochs {
        self.0.epochs
    }
    /// Half-open byte range of the unit in the track’s concatenated PES payload.
    pub fn elementary_range(&self) -> Range<u64> {
        self.0.unit.range()
    }
    /// Exact coded audio duration in seconds, even when it is fractional in 90 kHz ticks.
    /// Returns `None` when the unit supplies no usable sample count and rate.
    pub fn sample_duration(&self) -> Option<Time> {
        self.0.duration
    }
    /// The unit's bytes: the assembled access unit for video, the framed source bytes otherwise.
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
        TimeBase::HZ_90000
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
            AccessUnit::MpegVideo(unit) if unit.coding == MpegPictureCoding::Intra => {
                RandomAccess::Sync
            }
            AccessUnit::MpegVideo(_) => RandomAccess::NotSync,
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

/// A run of validated AVC or HEVC PES payload with invalid leading bytes and no
/// Annex B start code: every complete PES of the track between two ends of its byte
/// run. It is reported when the source ends the run, with its original payload and
/// first source anchor. Clones share the charged storage.
#[derive(Clone, Debug)]
pub struct IncompleteVideo {
    pub(super) data: Shared<UnframedData>,
    pub(super) ended_by: Option<SourceEvent>,
}

impl IncompleteVideo {
    /// The retained track configuration at the start of the unframed video run.
    pub fn track_info(&self) -> &Track {
        &self.data.track
    }
    /// Half-open byte range of the run in the track’s concatenated PES payload.
    pub fn elementary_range(&self) -> Range<u64> {
        self.data.range.clone()
    }
    /// Original AVC or HEVC PES payload bytes, including the invalid leading bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.data.bytes
    }
    /// Intersections with original PES payloads, ordered by elementary byte offset.
    pub fn sources(&self) -> &[PayloadSource] {
        self.data.sources.as_slice()
    }
    /// The applicable PES anchor at the start of the run, when available.
    pub fn clock(&self) -> Option<&PacketClock> {
        self.data.clock.as_ref()
    }
    /// The clock and configuration epochs in force for the run.
    pub fn epochs(&self) -> SourceEpochs {
        self.data.epochs
    }
    /// The source event that ended the run: a fault, a signalled discontinuity, a new
    /// clock epoch, a table change or the first PES of another byte run. `None` for a run
    /// that reached the logical end of the source.
    pub fn ended_by(&self) -> Option<&SourceEvent> {
        self.ended_by.as_ref()
    }
}

/// A validated private PES whose payload has no AC-3 or E-AC-3 syncword. It is
/// reported when the source ends its byte run, with the source track, PES and clock.
/// Clones share the charged storage.
#[derive(Clone, Debug)]
pub struct UnframedAudio {
    pub(super) data: Shared<UnframedData>,
    pub(super) carriage: PrivateAudioCarriage,
    pub(super) ended_by: Option<SourceEvent>,
}

impl UnframedAudio {
    /// The private AC-3 or E-AC-3 carriage admitted from the PMT’s signalling.
    pub fn carriage(&self) -> PrivateAudioCarriage {
        self.carriage
    }
    /// The source event that ended the PES's byte run, as for [`IncompleteVideo::ended_by`];
    /// `None` at the logical end of the source.
    pub fn ended_by(&self) -> Option<&SourceEvent> {
        self.ended_by.as_ref()
    }
    /// Track configuration retained from the PMT that admitted the private PES.
    pub fn track_info(&self) -> &Track {
        &self.data.track
    }
    /// Half-open byte range in the track’s concatenated PES payload.
    pub fn elementary_range(&self) -> Range<u64> {
        self.data.range.clone()
    }
    /// Original private PES payload bytes, including the bytes without an audio syncword.
    pub fn bytes(&self) -> &Bytes {
        &self.data.bytes
    }
    /// The original PES and its contribution in track-relative elementary byte coordinates.
    pub fn sources(&self) -> &[PayloadSource] {
        self.data.sources.as_slice()
    }
    /// The applicable PES anchor, including the presence or absence of its timestamp fields.
    pub fn clock(&self) -> Option<&PacketClock> {
        self.data.clock.as_ref()
    }
    /// The clock and configuration epochs in force for the payload.
    pub fn epochs(&self) -> SourceEpochs {
        self.data.epochs
    }
}

/// A reader output other than a complete access unit.
#[derive(Clone, Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "a carrier owns charged bytes and moves through without a separate allocation"
)]
pub enum ReaderEvent {
    /// Complete original carrier, with its source bytes and parsed fields.
    Transport(TransportPacket),
    /// A source counter observation with a `First` or `Continuous` outcome.
    Continuity {
        /// The 13-bit PID of the observed carrier.
        pid: u16,
        /// The counter outcome; `First` or `Continuous`.
        outcome: ContinuityOutcome,
    },
    /// Original source event with a shared budget charge.
    Source(SourceEvent),
    /// A completed PES whose source header predates a program boundary.
    /// Both source events remain available for recovery without asserting an AU
    /// association across the changed map or clock binding.
    InterruptedPes {
        /// The completed PES with the header and original spans from before the boundary.
        source: SourceEvent,
        /// The program boundary that the PES header predates.
        boundary: SourceEvent,
    },
    /// No complete AU framer is admitted for this stream. Its PMT and PES
    /// still surface as source events; no PES is relabelled as an AU.
    Unsupported {
        /// The retained PMT entry whose carriage has no admitted access-unit framer.
        track: Track,
        /// The refusal returned when the track’s signalling was tested for framing support.
        error: Error,
    },
    /// A PMT, clock or byte-run boundary, or a discontinuity the caller declared,
    /// interrupted unpublished elementary bytes. Their original PES events were
    /// delivered unchanged. A loss boundary is not logical EOF and never certifies a
    /// partial AU.
    Interrupted {
        /// The track whose bytes were interrupted.
        track: Track,
        /// Extent of the interrupted bytes in the track's concatenated PES payload.
        elementary_range: Range<u64>,
        /// The source event that interrupted the bytes.
        cause: SourceEvent,
    },
    /// An Annex-B source group without an admitted primary/base-layer picture.
    /// Its original NALs remain inspectable outside the neutral packet stream.
    VideoHeaders(Packet),
    /// Original unfinished audio bytes at logical EOF, with validated PES
    /// provenance and an optional one-use source clock. No complete AU is asserted.
    IncompleteAudio(Packet),
    /// Opaque ADTS bytes delimited by the compatibility parser. The packet
    /// retains its PES extents and any one-use clock without a sample duration.
    AdtsFragment(Packet),
    /// ADTS parser output whose bytes or clock use a rejected unbounded PES.
    /// The unit and its provenance remain outside the neutral packet stream.
    RecoveredAdts(Packet),
    /// Original LATM PES bytes under the explicit PES packet policy.
    LatmPes(LatmPes),
    /// A proven segment prefix with an unfinished final segment in the source PES.
    DvbSubtitlePrefix(Packet),
    /// An unframed video run that the source ended, under the explicit reporting policy.
    IncompleteVideo(IncompleteVideo),
    /// One opaque private-audio PES whose byte run the source ended, under the explicit
    /// reporting policy.
    UnframedAudio(UnframedAudio),
}

impl ReaderEvent {
    pub(crate) fn carrier_observation(&self) -> Option<CarrierObservation<'_>> {
        match self {
            Self::Transport(packet) => Some(CarrierObservation::Transport(packet.into())),
            Self::Continuity { pid, outcome } => Some(CarrierObservation::Continuity {
                pid: *pid,
                outcome: *outcome,
            }),
            _ => None,
        }
    }
}
