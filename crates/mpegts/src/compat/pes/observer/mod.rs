//! The demuxer's packet operation, separate from strict source PES.
//!
//! Independently authored from H.222.0 source syntax and recorded public-API
//! observations. Assembly consumes real input
//! boundaries; no complete source/demux output is split, repeated or repaired.
use crate::compat::pes::clock::DemuxProfile;
use crate::pes::HEADER_MAX;
use crate::pes::PesHeader;
use crate::psi::ElementaryStream;
use crate::transport::TransportPacket;
use crate::{AssemblyStep, ParseLimits, SourceEpochs, SourceRange, checked_pid};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{CodecId, MediaKind, TrackInfo};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Result, SourceId, SourceLocation, WorkBudget,
};

// Public option data in the reference's doc/demuxers.texi, not a parser
// implementation. This operation leaves cap-boundary/split contexts unsupported
// rather than changing the reference buffer or guessing their packet shape.
const DEFAULT_PACKET_CAP: usize = 204_800;

mod audio;
mod body;
mod compact;
mod dvb;
mod emit;
mod header;
mod input;
mod pes_list;
mod ready;
pub use audio::{PrivateAudioCarriage, PrivateAudioDescriptors};
pub use compact::{CompactPayload, DemuxSpanView, DemuxSpanViews, PayloadCursor};
pub use dvb::DvbSubtitlesDescriptors;
pub(crate) use pes_list::ListGrowth;
use pes_list::PesList;

/// Explicit single-program/single-PID profile from validated source signalling.
/// Plain pushes reject entity transitions; the explicit media transaction and
/// selected lifecycle APIs admit only their checked boundary contracts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemuxScope {
    pub program: u16,
    pub pid: u16,
    pub stream_type: u8,
    pub stream_id: u8,
    pub entity: SourceId,
    pub epochs: SourceEpochs,
}

/// Explicit registered transport carriage, separate from the plain constructor's
/// H264-only contract. These selected stream IDs are the observed narrow profile;
/// this enum does not parse or identify codec access units/configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisteredCarriage {
    Avc,
    Hevc,
    AacAdts,
    AacLatm,
}
impl RegisteredCarriage {
    fn selected_signature(self) -> (u8, u8) {
        match self {
            Self::Avc => (0x1b, 0xe0),
            Self::Hevc => (0x24, 0xe0),
            Self::AacAdts => (0x0f, 0xc0),
            Self::AacLatm => (0x11, 0xc0),
        }
    }
}

/// Kind exposed by a selected PES view. DVB subtitling and teletext descriptors
/// identify subtitle tracks. Other type 0x06 entries need an admitted observer.
/// MPEG-1 video (0x01) stays unknown in this view. The private types 0x81 and
/// 0x87 indicate audio and 0x90 indicates presentation graphics in this view.
/// The caller supplies the observer for this active program map.
pub fn selected_track_kind(
    stream: &ElementaryStream,
    observer: Option<&DemuxObserver>,
) -> MediaKind {
    if stream.stream_type == 0x01 {
        return MediaKind::Unknown;
    }
    if matches!(stream.stream_type, 0x81 | 0x87) {
        return MediaKind::Audio;
    }
    if stream.stream_type == 0x90 {
        return MediaKind::Subtitle;
    }
    if stream.stream_type != 0x06 {
        return stream.kind();
    }
    if matches!(
        stream.codec(),
        Some(CodecId::DvbSubtitle | CodecId::DvbTeletext)
    ) {
        return MediaKind::Subtitle;
    }
    let Some(observer) = observer.filter(|observer| {
        observer.scope.pid == stream.pid && observer.scope.stream_type == stream.stream_type
    }) else {
        return MediaKind::Unknown;
    };
    if observer.private_audio.is_some() {
        MediaKind::Audio
    } else if observer.payload_format == PayloadFormat::Dvb {
        MediaKind::Subtitle
    } else {
        MediaKind::Unknown
    }
}

/// Caller links the original strict-source event. None is an unknown source byte
/// run, never a demux-invented epoch. Event ordinals are not core I/O tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceContext {
    pub source_event_ordinal: u64,
    pub epochs: SourceEpochs,
    pub byte_run_epoch: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemuxBoundary {
    FiniteLength,
    NextPesStart,
    LogicalEof,
    FiniteOverrun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemuxPayloadKind {
    DeclaredComplete,
    DeclaredIncomplete,
    Unbounded,
    HeaderlessContinuation,
}

/// A span of real input, with its original transport packet and source-event
/// context. Backing bytes and vector capacity remain shared/charged core storage.
#[derive(Debug)]
pub struct DemuxSpan {
    pub(crate) bytes: Bytes,
    pub source: SourceRange,
    pub transport: SourceRange,
    pub context: SourceContext,
}
impl AsRef<[u8]> for DemuxSpan {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}
impl DemuxSpan {
    /// Shared immutable storage for retained-capacity accounting/deduplication.
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Transfer the charged payload to another reader.
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

/// This may be a corrupt/partial/headerless demux packet, NEVER a complete
/// strict-source `Pes` or a codec access unit. Demux position can name the last
/// PES-start sync even when the physical payload starts in a later packet.
#[derive(Debug)]
pub struct DemuxPacket {
    pub profile: DemuxProfile,
    pub program: u16,
    pub pid: u16,
    pub source_origin: SourceLocation,
    pub source_epochs: SourceEpochs,
    pub position: i64,
    pub flags: u32,
    /// Demux raw90-kHz duration; zero is its unknown sentinel, not source time.
    pub duration90_raw: i64,
    /// Explicit MPEGTS stream-ID side-data byte, retained on continuations.
    pub stream_id: u8,
    pub boundary: DemuxBoundary,
    pub payload_kind: DemuxPayloadKind,
    /// Original valid causal PES header, including raw33/None. It is retained
    /// even for a continuation whose demux output has no applicable time header.
    pub source_header: PesHeader,
    pub header_source_ranges: BudgetVec<SourceRange>,
    pub time_header_applies: bool,
    pub payload_len: usize,
    pub spans: BudgetVec<DemuxSpan>,
}
impl DemuxPacket {
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
    }
    pub fn time_header(&self) -> Option<&PesHeader> {
        self.time_header_applies.then_some(&self.source_header)
    }
}

/// An explicitly compact packet with the same causal header and logical
/// carrier facts as `DemuxPacket`. Its payload exposes borrowed range views.
#[derive(Debug)]
pub struct CompactDemuxPacket {
    pub profile: DemuxProfile,
    pub program: u16,
    pub pid: u16,
    pub source_origin: SourceLocation,
    pub source_epochs: SourceEpochs,
    pub position: i64,
    pub flags: u32,
    pub duration90_raw: i64,
    pub stream_id: u8,
    pub boundary: DemuxBoundary,
    pub payload_kind: DemuxPayloadKind,
    pub source_header: PesHeader,
    pub header_source_ranges: BudgetVec<SourceRange>,
    pub time_header_applies: bool,
    pub payload_len: usize,
    pub payload: CompactPayload,
}

impl CompactDemuxPacket {
    /// Original causal header when its time fields apply to this payload.
    pub fn time_header(&self) -> Option<&PesHeader> {
        self.time_header_applies.then_some(&self.source_header)
    }
    fn into_owned(self) -> Result<DemuxPacket> {
        Ok(DemuxPacket {
            profile: self.profile,
            program: self.program,
            pid: self.pid,
            source_origin: self.source_origin,
            source_epochs: self.source_epochs,
            position: self.position,
            flags: self.flags,
            duration90_raw: self.duration90_raw,
            stream_id: self.stream_id,
            boundary: self.boundary,
            payload_kind: self.payload_kind,
            source_header: self.source_header,
            header_source_ranges: self.header_source_ranges,
            time_header_applies: self.time_header_applies,
            payload_len: self.payload_len,
            spans: self.payload.into_owned_spans()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyContinuation {
    /// Retry through the regular input state machine.
    Unhandled,
    /// Payload was retained. The observer can accept the next carrier.
    Buffered,
    /// A finite PES completed. Poll the observer before offering more input.
    Complete,
}

#[derive(Debug)]
struct CarrierLayout {
    source_offset: u64,
    payload_source_offset: u64,
    prefix_bytes: usize,
    continuity_counter: u8,
    payload_unit_start: bool,
    discontinuity: bool,
}

impl From<&TransportPacket> for CarrierLayout {
    fn from(packet: &TransportPacket) -> Self {
        Self {
            source_offset: packet.source_offset,
            payload_source_offset: packet.payload_source_offset,
            prefix_bytes: packet.prefix.as_ref().map_or(0, Bytes::len),
            continuity_counter: packet.continuity_counter,
            payload_unit_start: packet.payload_unit_start,
            discontinuity: packet.discontinuity_indicator(),
        }
    }
}

#[derive(Debug)]
struct Pending {
    carrier: CarrierLayout,
    source: Bytes,
    payload: Option<Bytes>,
    context: SourceContext,
    position: usize,
    counter_checked: bool,
    started: bool,
    body_drained: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arena {
    Header,
    Payload,
    OutputHeader,
}

#[derive(Debug)]
pub(crate) struct Growth {
    arena: Arena,
    target: usize,
    pub(crate) remaining: u64,
}

impl Growth {
    fn plan(arena: Arena, len: usize, maximum: usize, item_bytes: usize) -> Result<Self> {
        let needed = len.checked_add(1).ok_or(Error::Overflow)?;
        let target = len.saturating_mul(2).max(4).min(maximum).max(needed);
        let bytes = target
            .checked_add(len)
            .and_then(|count| count.checked_mul(item_bytes))
            .ok_or(Error::Overflow)?;
        Ok(Self {
            arena,
            target,
            remaining: u64::try_from(bytes).map_err(|_| Error::Overflow)?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct Emission {
    boundary: DemuxBoundary,
    kind: DemuxPayloadKind,
    flags: u32,
    continue_without_header: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PayloadFormat {
    Owned,
    Dvb,
    Compact,
}

// Native payloads and their nonempty spans are bounded by DEFAULT_PACKET_CAP.
// Keep both counters in the ordinary byte-count word on 64-bit targets.
#[derive(Debug, Default)]
#[cfg_attr(target_pointer_width = "64", repr(align(8)))]
struct PayloadExtent {
    #[cfg(target_pointer_width = "64")]
    bytes: u32,
    #[cfg(target_pointer_width = "64")]
    spans: u32,
    #[cfg(not(target_pointer_width = "64"))]
    bytes: usize,
}
const _: () = assert!(size_of::<PayloadExtent>() == size_of::<usize>());
const _: () = assert!(align_of::<PayloadExtent>() == align_of::<usize>());
const _: () = assert!(size_of::<PayloadFormat>() == size_of::<bool>());

/// Opt-in, charged, one-transport-input mailbox. Caller keeps the strict source
/// operation and causal events; this observer cannot replace or suppress them.
#[derive(Debug)]
pub struct DemuxObserver {
    profile: DemuxProfile,
    scope: DemuxScope,
    // No source storage or auxiliary queue; only selects a bounded demux view.
    private_audio: Option<PrivateAudioCarriage>,
    demux_input: Option<crate::compat::input::InputBinding>,
    payload_format: PayloadFormat,
    budget: Budget,
    limits: ParseLimits,
    pending: Option<Pending>,
    ended: bool,
    last_input: Option<(u64, u64)>,
    entity_first: bool,
    media_position_base: u64,
    previous_counter: Option<u8>,
    active: bool,
    origin: Option<SourceLocation>,
    position: i64,
    raw_header: [u8; HEADER_MAX],
    header_used: usize,
    header_target: usize,
    header: Option<PesHeader>,
    declared_total: Option<usize>,
    body_limit: Option<usize>,
    time_header_applies: bool,
    corrupt: bool,
    payload_extent: PayloadExtent,
    header_ranges: BudgetVec<SourceRange>,
    spans: PesList<DemuxSpan>,
    output_header: PesList<SourceRange>,
    growth: Option<Growth>,
    emission: Option<Emission>,
    fault: Option<Error>,
}
impl DemuxObserver {
    fn payload_len(&self) -> usize {
        #[cfg(target_pointer_width = "64")]
        {
            self.payload_extent.bytes as usize
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            self.payload_extent.bytes
        }
    }
    fn span_count(&self) -> usize {
        #[cfg(target_pointer_width = "64")]
        {
            self.payload_extent.spans as usize
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            self.spans.len()
        }
    }
    fn groups_payload_runs(&self) -> bool {
        // Narrow targets keep ordinary storage rather than enlarge the
        // charged observer or rescan records to recover the logical count.
        cfg!(target_pointer_width = "64") && self.payload_format == PayloadFormat::Compact
    }
    fn set_payload_len(&mut self, bytes: usize) {
        debug_assert!(bytes <= DEFAULT_PACKET_CAP);
        #[cfg(target_pointer_width = "64")]
        {
            self.payload_extent.bytes = bytes as u32;
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            self.payload_extent.bytes = bytes;
        }
    }
    fn add_spans(&mut self, count: usize) {
        #[cfg(target_pointer_width = "64")]
        {
            self.payload_extent.spans += count as u32;
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            let _ = count;
        }
    }
    fn reset_payload_extent(&mut self) {
        self.payload_extent = PayloadExtent::default();
    }
    /// Explicit registered carriage.
    /// All selected kinds share the existing bounded charged assembly.
    /// Preflight is allocation-free; no source bytes, times or epochs are altered.
    pub fn new_registered(
        profile: DemuxProfile,
        scope: DemuxScope,
        carriage: RegisteredCarriage,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        if scope.program == 0 {
            return Err(Error::Invalid("native PES selected program"));
        }
        checked_pid(scope.pid)?;
        limits.validate()?;
        let (stream_type, stream_id) = carriage.selected_signature();
        if scope.stream_type != stream_type {
            return Err(Error::Invalid("native registered carriage kind mismatch"));
        }
        if scope.stream_id != stream_id {
            return Err(Error::Unsupported(
                "native registered PES stream ID unobserved",
            ));
        }
        Self::admitted(profile, scope, budget, limits)
    }

    pub fn new(
        profile: DemuxProfile,
        scope: DemuxScope,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        if scope.program == 0 {
            return Err(Error::Invalid("native PES selected program"));
        }
        checked_pid(scope.pid)?;
        if scope.stream_type != 0x1b || scope.stream_id != 0xe0 {
            return Err(Error::Unsupported("native PES stream context unobserved"));
        }
        limits.validate()?;
        Self::admitted(profile, scope, budget, limits)
    }
    fn admitted(
        profile: DemuxProfile,
        scope: DemuxScope,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        Ok(Self {
            profile,
            scope,
            private_audio: None,
            demux_input: None,
            payload_format: PayloadFormat::Owned,
            header_ranges: BudgetVec::new(&budget),
            spans: PesList::new(&budget),
            output_header: PesList::new(&budget),
            budget,
            limits,
            pending: None,
            ended: false,
            last_input: None,
            entity_first: false,
            media_position_base: 0,
            previous_counter: None,
            active: false,
            origin: None,
            position: 0,
            raw_header: [0; HEADER_MAX],
            header_used: 0,
            header_target: 6,
            header: None,
            declared_total: None,
            body_limit: None,
            time_header_applies: false,
            corrupt: false,
            payload_extent: PayloadExtent::default(),
            growth: None,
            emission: None,
            fault: None,
        })
    }
    /// Install once, before any carrier. Sequential input, clock and PMT
    /// epochs are separate; every bound push/poll must supply the actual cursor.
    pub fn bind_input(
        &mut self,
        context: &crate::compat::input::InputContext,
        input: &triergon_core::InputCursor,
    ) -> Result<()> {
        context.validate_cursor(input)?;
        if !self.recovery_idle() || self.last_input.is_some() || self.demux_input.is_some() {
            return Err(Error::Invalid("native startup binding after input"));
        }
        if input.source() != self.scope.entity || !context.backing_bytes().belongs_to(&self.budget)
        {
            return Err(Error::ForeignPool);
        }
        self.demux_input = Some(context.binding());
        Ok(())
    }
    #[cfg(feature = "reference-trace")]
    pub(crate) fn is_dvb_subtitles(&self) -> bool {
        self.payload_format == PayloadFormat::Dvb
    }
    fn finite_private(&self) -> bool {
        self.private_audio.is_some() || self.payload_format == PayloadFormat::Dvb
    }
    #[cold]
    fn finite_private_error(&self, audio: &'static str, dvb: &'static str) -> Error {
        Error::Unsupported(if self.payload_format == PayloadFormat::Dvb {
            dvb
        } else {
            audio
        })
    }
    pub(crate) fn recovery_idle(&self) -> bool {
        self.pending.is_none()
            && self.emission.is_none()
            && !self.active
            && !self.ended
            && self.fault.is_none()
    }
    pub(crate) fn recovery_pristine(&self) -> bool {
        self.recovery_idle() && self.last_input.is_none()
    }
    pub(crate) fn recovery_budget(&self) -> &Budget {
        &self.budget
    }
    pub(crate) fn note_recovered(&mut self, counter: u8, offset: u64, ordinal: u64) {
        self.previous_counter = Some(counter);
        self.last_input = Some((ordinal, offset));
        self.entity_first = false;
    }

    #[cfg(feature = "reference-trace")]
    pub fn profile(&self) -> DemuxProfile {
        self.profile
    }
    #[cfg(feature = "reference-trace")]
    pub fn operation(&self) -> &'static str {
        "ArchivedNativePesPacketsV1"
    }
    pub fn scope(&self) -> DemuxScope {
        self.scope
    }
    pub fn limits(&self) -> ParseLimits {
        self.limits
    }
    /// Logical EOF only. Segment/entity continuation is a distinct unproven
    /// demux context and must not be silently mapped to this operation.
    pub fn end_of_source(&mut self) -> Result<()> {
        if self.pending.is_some() || self.emission.is_some() {
            return Err(Error::Invalid("native PES input not drained"));
        }
        self.ended = true;
        Ok(())
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<DemuxPacket>> {
        self.poll_bound(work, cancel, None)
    }
    pub fn poll_with_input(
        &mut self,
        input: &triergon_core::InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<DemuxPacket>> {
        self.poll_bound(work, cancel, Some(input))
    }
    pub(crate) fn poll_bound(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        input: Option<&triergon_core::InputCursor>,
    ) -> Result<AssemblyStep<DemuxPacket>> {
        let output = self.poll_payload_bound(work, cancel, input, false)?;
        Ok(match output {
            AssemblyStep::Output(packet) => AssemblyStep::Output(packet.into_owned()?),
            AssemblyStep::NeedPayload => AssemblyStep::NeedPayload,
            AssemblyStep::Yield => AssemblyStep::Yield,
            AssemblyStep::Finished => AssemblyStep::Finished,
        })
    }

    /// Hands back the span list of a packet this observer emitted once its
    /// payload has been consumed ([`PayloadCursor::into_spans`]), so the next
    /// PES fills it without allocating. Admission, payload runs and growth
    /// work are those of a new list; only the allocation is kept. A list from
    /// another pool, or one handed back while spans are being assembled, is
    /// dropped.
    pub fn reuse_payload_spans(&mut self, spans: BudgetVec<DemuxSpan>) {
        self.spans.reuse(spans);
    }
    /// Hands back the header source ranges of a packet this observer emitted,
    /// as [`reuse_payload_spans`](Self::reuse_payload_spans) does its spans.
    pub fn reuse_header_ranges(&mut self, ranges: BudgetVec<SourceRange>) {
        self.output_header.reuse(ranges);
    }
    /// Select compact payload delivery on a fresh, inactive observer. Ordinary
    /// polling remains the default; compact delivery uses the explicit methods below.
    pub fn enable_compact_payloads(&mut self) -> Result<()> {
        if self.active
            || self.last_input.is_some()
            || self.ended
            || self.fault.is_some()
            || self.pending.is_some()
            || self.emission.is_some()
            || self.growth.is_some()
            || self.header.is_some()
            || self.span_count() != 0
        {
            return Err(Error::Invalid("compact PES payload selection after input"));
        }
        if self.finite_private() {
            return Err(Error::Unsupported("compact private PES payload"));
        }
        self.payload_format = PayloadFormat::Compact;
        Ok(())
    }

    /// Whether delivery uses the explicit compact polling methods.
    pub fn uses_compact_payloads(&self) -> bool {
        self.payload_format == PayloadFormat::Compact
    }

    /// Poll after selecting compact delivery, with the ordinary work and
    /// cancellation boundaries. A mismatched polling mode leaves state intact.
    pub fn poll_compact_payload(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<CompactDemuxPacket>> {
        self.poll_payload_bound(work, cancel, None, true)
    }
    /// Poll compact delivery while validating the observer's input binding.
    pub fn poll_compact_payload_with_input(
        &mut self,
        input: &triergon_core::InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<CompactDemuxPacket>> {
        self.poll_payload_bound(work, cancel, Some(input), true)
    }

    fn poll_payload_bound(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        input: Option<&triergon_core::InputCursor>,
        compact: bool,
    ) -> Result<AssemblyStep<CompactDemuxPacket>> {
        if compact != (self.payload_format == PayloadFormat::Compact) {
            return Err(Error::Invalid("PES payload polling representation"));
        }
        self.validate_input(input)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(error) = self.fault {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = &result {
            self.keep_fault(*error);
        }
        result
    }

    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<CompactDemuxPacket>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(AssemblyStep::Yield);
            }
            if self.emission.is_some() {
                return self.poll_emit(work, cancel);
            }
            if self.active && self.header.is_some() && self.body_limit == Some(self.payload_len()) {
                if self.pending.as_ref().is_some_and(|pending| {
                    pending.body_drained
                        || pending
                            .payload
                            .as_ref()
                            .is_some_and(|payload| pending.position == payload.len())
                }) {
                    self.pending = None;
                }
                self.schedule(DemuxBoundary::FiniteLength, false, false)?;
                continue;
            }
            if self.pending.is_none() {
                if !self.ended {
                    return Ok(AssemblyStep::NeedPayload);
                }
                if !self.active {
                    return Ok(AssemblyStep::Finished);
                }
                self.schedule(DemuxBoundary::LogicalEof, false, false)?;
                continue;
            }
            if !self
                .pending
                .as_ref()
                .ok_or(Error::Invalid("native PES input missing"))?
                .started
            {
                if !self.poll_start(work)? {
                    return Ok(AssemblyStep::Yield);
                }
                continue;
            }
            if self.header.is_none() {
                if !self.poll_header(work, cancel)? {
                    return Ok(AssemblyStep::Yield);
                }
            } else if !self.poll_body(work, cancel)? {
                return Ok(AssemblyStep::Yield);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::carrier::{PLAIN_PAYLOAD_BYTES, TS_PAYLOAD_START, TS188_BYTES};

    #[test]
    fn neighboring_payload_in_the_same_window_has_the_wrong_extent() {
        let pool = Budget::new(4096).unwrap();
        let width = crate::PacketFormat::Ts188.bytes();
        let mut data = vec![0x42; 2 * width];
        for carrier in data.chunks_exact_mut(width) {
            carrier[..4].copy_from_slice(&[0x47, 0, 7, 0x10]);
        }
        let backing = Bytes::copy_from_slice(&pool, &data).unwrap();
        let mut packet = crate::transport::parse_transport(
            backing.slice(0..width).unwrap(),
            crate::PacketFormat::Ts188,
            0,
        )
        .unwrap();
        DemuxObserver::transport_bounds(&packet).unwrap();
        packet.payload = Some(backing.slice(width + 4..2 * width).unwrap());
        assert!(
            packet
                .payload
                .as_ref()
                .unwrap()
                .shares_storage_with(&packet.source)
        );
        assert_eq!(
            DemuxObserver::transport_bounds(&packet),
            Err(Error::Invalid("native TS payload bounds"))
        );
    }

    #[test]
    fn compact_accounting_keeps_the_charged_observer_layout() {
        #[allow(
            dead_code,
            reason = "field layout fixes the ordinary observer's charged inline footprint"
        )]
        struct OwnedObserverLayout {
            profile: DemuxProfile,
            scope: DemuxScope,
            private_audio: Option<PrivateAudioCarriage>,
            demux_input: Option<crate::compat::input::InputBinding>,
            dvb_subtitles: bool,
            budget: Budget,
            limits: ParseLimits,
            pending: Option<Pending>,
            ended: bool,
            last_input: Option<(u64, u64)>,
            entity_first: bool,
            media_position_base: u64,
            previous_counter: Option<u8>,
            active: bool,
            origin: Option<SourceLocation>,
            position: i64,
            raw_header: [u8; HEADER_MAX],
            header_used: usize,
            header_target: usize,
            header: Option<PesHeader>,
            declared_total: Option<usize>,
            body_limit: Option<usize>,
            time_header_applies: bool,
            corrupt: bool,
            payload_len: usize,
            header_ranges: BudgetVec<SourceRange>,
            spans: PesList<DemuxSpan>,
            output_header: PesList<SourceRange>,
            growth: Option<Growth>,
            emission: Option<Emission>,
            fault: Option<Error>,
        }
        assert_eq!(size_of::<DemuxObserver>(), size_of::<OwnedObserverLayout>());
        assert_eq!(
            align_of::<DemuxObserver>(),
            align_of::<OwnedObserverLayout>()
        );
        assert_eq!(
            size_of::<Option<DemuxObserver>>(),
            size_of::<Option<OwnedObserverLayout>>()
        );
    }

    fn body_carrier(pool: &Budget, length: u16, offset: u64, counter: u8) -> TransportPacket {
        let [hi, lo] = length.to_be_bytes();
        let mut raw = [0xff; TS188_BYTES];
        raw[..13].copy_from_slice(&[
            0x47,
            0x41,
            0,
            0x10 | counter,
            0,
            0,
            1,
            0xe0,
            hi,
            lo,
            0x80,
            0,
            0,
        ]);
        let mut packet = crate::transport::parse_transport(
            Bytes::copy_from_slice(pool, &raw).unwrap(),
            crate::PacketFormat::Ts188,
            offset,
        )
        .unwrap();
        packet.source_id = Some(SourceId(1));
        packet
    }

    fn body_observer(pool: &Budget) -> (DemuxObserver, SourceContext) {
        let epochs = SourceEpochs {
            configuration: 0,
            clock: 0,
        };
        let observer = DemuxObserver::new(
            DemuxProfile::PesPackets,
            DemuxScope {
                program: 1,
                pid: 256,
                stream_type: 0x1b,
                stream_id: 0xe0,
                entity: SourceId(1),
                epochs,
            },
            pool.clone(),
            ParseLimits::default(),
        )
        .unwrap();
        let context = SourceContext {
            source_event_ordinal: 4,
            epochs,
            byte_run_epoch: None,
        };
        (observer, context)
    }

    fn pending_body(length: u16) -> (DemuxObserver, Bytes, SourceContext) {
        let pool = Budget::new(8192).unwrap();
        let (mut observer, context) = body_observer(&pool);
        let packet = body_carrier(&pool, length, 564, 0);
        let payload = packet.payload.as_ref().unwrap().clone();
        observer.push(packet, context).unwrap();
        let mut work = WorkBudget::new(u64::MAX);
        assert!(observer.poll_start(&mut work).unwrap());
        for _ in 0..8 {
            if observer.header.is_some() {
                break;
            }
            assert!(
                observer
                    .poll_header(&mut work, &AtomicBool::new(false))
                    .unwrap()
            );
        }
        assert!(observer.header.is_some());
        assert_eq!(observer.pending.as_ref().unwrap().position, 9);
        (observer, payload, context)
    }

    #[test]
    fn admitted_carrier_bounds_keep_ranges_and_cancellation_across_quanta() {
        for format in [crate::PacketFormat::Ts188, crate::PacketFormat::M2ts192] {
            for compact in [false, true] {
                for quantum in [1, 2, 7, 64, u64::MAX] {
                    let pool = Budget::new(8192).unwrap();
                    let (mut observer, context) = body_observer(&pool);
                    if compact {
                        observer.enable_compact_payloads().unwrap();
                    }
                    let mut packet = body_carrier(&pool, 178, 564, 0);
                    let prefix = if format == crate::PacketFormat::M2ts192 {
                        let mut raw = vec![0, 0, 0, 1];
                        raw.extend_from_slice(packet.source.as_ref());
                        packet = crate::transport::parse_transport(
                            Bytes::copy_from_slice(&pool, &raw).unwrap(),
                            format,
                            564,
                        )
                        .unwrap();
                        packet.source_id = Some(SourceId(1));
                        4
                    } else {
                        0
                    };
                    let original = packet.source.clone();
                    observer.push(packet, context).unwrap();
                    assert!(matches!(
                        observer.poll_payload_bound(
                            &mut WorkBudget::new(0),
                            &AtomicBool::new(true),
                            None,
                            compact,
                        ),
                        Err(Error::Cancelled)
                    ));
                    assert!(
                        observer
                            .pending
                            .as_ref()
                            .unwrap()
                            .source
                            .shares_storage_with(&original)
                    );
                    assert!(matches!(
                        observer
                            .poll_payload_bound(
                                &mut WorkBudget::new(0),
                                &AtomicBool::new(false),
                                None,
                                compact,
                            )
                            .unwrap(),
                        AssemblyStep::Yield
                    ));
                    let mut output = None;
                    for _ in 0..4096 {
                        match observer
                            .poll_payload_bound(
                                &mut WorkBudget::new(quantum),
                                &AtomicBool::new(false),
                                None,
                                compact,
                            )
                            .unwrap()
                        {
                            AssemblyStep::Output(packet) => {
                                output = Some(packet);
                                break;
                            }
                            AssemblyStep::Yield => {}
                            other => panic!("unexpected finite PES step: {other:?}"),
                        }
                    }
                    let packet = output.expect("finite PES completes with every positive quantum");
                    assert_eq!(packet.position, 564 + prefix as i64);
                    assert_eq!(packet.boundary, DemuxBoundary::FiniteLength);
                    assert_eq!(packet.payload_len, 175);
                    let spans = packet.payload.spans().collect::<Vec<_>>();
                    assert_eq!(spans.len(), 1);
                    assert_eq!(
                        spans[0].source,
                        SourceRange {
                            entity: SourceId(1),
                            offset: 577 + prefix as u64,
                            bytes: 175
                        }
                    );
                    assert_eq!(
                        spans[0].transport,
                        SourceRange {
                            entity: SourceId(1),
                            offset: 564,
                            bytes: TS188_BYTES + prefix
                        }
                    );
                    assert_eq!(spans[0].context, context);
                    assert_eq!(spans[0].as_ref(), &original.as_ref()[prefix + 13..]);
                }
            }
        }
    }

    #[test]
    fn invalid_carrier_bounds_keep_admission_precedence_and_original_input() {
        for fault in 0..9 {
            for quantum in [0, 1, 7] {
                let pool = Budget::new(8192).unwrap();
                let (mut observer, mut context) = body_observer(&pool);
                let mut packet = body_carrier(&pool, 178, 564, 0);
                let expected = match fault {
                    0 => {
                        packet.source_offset = u64::MAX;
                        packet.prefix = Some(packet.source.slice(0..1).unwrap());
                        Error::Overflow
                    }
                    1 => {
                        packet.prefix = Some(packet.source.slice(0..1).unwrap());
                        Error::Invalid("native TS packet layout")
                    }
                    2 => {
                        packet.payload_source_offset = 567;
                        Error::Invalid("native TS payload bounds")
                    }
                    3 => {
                        packet.payload_source_offset = u64::MAX;
                        Error::Overflow
                    }
                    4 => {
                        packet.payload = Some(Bytes::copy_from_slice(&pool, &[0; 184]).unwrap());
                        Error::Invalid("native TS payload bounds")
                    }
                    5 => {
                        context.epochs.clock = 1;
                        Error::Unsupported("native PES source context transition unobserved")
                    }
                    6 => {
                        packet.source_id = Some(SourceId(2));
                        Error::Unsupported("native PES source context transition unobserved")
                    }
                    7 => {
                        observer.last_input = Some((context.source_event_ordinal, 376));
                        Error::Invalid("native PES source event order")
                    }
                    _ => {
                        let foreign = Budget::new(8192).unwrap();
                        packet = body_carrier(&foreign, 178, 564, 0);
                        packet.prefix = Some(packet.source.slice(0..1).unwrap());
                        Error::Invalid("foreign native PES byte pool")
                    }
                };
                let pointer = packet.source.as_ref().as_ptr();
                let payload_pointer = packet.payload.as_ref().unwrap().as_ref().as_ptr();
                let used = pool.used();
                let previous = observer.last_input;
                let rejected = observer.push(packet, context).unwrap_err();
                assert_eq!(rejected.error, expected);
                assert_eq!(rejected.value.source.as_ref().as_ptr(), pointer);
                assert_eq!(
                    rejected.value.payload.as_ref().unwrap().as_ref().as_ptr(),
                    payload_pointer
                );
                assert_eq!(pool.used(), used);
                assert!(observer.pending.is_none());
                assert_eq!(observer.last_input, previous);
                let mut work = WorkBudget::new(quantum);
                let step = observer.poll(&mut work, &AtomicBool::new(false)).unwrap();
                assert!(if quantum == 0 {
                    matches!(step, AssemblyStep::Yield)
                } else {
                    matches!(step, AssemblyStep::NeedPayload)
                });
                assert_eq!(work.spent(), 0);
            }
        }
    }

    #[test]
    fn independent_transport_bounds_keep_layout_and_payloadless_error_order() {
        let pool = Budget::new(8192).unwrap();
        let (mut observer, context) = body_observer(&pool);
        let mut packet = body_carrier(&pool, 178, 564, 0);
        packet.source_offset = u64::MAX;
        packet.prefix = Some(packet.source.slice(0..1).unwrap());
        assert_eq!(
            DemuxObserver::transport_bounds(&packet),
            Err(Error::Invalid("native TS packet layout"))
        );
        packet.prefix = None;
        assert_eq!(
            DemuxObserver::transport_bounds(&packet),
            Err(Error::Overflow)
        );
        packet.payload = None;
        assert_eq!(DemuxObserver::transport_bounds(&packet), Ok(()));
        assert_eq!(
            observer.push(packet, context).unwrap_err().error,
            Error::Overflow
        );
    }

    fn assert_body_not_moved(observer: &DemuxObserver, expected: &Bytes) {
        let pending = observer.pending.as_ref().unwrap();
        let payload = pending.payload.as_ref().unwrap();
        assert!(payload.shares_storage_with(expected));
        assert_eq!(payload.as_ref().as_ptr(), expected.as_ref().as_ptr());
        assert_eq!(payload.as_ref(), expected.as_ref());
        assert_eq!(pending.position, 9);
        assert!(!pending.body_drained);
        assert_eq!(observer.payload_len(), 0);
        assert!(observer.spans.is_empty());
    }

    #[test]
    fn completed_body_moves_its_view_but_keeps_pending_until_the_same_poll_boundary() {
        let (mut observer, payload, context) = pending_body(0);
        let cancel = AtomicBool::new(false);
        assert!(
            observer
                .reserve(Arena::Payload, &mut WorkBudget::new(u64::MAX), &cancel)
                .unwrap()
        );
        assert!(
            !observer
                .poll_body(&mut WorkBudget::new(0), &cancel)
                .unwrap()
        );
        assert_body_not_moved(&observer, &payload);
        let mut work = WorkBudget::new(1);
        assert!(observer.poll_body(&mut work, &cancel).unwrap());
        assert_eq!(work.spent(), 1);
        let pending = observer.pending.as_ref().unwrap();
        assert!(pending.body_drained);
        assert!(pending.payload.is_none());
        assert_eq!(pending.position, payload.len());
        assert_eq!(
            &pending.source.as_ref()[TS_PAYLOAD_START..],
            payload.as_ref()
        );
        let [span] = observer.spans.as_slice() else {
            panic!("expected one admitted span")
        };
        assert!(span.backing_bytes().shares_storage_with(&payload));
        assert_eq!(span.as_ref().as_ptr(), payload.as_ref()[9..].as_ptr());
        assert_eq!(span.as_ref(), &payload.as_ref()[9..]);
        assert_eq!(span.context, context);
        assert_eq!(
            span.source,
            SourceRange {
                entity: SourceId(1),
                offset: 577,
                bytes: 175
            }
        );
        assert_eq!(
            span.transport,
            SourceRange {
                entity: SourceId(1),
                offset: 564,
                bytes: TS188_BYTES
            }
        );
        assert!(matches!(
            observer.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            AssemblyStep::Yield
        ));
        let next = body_carrier(&observer.budget, 0, 752, 1);
        let next_context = SourceContext {
            source_event_ordinal: 6,
            ..context
        };
        let rejected = observer.push(next, next_context).unwrap_err();
        assert_eq!(
            rejected.error,
            Error::Invalid("native PES input not drained")
        );
        assert!(matches!(
            observer.poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert!(observer.pending.as_ref().unwrap().body_drained);
        let mut work = WorkBudget::new(1);
        assert!(matches!(
            observer.poll(&mut work, &cancel).unwrap(),
            AssemblyStep::NeedPayload
        ));
        assert_eq!(work.spent(), 0);
        observer.push(rejected.value, next_context).unwrap();
    }

    #[test]
    fn body_transfer_waits_for_arena_capacity_and_keeps_prepaid_retry_work() {
        let (mut observer, payload, _) = pending_body(0);
        let cancel = AtomicBool::new(false);
        assert!(
            !observer
                .poll_body(&mut WorkBudget::new(1), &cancel)
                .unwrap()
        );
        assert_body_not_moved(&observer, &payload);
        let pressure = observer
            .budget
            .reserve(observer.budget.available())
            .unwrap();
        assert!(matches!(
            observer.poll_body(&mut WorkBudget::new(u64::MAX), &cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_body_not_moved(&observer, &payload);
        assert_eq!(observer.growth.as_ref().unwrap().remaining, 0);
        drop(pressure);
        let mut work = WorkBudget::new(1);
        assert!(observer.poll_body(&mut work, &cancel).unwrap());
        assert_eq!(work.spent(), 1);
        assert!(observer.pending.as_ref().unwrap().body_drained);
        assert!(
            observer.spans.as_slice()[0]
                .backing_bytes()
                .shares_storage_with(&payload)
        );
    }

    #[test]
    fn body_ranges_and_cancellation_are_checked_before_transfer_in_original_order() {
        for fault in 0..4 {
            let (mut observer, payload, _) = pending_body(0);
            assert!(
                observer
                    .reserve(
                        Arena::Payload,
                        &mut WorkBudget::new(u64::MAX),
                        &AtomicBool::new(false)
                    )
                    .unwrap()
            );
            let packet = &mut observer.pending.as_mut().unwrap().carrier;
            match fault {
                0 => {}
                1 => packet.payload_source_offset = u64::MAX - 8,
                2 => packet.payload_source_offset = u64::MAX - 180,
                _ => packet.source_offset = u64::MAX - 100,
            }
            let mut work = WorkBudget::new(1);
            assert_eq!(
                observer.poll_body(&mut work, &AtomicBool::new(true)),
                Err(if fault == 0 {
                    Error::Cancelled
                } else {
                    Error::Overflow
                })
            );
            assert_eq!(work.spent(), 1);
            assert_body_not_moved(&observer, &payload);
        }
    }

    #[test]
    fn finite_completion_keeps_full_extents_and_overlong_start_trimming() {
        for (length, count, moved) in [(178, 175, true), (8, 5, false)] {
            let (mut observer, payload, context) = pending_body(length);
            let cancel = AtomicBool::new(false);
            assert!(
                observer
                    .reserve(Arena::Payload, &mut WorkBudget::new(u64::MAX), &cancel)
                    .unwrap()
            );
            let mut work = WorkBudget::new(1);
            assert!(observer.poll_body(&mut work, &cancel).unwrap());
            assert_eq!(work.spent(), 1);
            let pending = observer.pending.as_ref().unwrap();
            assert_eq!(pending.body_drained, moved);
            assert_eq!(pending.payload.is_none(), moved);
            assert_eq!(pending.position, payload.len());
            assert_eq!(
                &pending.source.as_ref()[TS_PAYLOAD_START..],
                payload.as_ref()
            );
            assert!(matches!(
                observer.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
                AssemblyStep::Yield
            ));
            let AssemblyStep::Output(packet) = observer
                .poll(&mut WorkBudget::new(u64::MAX), &cancel)
                .unwrap()
            else {
                panic!("expected the finite PES");
            };
            assert_eq!(packet.boundary, DemuxBoundary::FiniteLength);
            assert_eq!(packet.payload_len, count);
            assert_eq!(packet.source_header.as_bytes(), &payload.as_ref()[..9]);
            let [span] = packet.spans.as_slice() else {
                panic!("expected one finite body span")
            };
            assert_eq!(span.as_ref(), &payload.as_ref()[9..9 + count]);
            assert!(span.backing_bytes().shares_storage_with(&payload));
            assert_eq!(
                span.source,
                SourceRange {
                    entity: SourceId(1),
                    offset: 577,
                    bytes: count
                }
            );
            assert_eq!(
                span.transport,
                SourceRange {
                    entity: SourceId(1),
                    offset: 564,
                    bytes: TS188_BYTES
                }
            );
            assert_eq!(span.context, context);
            assert!(observer.pending.is_none());
        }
    }

    #[test]
    fn finite_body_reserves_only_the_received_span() {
        let pool = Budget::new(8192).unwrap();
        let entity = SourceId(1);
        let epochs = SourceEpochs {
            configuration: 0,
            clock: 0,
        };
        let mut observer = DemuxObserver::new(
            DemuxProfile::PesPackets,
            DemuxScope {
                program: 1,
                pid: 256,
                stream_type: 0x1b,
                stream_id: 0xe0,
                entity,
                epochs,
            },
            pool.clone(),
            ParseLimits::default(),
        )
        .unwrap();
        let length = (3 + 5 * PLAIN_PAYLOAD_BYTES) as u16;
        let [hi, lo] = length.to_be_bytes();
        let mut raw = [0xff; TS188_BYTES];
        raw[..13].copy_from_slice(&[0x47, 0x41, 0, 0x10, 0, 0, 1, 0xe0, hi, lo, 0x80, 0, 0]);
        let mut carrier = crate::transport::parse_transport(
            Bytes::copy_from_slice(&pool, &raw).unwrap(),
            crate::PacketFormat::Ts188,
            0,
        )
        .unwrap();
        carrier.source_id = Some(entity);
        observer
            .push(
                carrier,
                SourceContext {
                    source_event_ordinal: 0,
                    epochs,
                    byte_run_epoch: None,
                },
            )
            .unwrap();
        assert!(matches!(
            observer
                .poll(&mut WorkBudget::new(u64::MAX), &AtomicBool::new(false))
                .unwrap(),
            AssemblyStep::NeedPayload
        ));
        assert_eq!(observer.spans.len(), 1);
        // The first append uses the ordinary arena size. Other tracks can
        // still use the pool while this one waits for its remaining carriers.
        assert_eq!(observer.spans.capacity(), 4);
    }
}
