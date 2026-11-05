//! Explicit archived-native packet operation, separate from strict source PES.
//!
//! Independently authored from H.222.0 source syntax and archived public-API
//! observations, after parent-observed runtime red. Assembly consumes real input
//! boundaries; no complete source/native output is split, repeated or repaired.
use crate::clock::NativeClockProfile;
use crate::pes::{HEADER_MAX, parse_header};
use crate::{
    AssemblyStep, ParseLimits, PesHeader, SourceEpochs, SourceRange, TransportPacket, checked_pid,
    commit_range, limit, prepare_range,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, PushError, Result, SourceId, SourceLocation, WorkBudget,
};

// Public option data in the exact pin's doc/demuxers.texi, not a parser
// implementation. This operation leaves cap-boundary/split contexts unsupported
// rather than changing the reference buffer or guessing their packet shape.
const DEFAULT_NATIVE_PACKET_CAP: usize = 204_800;

mod audio;
mod dvb;
pub use audio::{NativePrivateAudioCarriage, NativePrivateAudioDescriptors};
pub use dvb::NativeDvbSubtitlesDescriptors;

/// Explicit single-program/single-PID profile from validated source signalling.
/// Legacy pushes reject entity transitions; the explicit media transaction and
/// selected lifecycle APIs admit only their checked boundary contracts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativePesScope {
    pub program: u16,
    pub pid: u16,
    pub stream_type: u8,
    pub stream_id: u8,
    pub entity: SourceId,
    pub epochs: SourceEpochs,
}

/// Explicit registered transport carriage, separate from the legacy constructor's
/// H264-only contract. These selected stream IDs are the observed narrow profile;
/// this enum does not parse or identify codec access units/configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeRegisteredCarriage {
    Avc,
    Hevc,
    AacAdts,
    AacLatm,
}
impl NativeRegisteredCarriage {
    fn selected_signature(self) -> (u8, u8) {
        match self {
            Self::Avc => (0x1b, 0xe0),
            Self::Hevc => (0x24, 0xe0),
            Self::AacAdts => (0x0f, 0xc0),
            Self::AacLatm => (0x11, 0xc0),
        }
    }
}

/// Caller links the original strict-source event. None is an unknown source byte
/// run, never a native-invented epoch. Event ordinals are not core I/O tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSourceContext {
    pub source_event_ordinal: u64,
    pub epochs: SourceEpochs,
    pub byte_run_epoch: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePesBoundary {
    FiniteLength,
    NextPesStart,
    LogicalEof,
    FiniteOverrun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativePesPayloadKind {
    DeclaredComplete,
    DeclaredIncomplete,
    Unbounded,
    HeaderlessContinuation,
}

/// A span of real input, with its original transport packet and source-event
/// context. Backing bytes and vector capacity remain shared/charged core storage.
#[derive(Debug)]
pub struct NativePesSpan {
    pub(crate) bytes: Bytes,
    pub source: SourceRange,
    pub transport: SourceRange,
    pub context: NativeSourceContext,
}
impl AsRef<[u8]> for NativePesSpan {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}
impl NativePesSpan {
    /// Shared immutable owner for retained-capacity accounting/deduplication.
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Transfer the charged payload to another reader.
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

/// This may be a corrupt/partial/headerless native packet, NEVER a complete
/// strict-source `Pes` or a codec access unit. Native position can name the last
/// PES-start sync even when the physical payload starts in a later packet.
#[derive(Debug)]
pub struct NativePesPacket {
    pub profile: NativeClockProfile,
    pub program: u16,
    pub pid: u16,
    pub source_origin: SourceLocation,
    pub source_epochs: SourceEpochs,
    pub native_position: i64,
    pub native_flags: u32,
    /// Native raw90-kHz duration; zero is its unknown sentinel, not source time.
    pub native_duration90_raw: i64,
    /// Explicit MPEGTS stream-ID side-data byte, retained on continuations.
    pub native_stream_id: u8,
    pub boundary: NativePesBoundary,
    pub payload_kind: NativePesPayloadKind,
    /// Original valid causal PES header, including raw33/None. It is retained
    /// even for a continuation whose native output has no applicable time header.
    pub source_header: PesHeader,
    pub header_source_ranges: BudgetVec<SourceRange>,
    pub time_header_applies: bool,
    pub payload_len: usize,
    pub spans: BudgetVec<NativePesSpan>,
}
impl NativePesPacket {
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
    }
    pub fn time_header(&self) -> Option<&PesHeader> {
        self.time_header_applies.then_some(&self.source_header)
    }
}

#[derive(Debug)]
struct Pending {
    packet: TransportPacket,
    context: NativeSourceContext,
    position: usize,
    counter_checked: bool,
    started: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arena {
    Header,
    Payload,
    OutputHeader,
}

#[derive(Debug)]
struct Growth {
    arena: Arena,
    target: usize,
    remaining: u64,
}

#[derive(Clone, Copy, Debug)]
struct Emission {
    boundary: NativePesBoundary,
    kind: NativePesPayloadKind,
    flags: u32,
    continue_without_header: bool,
}

/// Opt-in, charged, one-transport-input mailbox. Caller keeps the strict source
/// operation and causal events; this observer cannot replace or suppress them.
#[derive(Debug)]
pub struct NativePesObserver {
    profile: NativeClockProfile,
    scope: NativePesScope,
    // No source owner or auxiliary queue; only selects a bounded native view.
    private_audio: Option<NativePrivateAudioCarriage>,
    native_input: Option<crate::native_input::NativeInputBinding>,
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
    native_position: i64,
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
    spans: BudgetVec<NativePesSpan>,
    output_header: BudgetVec<SourceRange>,
    growth: Option<Growth>,
    emission: Option<Emission>,
    fault: Option<Error>,
}
impl NativePesObserver {
    /// Explicit registered carriage, implemented after parent-observed runtime RED.
    /// All selected kinds share the existing bounded charged assembly.
    /// Preflight is allocation-free; no source bytes, times or epochs are altered.
    pub fn new_registered(
        profile: NativeClockProfile,
        scope: NativePesScope,
        carriage: NativeRegisteredCarriage,
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
        profile: NativeClockProfile,
        scope: NativePesScope,
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
        profile: NativeClockProfile,
        scope: NativePesScope,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        Ok(Self {
            profile,
            scope,
            private_audio: None,
            native_input: None,
            dvb_subtitles: false,
            header_ranges: BudgetVec::new(&budget),
            spans: BudgetVec::new(&budget),
            output_header: BudgetVec::new(&budget),
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
            native_position: 0,
            raw_header: [0; HEADER_MAX],
            header_used: 0,
            header_target: 6,
            header: None,
            declared_total: None,
            body_limit: None,
            time_header_applies: false,
            corrupt: false,
            payload_len: 0,
            growth: None,
            emission: None,
            fault: None,
        })
    }
    /// Install once, before any carrier. Sequential input, clock and PMT
    /// epochs are separate; every bound push/poll must supply the actual cursor.
    pub fn bind_native_input_v1(
        &mut self,
        context: &crate::native_input::NativeInputContext,
        input: &triergon_core::InputCursor,
    ) -> Result<()> {
        context.validate_cursor(input)?;
        if !self.recovery_idle() || self.last_input.is_some() || self.native_input.is_some() {
            return Err(Error::Invalid("native startup binding after input"));
        }
        if input.source() != self.scope.entity || !context.backing_bytes().belongs_to(&self.budget)
        {
            return Err(Error::ForeignPool);
        }
        self.native_input = Some(context.binding());
        Ok(())
    }
    pub(crate) fn validate_native_input(
        &self,
        input: Option<&triergon_core::InputCursor>,
    ) -> Result<()> {
        match (&self.native_input, input) {
            (Some(context), Some(input)) => context.validate_cursor(input),
            (Some(_), None) => Err(Error::Invalid(
                "bound native operation requires input cursor",
            )),
            (None, _) => Ok(()),
        }
    }
    #[allow(
        clippy::result_large_err,
        reason = "recover the original charged inline carrier"
    )]
    pub fn push_with_input(
        &mut self,
        packet: TransportPacket,
        context: NativeSourceContext,
        input: &triergon_core::InputCursor,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        self.push_input_boundary(packet, context, false, input)
    }
    #[allow(
        clippy::result_large_err,
        reason = "recover the original charged inline carrier"
    )]
    pub(crate) fn push_input_boundary(
        &mut self,
        packet: TransportPacket,
        context: NativeSourceContext,
        boundary: bool,
        input: &triergon_core::InputCursor,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        self.push_boundary(packet, context, boundary, Some(input))
    }
    pub(crate) fn is_dvb_subtitles(&self) -> bool {
        self.dvb_subtitles
    }
    fn finite_private(&self) -> bool {
        self.private_audio.is_some() || self.dvb_subtitles
    }
    fn finite_private_error(&self, audio: &'static str, dvb: &'static str) -> Error {
        Error::Unsupported(if self.dvb_subtitles { dvb } else { audio })
    }
    pub(crate) fn recovery_idle(&self) -> bool {
        self.pending.is_none()
            && self.emission.is_none()
            && !self.active
            && !self.ended
            && self.fault.is_none()
    }
    pub(crate) fn note_recovered(&mut self, counter: u8, offset: u64, ordinal: u64) {
        self.previous_counter = Some(counter);
        self.last_input = Some((ordinal, offset));
        self.entity_first = false;
    }

    pub fn profile(&self) -> NativeClockProfile {
        self.profile
    }
    pub fn operation(&self) -> &'static str {
        "ArchivedNativePesPacketsV1"
    }
    pub fn scope(&self) -> NativePesScope {
        self.scope
    }
    /// Retain the same logical TS/PES/native-clock operation across a confirmed
    /// entity End. Only physical entity/order and input-binding facts change.
    /// A false result is a work yield; every rejection leaves assembly intact.
    pub fn continue_media_entity_v1(
        &mut self,
        end: &crate::MediaEntityEndV1,
        input: &mut triergon_core::InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if work.remaining() == 0 {
            return Ok(false);
        }
        end.validate_continuation(input)?;
        if !self.budget.same_pool(&end.budget) {
            return Err(Error::ForeignPool);
        }
        if end.location.source != self.scope.entity
            || end.epochs != self.scope.epochs
            || end.program != self.scope.program
        {
            return Err(Error::Invalid("native media entity source scope mismatch"));
        }
        if self.pending.is_some()
            || self.emission.is_some()
            || self.growth.is_some()
            || self.fault.is_some()
            || self.ended
        {
            return Err(Error::Invalid("native media entity operation not drained"));
        }
        let position = self
            .media_position_base
            .checked_add(end.location.offset)
            .ok_or(Error::Overflow)?;
        i64::try_from(position).map_err(|_| Error::Overflow)?;
        let binding = self
            .native_input
            .as_ref()
            .map(|b| b.continued_media_entity_v1(end, input))
            .transpose()?;
        // An empty core read checks sticky transport/cancellation failure without
        // consuming an already supplied new-entity window.
        input.read_into(&mut [], work, cancel)?;
        if !work.try_charge(1) {
            return Ok(false);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.scope.entity = input.source();
        self.native_input = binding;
        self.entity_first = true;
        self.media_position_base = position;
        Ok(true)
    }
    pub(crate) fn media_native_position_v1(&self, offset: u64) -> Result<i64> {
        i64::try_from(
            self.media_position_base
                .checked_add(offset)
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)
    }
    pub fn limits(&self) -> ParseLimits {
        self.limits
    }
    /// Only the selected PID, in source order. Rejected input is recovered
    /// intact without a fresh allocation. Drain poll before another push.
    #[allow(
        clippy::result_large_err,
        reason = "recover original inline charged transport input; no uncharged Box allocation"
    )]
    pub fn push(
        &mut self,
        packet: TransportPacket,
        context: NativeSourceContext,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        self.push_boundary(packet, context, false, None)
    }
    #[allow(
        clippy::result_large_err,
        reason = "return original charged transport input inline"
    )]
    pub(crate) fn push_clock_boundary_v1(
        &mut self,
        packet: TransportPacket,
        context: NativeSourceContext,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        self.push_boundary(packet, context, true, None)
    }
    pub(crate) fn commit_clock_epoch_v1(&mut self, epochs: SourceEpochs) {
        self.scope.epochs = epochs;
    }
    /// A drained codec-map commit. Returned old packets keep their original
    /// headers/owners. Retire only the idle internal header alias; preserve wire
    /// counter/order and charged arena capacity, without copying a payload.
    pub(crate) fn commit_pmt_scope_v1(&mut self, scope: NativePesScope) {
        self.scope = scope;
        self.header = None;
        self.origin = None;
    }
    pub(crate) fn recovery_counter(&self) -> Option<u8> {
        self.previous_counter
    }
    #[allow(
        clippy::result_large_err,
        reason = "return original charged transport input inline"
    )]
    fn push_boundary(
        &mut self,
        packet: TransportPacket,
        context: NativeSourceContext,
        selected_boundary: bool,
        input: Option<&triergon_core::InputCursor>,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        if let Some(error) = self.input_error(&packet, context, selected_boundary, input) {
            return Err(PushError {
                error,
                value: packet,
            });
        }
        self.last_input = Some((context.source_event_ordinal, packet.source_offset));
        self.entity_first = false;
        self.pending = Some(Pending {
            packet,
            context,
            position: 0,
            counter_checked: false,
            started: false,
        });
        Ok(())
    }

    fn input_error(
        &self,
        packet: &TransportPacket,
        context: NativeSourceContext,
        selected_boundary: bool,
        input: Option<&triergon_core::InputCursor>,
    ) -> Option<Error> {
        if let Err(error) = self.validate_native_input(input) {
            Some(error)
        } else if !packet.source.belongs_to(&self.budget) {
            Some(Error::Invalid("foreign native PES byte pool"))
        } else if self.pending.is_some() || self.emission.is_some() {
            Some(Error::Invalid("native PES input not drained"))
        } else if self.ended {
            Some(Error::Invalid("native PES input after EOF"))
        } else if let Some(error) = self.fault {
            Some(error)
        } else if packet.pid != self.scope.pid {
            Some(Error::Invalid("native PES input PID"))
        } else if packet.continuity_counter > 15 {
            Some(Error::Invalid("native TS counter range"))
        } else if packet.source_id != Some(self.scope.entity) || context.epochs != self.scope.epochs
        {
            Some(Error::Unsupported(
                "native PES source context transition unobserved",
            ))
        } else if packet.transport_error || packet.scrambling != 0 {
            Some(Error::Unsupported(
                "native PES damaged/opaque transport unobserved",
            ))
        } else if self.finite_private() && packet.prefix.is_some() {
            Some(self.finite_private_error(
                "native private audio M2TS context unobserved",
                "native DVB M2TS context unobserved",
            ))
        } else if self.finite_private()
            && (context.byte_run_epoch.is_some() || packet.discontinuity_indicator())
        {
            Some(self.finite_private_error(
                "native private audio damaged/clock context unobserved",
                "native DVB damaged/clock context unobserved",
            ))
        } else if !selected_boundary
            && packet.discontinuity_indicator()
            && packet
                .adaptation
                .as_ref()
                .is_some_and(|field| field.pcr.is_some())
        {
            Some(Error::Unsupported(
                "native PCR clock boundary context unobserved",
            ))
        } else if self.last_input.is_some_and(|(event, offset)| {
            context.source_event_ordinal <= event
                || (!self.entity_first && packet.source_offset <= offset)
        }) {
            Some(Error::Invalid("native PES source event order"))
        } else if packet
            .source_offset
            .checked_add(packet.source.len() as u64)
            .is_none()
        {
            Some(Error::Overflow)
        } else {
            Self::transport_bounds(packet).err()
        }
    }

    /// Append an ordinary TS188 continuation when it fits the existing arena.
    /// False leaves the carrier, observer and budget ready for normal polling.
    pub fn append_ready_continuation(
        &mut self,
        packet: &mut TransportPacket,
        context: NativeSourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let Some(payload) = packet.payload.as_ref() else {
            return Ok(false);
        };
        let count = payload.len();
        let next = self.payload_len.checked_add(count).ok_or(Error::Overflow)?;
        if packet.payload_unit_start
            || packet.discontinuity_indicator()
            || packet.prefix.is_some()
            || count == 0
            || !self.active
            || self.header.is_none()
            || self.spans.len() >= self.spans.capacity().min(self.limits.source_spans)
            || self.body_limit.is_some_and(|limit| next >= limit)
            || next >= DEFAULT_NATIVE_PACKET_CAP
            || self.header_used + next > self.limits.pes_bytes
            || self.previous_counter.map(|value| (value + 1) & 15)
                != Some(packet.continuity_counter)
            || self.input_error(packet, context, false, None).is_some()
            || cancel.load(Ordering::Relaxed)
            || !work.try_charge(2)
        {
            return Ok(false);
        }
        let bytes = packet
            .payload
            .take()
            .ok_or(Error::Invalid("native PES payload missing"))?;
        self.spans
            .try_push(NativePesSpan {
                bytes,
                source: SourceRange {
                    entity: self.scope.entity,
                    offset: packet.payload_source_offset,
                    bytes: count,
                },
                transport: SourceRange {
                    entity: self.scope.entity,
                    offset: packet.source_offset,
                    bytes: packet.source.len(),
                },
                context,
            })
            .map_err(|error| error.error)?;
        self.last_input = Some((context.source_event_ordinal, packet.source_offset));
        self.entity_first = false;
        self.previous_counter = Some(packet.continuity_counter);
        self.payload_len = next;
        Ok(true)
    }
    pub(crate) fn transport_bounds(packet: &TransportPacket) -> Result<()> {
        let prefix = packet.prefix.as_ref().map_or(0, |bytes| bytes.len());
        if (prefix != 0 && prefix != 4) || packet.source.len() != 188 + prefix {
            return Err(Error::Invalid("native TS packet layout"));
        }
        if let Some(payload) = packet.payload.as_ref() {
            let start = packet
                .source_offset
                .checked_add(prefix as u64 + 4)
                .ok_or(Error::Overflow)?;
            let end = packet
                .source_offset
                .checked_add(packet.source.len() as u64)
                .ok_or(Error::Overflow)?;
            let payload_end = packet
                .payload_source_offset
                .checked_add(payload.len() as u64)
                .ok_or(Error::Overflow)?;
            if payload.len() > 184
                || !payload.shares_storage_with(&packet.source)
                || packet.payload_source_offset < start
                || payload_end > end
            {
                return Err(Error::Invalid("native TS payload bounds"));
            }
        }
        Ok(())
    }
    /// Logical EOF only. Segment/entity continuation is a distinct unproven
    /// native context and must not be silently mapped to this operation.
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
    ) -> Result<AssemblyStep<NativePesPacket>> {
        self.poll_bound(work, cancel, None)
    }
    pub fn poll_with_input(
        &mut self,
        input: &triergon_core::InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<NativePesPacket>> {
        self.poll_bound(work, cancel, Some(input))
    }
    pub(crate) fn poll_bound(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        input: Option<&triergon_core::InputCursor>,
    ) -> Result<AssemblyStep<NativePesPacket>> {
        self.validate_native_input(input)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(error) = self.fault {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result.as_ref() {
            if !matches!(
                error,
                Error::Backpressure { .. } | Error::Allocation | Error::Cancelled
            ) {
                self.fault = Some(*error);
            }
        }
        result
    }

    /// Prepay allocation/move work across small polls, then reserve before any
    /// logical byte/extent is committed. A pressure retry retains paid work and
    /// the original mailbox, not a partially published result.
    fn reserve(
        &mut self,
        arena: Arena,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let (len, capacity, item_bytes) = match arena {
            Arena::Header => (
                self.header_ranges.len(),
                self.header_ranges.capacity(),
                size_of::<SourceRange>(),
            ),
            Arena::Payload => (
                self.spans.len(),
                self.spans.capacity(),
                size_of::<NativePesSpan>(),
            ),
            Arena::OutputHeader => (
                self.output_header.len(),
                self.output_header.capacity(),
                size_of::<SourceRange>(),
            ),
        };
        let maximum = if arena == Arena::Payload {
            self.limits.source_spans
        } else {
            self.limits.source_spans.min(HEADER_MAX)
        };
        if len >= maximum {
            return Err(limit(len.saturating_add(1), maximum));
        }
        if capacity > len {
            return Ok(true);
        }
        if self.growth.is_none() {
            let needed = len.checked_add(1).ok_or(Error::Overflow)?;
            let target = capacity.saturating_mul(2).max(4).min(maximum).max(needed);
            let bytes = target
                .checked_add(len)
                .and_then(|count| count.checked_mul(item_bytes))
                .ok_or(Error::Overflow)?;
            self.growth = Some(Growth {
                arena,
                target,
                remaining: u64::try_from(bytes).map_err(|_| Error::Overflow)?,
            });
        }
        let growth = self
            .growth
            .as_mut()
            .ok_or(Error::Invalid("native PES growth missing"))?;
        if growth.arena != arena {
            return Err(Error::Invalid("native PES growth phase"));
        }
        let paid = work.take(growth.remaining);
        growth.remaining -= paid;
        if growth.remaining != 0 {
            return Ok(false);
        }
        let target = growth.target;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        match arena {
            Arena::Header => self.header_ranges.try_reserve_exact(target - len)?,
            Arena::Payload => self.spans.try_reserve_exact(target - len)?,
            Arena::OutputHeader => self.output_header.try_reserve_exact(target - len)?,
        }
        self.growth = None;
        Ok(true)
    }

    fn begin_pes(&mut self) -> Result<()> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let local_position = if let Some(context) = &self.native_input {
            context.position(&pending.packet)?
        } else {
            let prefix = pending
                .packet
                .prefix
                .as_ref()
                .map_or(0, |bytes| bytes.len());
            i64::try_from(
                pending
                    .packet
                    .source_offset
                    .checked_add(prefix as u64)
                    .ok_or(Error::Overflow)?,
            )
            .map_err(|_| Error::Overflow)?
        };
        self.native_position = self.media_native_position_v1(
            u64::try_from(local_position).map_err(|_| Error::Overflow)?,
        )?;
        self.origin = Some(SourceLocation {
            source: self.scope.entity,
            offset: pending.packet.source_offset,
        });
        self.active = true;
        self.raw_header = [0; HEADER_MAX];
        self.header_used = 0;
        self.header_target = 6;
        self.header = None;
        self.declared_total = None;
        self.body_limit = None;
        self.time_header_applies = true;
        self.corrupt = false;
        self.payload_len = 0;
        self.header_ranges.clear();
        self.spans.clear();
        self.output_header.clear();
        Ok(())
    }

    fn schedule(
        &mut self,
        boundary: NativePesBoundary,
        continuation: bool,
        forced_corrupt: bool,
    ) -> Result<()> {
        let header = self.header.as_ref().ok_or(Error::Unsupported(
            "native interrupted PES header unobserved",
        ))?;
        if self.finite_private()
            && (boundary != NativePesBoundary::FiniteLength
                || continuation
                || forced_corrupt
                || self.corrupt
                || self.body_limit != Some(self.payload_len))
        {
            return Err(self.finite_private_error(
                "native private audio incomplete/recovery boundary unobserved",
                "native DVB incomplete/recovery boundary unobserved",
            ));
        }
        if self.payload_len == 0 {
            return Err(Error::Unsupported("native empty PES emission unobserved"));
        }
        let incomplete = self
            .body_limit
            .is_some_and(|expected| expected != self.payload_len);
        let kind = if !self.time_header_applies {
            NativePesPayloadKind::HeaderlessContinuation
        } else if header.declared_packet_length == 0 {
            NativePesPayloadKind::Unbounded
        } else if incomplete {
            NativePesPayloadKind::DeclaredIncomplete
        } else {
            NativePesPayloadKind::DeclaredComplete
        };
        self.emission = Some(Emission {
            boundary,
            kind,
            flags: if self.corrupt || forced_corrupt || incomplete {
                2
            } else {
                0
            },
            continue_without_header: continuation,
        });
        Ok(())
    }

    fn poll_emit(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<NativePesPacket>> {
        while self.output_header.len() < self.header_ranges.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !self.reserve(Arena::OutputHeader, work, cancel)? || !work.try_charge(1) {
                return Ok(AssemblyStep::Yield);
            }
            let range = self.header_ranges.as_slice()[self.output_header.len()];
            self.output_header
                .try_push(range)
                .map_err(|error| error.error)?;
        }
        if !work.try_charge(1) {
            return Ok(AssemblyStep::Yield);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let emission = self
            .emission
            .ok_or(Error::Invalid("native PES emission missing"))?;
        let header = self
            .header
            .as_ref()
            .ok_or(Error::Invalid("native PES causal header missing"))?
            .clone();
        let origin = self
            .origin
            .ok_or(Error::Invalid("native PES origin missing"))?;
        let packet = NativePesPacket {
            profile: self.profile,
            program: self.scope.program,
            pid: self.scope.pid,
            source_origin: origin,
            source_epochs: self.scope.epochs,
            native_position: self.native_position,
            native_flags: emission.flags,
            native_duration90_raw: 0,
            native_stream_id: header.stream_id,
            boundary: emission.boundary,
            payload_kind: emission.kind,
            source_header: header,
            header_source_ranges: std::mem::replace(
                &mut self.output_header,
                BudgetVec::new(&self.budget),
            ),
            time_header_applies: self.time_header_applies,
            payload_len: self.payload_len,
            spans: std::mem::replace(&mut self.spans, BudgetVec::new(&self.budget)),
        };
        self.payload_len = 0;
        self.corrupt = false;
        self.emission = None;
        if emission.continue_without_header {
            self.active = true;
            self.time_header_applies = false;
            self.body_limit = None;
        } else {
            self.active = false;
        }
        Ok(AssemblyStep::Output(packet))
    }

    fn finish_header(&mut self) -> Result<()> {
        let header = parse_header(&self.raw_header, self.header_used)?;
        if header.stream_id != self.scope.stream_id {
            return Err(Error::Unsupported(
                "native PES stream-id context unobserved",
            ));
        }
        self.body_limit = self
            .declared_total
            .map(|total| {
                total
                    .checked_sub(self.header_used)
                    .ok_or(Error::Invalid("PES header exceeds length"))
            })
            .transpose()?;
        self.header = Some(header);
        Ok(())
    }

    fn poll_header(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if self.header_used == self.header_target {
            if !work.try_charge(1) {
                return Ok(false);
            }
            match self.header_used {
                6 => {
                    if self.raw_header[..3] != [0, 0, 1] {
                        return Err(Error::Invalid("PES start code"));
                    }
                    if self.raw_header[3] != self.scope.stream_id {
                        return Err(Error::Unsupported(
                            "native PES stream-id context unobserved",
                        ));
                    }
                    let length =
                        usize::from(u16::from_be_bytes([self.raw_header[4], self.raw_header[5]]));
                    if length == 0 && self.finite_private() {
                        return Err(self.finite_private_error(
                            "native private audio unbounded PES unobserved",
                            "native DVB unbounded PES unobserved",
                        ));
                    }
                    if length != 0 {
                        let total = length + 6;
                        if total > self.limits.pes_bytes {
                            return Err(limit(total, self.limits.pes_bytes));
                        }
                        if total < 9 {
                            return Err(Error::Invalid("PES optional-header length"));
                        }
                        self.declared_total = Some(total);
                    }
                    self.header_target = 9;
                }
                9 => {
                    self.header_target = 9 + usize::from(self.raw_header[8]);
                    if self.header_target > self.limits.pes_bytes {
                        return Err(limit(self.header_target, self.limits.pes_bytes));
                    }
                    if self
                        .declared_total
                        .is_some_and(|total| self.header_target > total)
                    {
                        return Err(Error::Invalid("PES header exceeds length"));
                    }
                    if self.header_target == 9 {
                        self.finish_header()?;
                    }
                }
                _ => self.finish_header()?,
            }
            return Ok(true);
        }
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let payload = pending
            .packet
            .payload
            .as_ref()
            .ok_or(Error::Invalid("native PES payload missing"))?;
        if pending.position == payload.len() {
            self.pending = None;
            return Ok(true);
        }
        let extent = SourceRange {
            entity: self.scope.entity,
            offset: pending
                .packet
                .payload_source_offset
                .checked_add(pending.position as u64)
                .ok_or(Error::Overflow)?,
            bytes: 1,
        };
        let merge = self.header_ranges.as_slice().last().is_some_and(|last| {
            last.entity == extent.entity && last.end().ok() == Some(extent.offset)
        });
        if !merge && !self.reserve(Arena::Header, work, cancel)? {
            return Ok(false);
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let payload = pending
            .packet
            .payload
            .as_ref()
            .ok_or(Error::Invalid("native PES payload missing"))?
            .as_ref();
        let count = (self.header_target - self.header_used)
            .min(payload.len() - pending.position)
            .min(work.remaining().min(HEADER_MAX as u64) as usize + 1);
        let bytes = &payload[pending.position..pending.position + count];
        let extent = SourceRange {
            bytes: count,
            ..extent
        };
        let merge = prepare_range(&mut self.header_ranges, extent, self.limits.source_spans)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        commit_range(&mut self.header_ranges, extent, merge)?;
        self.raw_header[self.header_used..self.header_used + count].copy_from_slice(bytes);
        work.take(count as u64 - 1);
        self.header_used += count;
        self.pending
            .as_mut()
            .ok_or(Error::Invalid("native PES input missing"))?
            .position += count;
        Ok(true)
    }

    fn poll_start(&mut self, work: &mut WorkBudget) -> Result<bool> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let has_payload = pending
            .packet
            .payload
            .as_ref()
            .is_some_and(|bytes| !bytes.is_empty());
        if !pending.counter_checked {
            if !work.try_charge(1) {
                return Ok(false);
            }
            let gap = self.previous_counter.is_some_and(|previous| {
                pending.packet.continuity_counter != ((previous + u8::from(has_payload)) & 15)
            });
            if gap && self.finite_private() {
                return Err(self.finite_private_error(
                    "native private audio counter damage unobserved",
                    "native DVB counter damage unobserved",
                ));
            }
            if gap && !pending.packet.payload_unit_start && pending.packet.discontinuity_indicator()
            {
                return Err(Error::Unsupported(
                    "native signalled counter break unobserved",
                ));
            }
            if gap && !has_payload {
                return Err(Error::Unsupported(
                    "native adaptation-only counter break unobserved",
                ));
            }
            // The observed reference marks the active byte buffer before a
            // possible pre-append overflow flush. A new PUSI then starts clean.
            if gap && !pending.packet.payload_unit_start && self.active {
                self.corrupt = true;
            }
            self.previous_counter = Some(pending.packet.continuity_counter);
            self.pending
                .as_mut()
                .ok_or(Error::Invalid("native PES input missing"))?
                .counter_checked = true;
        }
        if !has_payload {
            self.pending = None;
            return Ok(true);
        }
        let start = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?
            .packet
            .payload_unit_start;
        if start && self.active {
            self.schedule(NativePesBoundary::NextPesStart, false, false)?;
            return Ok(true);
        }
        if start {
            if !work.try_charge(1) {
                return Ok(false);
            }
            self.begin_pes()?;
        } else if !self.active {
            return Err(Error::Unsupported("native orphan continuation unobserved"));
        }
        self.pending
            .as_mut()
            .ok_or(Error::Invalid("native PES input missing"))?
            .started = true;
        Ok(true)
    }

    fn poll_body(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let payload = pending
            .packet
            .payload
            .as_ref()
            .ok_or(Error::Invalid("native PES payload missing"))?;
        let available = payload.len() - pending.position;
        if available == 0 {
            self.pending = None;
            return Ok(true);
        }
        let mut count = available;
        if let Some(limit) = self.body_limit {
            let remaining = limit
                .checked_sub(self.payload_len)
                .ok_or(Error::Invalid("native finite buffer length"))?;
            if available > remaining {
                if self.finite_private() {
                    return Err(self.finite_private_error(
                        "native private audio finite overrun unobserved",
                        "native DVB finite overrun unobserved",
                    ));
                }
                if pending.packet.payload_unit_start {
                    // Observed finite-overlong PUSI: bound this admission to its
                    // declared PES bytes. The original raw carrier is unchanged.
                    count = remaining;
                } else {
                    self.schedule(NativePesBoundary::FiniteOverrun, true, true)?;
                    return Ok(true);
                }
            }
        }
        let next = self.payload_len.checked_add(count).ok_or(Error::Overflow)?;
        let retained = self.header_used.checked_add(next).ok_or(Error::Overflow)?;
        if retained > self.limits.pes_bytes {
            return Err(limit(retained, self.limits.pes_bytes));
        }
        if next >= DEFAULT_NATIVE_PACKET_CAP {
            return Err(Error::Unsupported("native packet-cap boundary unobserved"));
        }
        if !self.reserve(Arena::Payload, work, cancel)? || !work.try_charge(1) {
            return Ok(false);
        }
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("native PES input missing"))?;
        let payload = pending
            .packet
            .payload
            .as_ref()
            .ok_or(Error::Invalid("native PES payload missing"))?;
        let start = pending.position;
        let end = start.checked_add(count).ok_or(Error::Overflow)?;
        let span = NativePesSpan {
            bytes: payload.slice(start..end)?,
            source: SourceRange {
                entity: self.scope.entity,
                offset: pending
                    .packet
                    .payload_source_offset
                    .checked_add(start as u64)
                    .ok_or(Error::Overflow)?,
                bytes: count,
            },
            transport: SourceRange {
                entity: self.scope.entity,
                offset: pending.packet.source_offset,
                bytes: pending.packet.source.len(),
            },
            context: pending.context,
        };
        span.source.end()?;
        span.transport.end()?;
        let next_position = if count < available {
            payload.len()
        } else {
            end
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.spans.try_push(span).map_err(|error| error.error)?;
        self.payload_len = next;
        // A bounded <=184-byte admission per work unit, not a per-yield span.
        self.pending
            .as_mut()
            .ok_or(Error::Invalid("native PES input missing"))?
            .position = next_position;
        Ok(true)
    }

    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<NativePesPacket>> {
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
            if self.active && self.header.is_some() && self.body_limit == Some(self.payload_len) {
                if self.pending.as_ref().is_some_and(|pending| {
                    pending
                        .packet
                        .payload
                        .as_ref()
                        .is_some_and(|payload| pending.position == payload.len())
                }) {
                    self.pending = None;
                }
                self.schedule(NativePesBoundary::FiniteLength, false, false)?;
                continue;
            }
            if self.pending.is_none() {
                if !self.ended {
                    return Ok(AssemblyStep::NeedPayload);
                }
                if !self.active {
                    return Ok(AssemblyStep::Finished);
                }
                self.schedule(NativePesBoundary::LogicalEof, false, false)?;
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
