//! Source access units composed from the transport reader and existing framers.
mod lane;
mod model;
use crate::clock::{ClockKey, SourceTimePolicy};
use crate::compat::adts::{self, PacketPolicy};
use crate::compat::annex_b::VideoEofPolicy;
use crate::compat::latm;
use crate::compat::pes::UnframedAudioPolicy;
use crate::psi::DescriptorNamespace;
use crate::{
    CarrierObservation, ContinuityOutcome, PacketFormat, ParseLimits, TraceEvent, TransportStream,
};
use lane::{Lane, LaneStep, PesRun, StorageScan};
pub use model::{
    Ac3AccessUnit, AccessUnit, DvbSubtitleUnit, IncompleteVideo, LoasFrame, Packet, PacketClock,
    PayloadSource, ReaderEvent, SourceEvent, Track, UnframedAudio,
};
use model::{ClockContext, Shared};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{ContainerReader, Event, ReaderStep, SeekRequest};
use triergon_core::{
    Backpressure, Budget, BudgetVec, Error, InputCursor, InputRequest, InputToken, InputWindow,
    Result, SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};
use triergon_framing::RawAnnexBLimits;

/// Reporting of unfinished audio bytes at logical source EOF.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioEofPolicy {
    /// Keep the audio framer's truncation error.
    #[default]
    RejectIncomplete,
    /// Report validated PES bytes as a source fragment, never as a complete AU.
    ReportIncomplete,
}

/// Reporting of an overlong final DVB subtitle segment in a validated finite PES.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DvbSubtitleTailPolicy {
    #[default]
    RejectTruncated,
    /// Report complete preceding segments as a source event with the original PES.
    ReportCompletePrefix,
}

/// Interpretation of the next physical entity in a transport stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityTransition {
    /// Continue PSI, PES, clock and access-unit state across the boundary.
    Contiguous,
    /// Start a new configuration and clock epoch, reporting interrupted units.
    Discontinuity,
}

#[derive(Clone, Copy)]
struct NextEntity {
    source: SourceId,
    capabilities: SourceCapabilities,
    transition: EntityTransition,
}

/// Bounds and source interpretation for the AU reader, independent of its live
/// storage budget. An absent program selects the sole PAT media program;
/// transports with several programs require an explicit selection.
#[derive(Clone, Copy, Debug)]
pub struct ReaderLimits {
    pub format: PacketFormat,
    pub program: Option<u16>,
    pub descriptor_namespace: DescriptorNamespace,
    pub transport: ParseLimits,
    pub video: RawAnnexBLimits,
    /// Maximum bytes in one audio syncframe. E-AC-3 groups have a derived bound.
    pub maximum_audio_frame_bytes: usize,
    pub audio_eof_policy: AudioEofPolicy,
    pub unframed_audio_policy: UnframedAudioPolicy,
    /// Framing policy for ADTS. The compatibility policy handles ADTS EOF itself.
    pub adts_packet_policy: PacketPolicy,
    /// Wire policy for ADTS PES with a zero packet-length field.
    pub adts_pes_policy: adts::PesPolicy,
    /// LOAS envelope framing by default. PES preservation emits source events.
    pub latm_packet_policy: latm::PacketPolicy,
    pub dvb_subtitle_tail_policy: DvbSubtitleTailPolicy,
    pub video_eof_policy: VideoEofPolicy,
    pub maximum_access_units: usize,
    pub maximum_pes_per_access_unit: usize,
    pub elementary_input_window: usize,
    /// Period-zero timestamps by default. Every lifted value carries its
    /// SourceTimeStatus; no missing DTS or per-frame timestamp is synthesized.
    pub time_policy: SourceTimePolicy,
}

impl Default for ReaderLimits {
    fn default() -> Self {
        Self {
            format: PacketFormat::Ts188,
            program: None,
            descriptor_namespace: DescriptorNamespace::Unspecified,
            transport: ParseLimits::default(),
            video: RawAnnexBLimits::default(),
            maximum_audio_frame_bytes: 8192,
            audio_eof_policy: AudioEofPolicy::RejectIncomplete,
            unframed_audio_policy: UnframedAudioPolicy::Strict,
            adts_packet_policy: PacketPolicy::Strict,
            adts_pes_policy: adts::PesPolicy::Strict,
            latm_packet_policy: latm::PacketPolicy::Strict,
            dvb_subtitle_tail_policy: DvbSubtitleTailPolicy::RejectTruncated,
            video_eof_policy: VideoEofPolicy::RejectUnframed,
            maximum_access_units: 32_000_000,
            maximum_pes_per_access_unit: 4096,
            elementary_input_window: 64 * 1024,
            time_policy: SourceTimePolicy::RelativeEpochZero,
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Prepare,
    Begin,
    DrainAll { index: usize, retained: usize },
    Build(usize),
    DrainLane(usize),
    FaultLane(usize),
    Feed(usize),
    Fragments(usize),
    FragmentOutput(usize),
    Publish,
}

/// Incremental TS188/M2TS192 reader delivering complete source AUs, separately
/// from every original transport event. This performs no I/O or decoder-feed
/// injection. Unsupported carriages retain source facts and report a typed gap.
pub struct Reader {
    pool: Budget,
    source: SourceId,
    caps: SourceCapabilities,
    window: usize,
    limits: ReaderLimits,
    input: Option<InputCursor>,
    stream: Option<TransportStream>,
    lanes: BudgetVec<Lane>,
    tracks: BudgetVec<Track>,
    staged_tracks: BudgetVec<Track>,
    map: Option<SourceEvent>,
    raw: Option<TraceEvent>,
    pending: Option<SourceEvent>,
    phase: Phase,
    rebuild: Option<SourceEvent>,
    active: Option<usize>,
    queued_pes: Option<(usize, SourceEvent, ClockContext)>,
    unsupported_at: Option<usize>,
    tsid: Option<u16>,
    boundary: Option<SourceEvent>,
    next_entity: Option<NextEntity>,
    end: Option<SourceLocation>,
    finished: bool,
    failure: Option<Error>,
    deferred_fragment: bool,
    storage: Option<StorageScan>,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("source", &self.source)
            .field("limits", &self.limits)
            .field("tracks", &self.lanes.len())
            .field("finished", &self.finished)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl Reader {
    fn keeps_lane(&self, index: usize, source: &SourceEvent) -> bool {
        if !matches!(source.event(), TraceEvent::ProgramMap { active: true, .. }) {
            return false;
        }
        let lane = &self.lanes.as_slice()[index];
        let old = lane.track.program_map();
        let new = source.map();
        if old.pcr_pid != new.pcr_pid || old.header.extension != new.header.extension {
            return false;
        }
        // A version change alone still marks a source boundary. A changed
        // stream list can leave other routes continuous.
        if old.streams.len() == new.streams.len()
            && old
                .streams
                .iter()
                .zip(new.streams.iter())
                .all(|(old, new)| {
                    old.pid == new.pid
                        && old.stream_type == new.stream_type
                        && old.descriptor_bytes.as_ref() == new.descriptor_bytes.as_ref()
                })
        {
            return false;
        }
        let stream = lane.track.stream();
        if matches!(stream.stream_type, 0x06 | 0x81 | 0x87)
            && old.program_descriptors.as_ref() != new.program_descriptors.as_ref()
        {
            return false;
        }
        new.streams
            .iter()
            .position(|entry| entry.pid == stream.pid)
            .and_then(|position| self.stream.as_ref()?.route_state(position, stream.pid))
            .is_some_and(|(epochs, pending)| {
                epochs.configuration == lane.track.epoch
                    && !matches!(lane.run, PesRun::Active(old, _) if old != epochs)
                    && !matches!(lane.run, PesRun::BeforeBoundary(_))
                    && pending.is_none_or(|start| start.source == self.source)
            })
    }

    pub fn new(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        limits: ReaderLimits,
        input_window: usize,
    ) -> Result<Self> {
        if limits.maximum_audio_frame_bytes == 0
            || limits.maximum_access_units == 0
            || limits.maximum_pes_per_access_unit == 0
            || limits.elementary_input_window == 0
            || limits.program == Some(0)
        {
            return Err(Error::Invalid("MPEG-TS access-unit reader limits"));
        }
        let mut stream = TransportStream::new_with_program(
            pool.clone(),
            limits.format,
            limits.transport,
            limits.program,
            limits.descriptor_namespace,
        )?;
        stream.share_input();
        if limits.adts_pes_policy == adts::PesPolicy::ReportUnbounded {
            stream.allow_unbounded_adts();
        }
        let input = InputCursor::new(pool, source, caps, input_window)?;
        Ok(Self {
            pool: pool.clone(),
            source,
            caps,
            window: input_window,
            limits,
            input: Some(input),
            stream: Some(stream),
            lanes: BudgetVec::new(pool),
            tracks: BudgetVec::new(pool),
            staged_tracks: BudgetVec::new(pool),
            map: None,
            raw: None,
            pending: None,
            phase: Phase::Begin,
            rebuild: None,
            active: None,
            queued_pes: None,
            unsupported_at: None,
            tsid: None,
            boundary: None,
            next_entity: None,
            end: None,
            finished: false,
            failure: None,
            deferred_fragment: false,
            storage: None,
        })
    }
    pub(crate) fn continuity_source(&self) -> Option<(&InputCursor, Option<&TransportStream>)> {
        Some((self.input.as_ref()?, self.stream.as_ref()))
    }
    pub fn tracks(&self) -> &[Track] {
        self.tracks.as_slice()
    }
    pub fn request(&self) -> Result<InputRequest> {
        self.healthy()?;
        Ok(self
            .input
            .as_ref()
            .ok_or(Error::Invalid("MPEG-TS reader closed"))?
            .request())
    }
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.healthy()?;
        self.input
            .as_mut()
            .ok_or(Error::Invalid("MPEG-TS reader closed"))?
            .provide(window)?;
        if self.storage.is_some() && !window.bytes().is_empty() {
            // An early reply can add backing storage while a growth audit yields.
            self.storage = None;
        }
        Ok(())
    }
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        let input = self
            .input
            .as_mut()
            .ok_or(Error::Invalid("MPEG-TS reader closed"))?;
        let result = input.finish(source, token);
        self.caps = input.capabilities();
        if let Err(error) = result
            && error == Error::Truncated
        {
            self.stop(error);
        }
        result
    }
    /// Complete one entity and queue its successor. The reader consumes the
    /// final supplied bytes before requesting offset zero of the next entity.
    /// The generation is preserved and the request number advances.
    /// Use [Self::finish] for the last entity of the logical stream.
    pub fn finish_entity(
        &mut self,
        source: SourceId,
        token: InputToken,
        next: SourceId,
        capabilities: SourceCapabilities,
        transition: EntityTransition,
    ) -> Result<()> {
        self.healthy()?;
        if self.next_entity.is_some() || self.end.is_some() {
            return Err(Error::Invalid(
                "MPEG-TS entity completion already specified",
            ));
        }
        if self.source == next {
            return Err(Error::SourceMismatch);
        }
        token.request().checked_add(1).ok_or(Error::Overflow)?;
        self.finish(source, token)?;
        self.next_entity = Some(NextEntity {
            source: next,
            capabilities,
            transition,
        });
        Ok(())
    }
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.healthy()?;
        self.input
            .as_mut()
            .ok_or(Error::Invalid("MPEG-TS reader closed"))?
            .fail(source, token, failure)?;
        self.stop(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }
    /// Release reader-owned state. Previously returned packets/events remain valid.
    pub fn close(&mut self) {
        self.stop(Error::Closed);
    }
    /// Restart the current entity with fresh transport state and a new generation.
    pub fn reset(&mut self) -> Result<InputRequest> {
        let next = Self::new(&self.pool, self.source, self.caps, self.limits, self.window)?;
        let request = next.request()?;
        *self = next;
        Ok(request)
    }
    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }
    fn stop(&mut self, error: Error) {
        self.failure = Some(error);
        self.input = None;
        self.stream = None;
        self.lanes = BudgetVec::new(&self.pool);
        self.tracks = BudgetVec::new(&self.pool);
        self.staged_tracks = BudgetVec::new(&self.pool);
        self.pending = None;
        self.raw = None;
        self.map = None;
        self.rebuild = None;
        self.active = None;
        self.queued_pes = None;
        self.unsupported_at = None;
        self.boundary = None;
        self.next_entity = None;
        self.storage = None;
    }
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        self.step_observing::<false>(work, cancel, |_| ())
    }

    /// Inspect carriers and ordinary continuity events in source order, then
    /// return the next packet, other source event or progress step.
    ///
    /// The callback borrows each carrier within the caller's work grant. The
    /// reader preserves the input, yield and backpressure protocol of [Self::step].
    pub fn step_with_carriers(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        mut observe: impl FnMut(CarrierObservation<'_>),
    ) -> Result<ReaderStep<Self>> {
        loop {
            let step = self.step_observing::<true>(work, cancel, &mut observe)?;
            if let Step::Ready(Event::Other(event)) = &step
                && let Some(event) = event.carrier_observation()
            {
                observe(event);
            } else {
                return Ok(step);
            }
        }
    }

    fn step_observing<const OBSERVING: bool>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        observe: impl FnMut(CarrierObservation<'_>),
    ) -> Result<ReaderStep<Self>> {
        self.healthy()?;
        match self.advance::<OBSERVING>(work, cancel, observe) {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(Step::Backpressure(Backpressure {
                requested,
                available,
            })),
            Err(error) => {
                if error != Error::Allocation {
                    self.stop(error);
                }
                Err(error)
            }
            result => result,
        }
    }
    fn output(packet: Packet) -> Event<Packet, ReaderEvent> {
        if packet.0.recovered_adts {
            return Event::Other(ReaderEvent::RecoveredAdts(packet));
        }
        match packet.access_unit() {
            AccessUnit::DvbSubtitle(unit) if unit.truncated => {
                Event::Other(ReaderEvent::DvbSubtitlePrefix(packet))
            }
            AccessUnit::IncompleteAudio { .. } => {
                Event::Other(ReaderEvent::IncompleteAudio(packet))
            }
            AccessUnit::AdtsFragment(_) => Event::Other(ReaderEvent::AdtsFragment(packet)),
            AccessUnit::Video(unit) if !unit.has_vcl => {
                Event::Other(ReaderEvent::VideoHeaders(packet))
            }
            _ => Event::Packet(packet),
        }
    }
    fn is_inline_source(event: &TraceEvent) -> bool {
        matches!(
            event,
            TraceEvent::Transport(_)
                | TraceEvent::Continuity {
                    outcome: ContinuityOutcome::First | ContinuityOutcome::Continuous,
                    ..
                }
        )
    }
    fn inline_source(event: TraceEvent) -> ReaderStep<Self> {
        let event = match event {
            TraceEvent::Transport(packet) => ReaderEvent::Transport(packet),
            TraceEvent::Continuity { pid, outcome } => ReaderEvent::Continuity { pid, outcome },
            _ => unreachable!("expected an inline source event"),
        };
        Step::Ready(Event::Other(event))
    }
    fn lane_step(
        &mut self,
        index: usize,
        draining: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<ReaderStep<Self>>> {
        match self.lanes.as_mut_slice()[index].step(draining, work, cancel)? {
            LaneStep::Packet(packet) => Ok(Some(Step::Ready(Self::output(packet)))),
            LaneStep::IncompleteVideo(video) => Ok(Some(Step::Ready(Event::Other(
                ReaderEvent::IncompleteVideo(video),
            )))),
            LaneStep::LatmPes(packet) => Ok(Some(Step::Ready(Event::Other(ReaderEvent::LatmPes(
                packet,
            ))))),
            LaneStep::UnframedAudio(audio) => Ok(Some(Step::Ready(Event::Other(
                ReaderEvent::UnframedAudio(audio),
            )))),
            LaneStep::Yield => Ok(Some(Step::Yield)),
            LaneStep::DeferredFragment => {
                self.deferred_fragment = true;
                Ok(None)
            }
            LaneStep::End | LaneStep::NeedPayload => Ok(None),
        }
    }
    fn advance<const OBSERVING: bool>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        mut observe: impl FnMut(CarrierObservation<'_>),
    ) -> Result<ReaderStep<Self>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            if let Some(raw) = &self.raw {
                if matches!(self.phase, Phase::Prepare) {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if !Self::is_inline_source(raw) {
                        let charge = Shared::<TraceEvent>::reserve(&self.pool)?;
                        let event = self.raw.take().expect("raw source");
                        self.pending = Some(SourceEvent(Shared::new(event, charge)));
                    }
                    self.phase = Phase::Begin;
                }
                if self.raw.is_some() {
                    // Inline source events keep the original begin and publish
                    // work boundaries.
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if matches!(self.phase, Phase::Begin) {
                        self.phase = Phase::Publish;
                        continue;
                    }
                    return Ok(Self::inline_source(self.raw.take().expect("inline source")));
                }
            }
            if self.pending.is_some() {
                match self.phase {
                    Phase::Prepare => unreachable!("pending source is prepared"),
                    Phase::Begin => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let source = self.pending.as_ref().expect("pending source");
                        match source.event() {
                            TraceEvent::Pat(table) => {
                                self.tsid = Some(table.header.extension);
                                self.phase = if self.map.is_some()
                                    && !self.stream.as_ref().expect("active transport").has_active_map()
                                {
                                    Phase::DrainAll { index: 0, retained: 0 }
                                } else {
                                    Phase::Publish
                                };
                            }
                            TraceEvent::ProgramMap { active: true, configuration_epoch, .. }
                                if self.map.as_ref().is_none_or(|map| !matches!(map.event(), TraceEvent::ProgramMap { configuration_epoch: old, .. } if old == configuration_epoch)) => {
                                    self.rebuild = Some(source.clone());
                                    self.phase = Phase::DrainAll { index: 0, retained: 0 };
                                }
                            TraceEvent::Discontinuity { .. } | TraceEvent::Finished => {
                                self.rebuild = None;
                                self.phase = Phase::DrainAll { index: 0, retained: 0 };
                            }
                            TraceEvent::Timeline { .. } => {
                                self.rebuild = self.map.clone();
                                self.phase = Phase::DrainAll { index: 0, retained: 0 };
                            }
                            TraceEvent::Continuity { pid, outcome: ContinuityOutcome::SignalledDiscontinuity } => {
                                self.phase = self.lanes.as_slice().iter()
                                    .position(|lane| lane.track.stream().pid == *pid)
                                    .map_or(Phase::Publish, Phase::FaultLane);
                            }
                            TraceEvent::Pes(pes) | TraceEvent::UnboundedAdtsPes(adts::UnboundedPes(pes)) => {
                                if let Some(index) = self.lanes.as_slice().iter().position(|lane| lane.track.stream().pid == pes.pid) {
                                    let lane = &mut self.lanes.as_mut_slice()[index];
                                    if let PesRun::BeforeBoundary(start) = lane.run {
                                        lane.run = PesRun::Idle;
                                        if let Some(boundary) = &self.boundary
                                            && pes.header_source_ranges.as_slice().first().is_some_and(|range| {
                                                range.entity == start.source && range.offset == start.offset
                                            })
                                        {
                                            let event = ReaderEvent::InterruptedPes {
                                                source: source.clone(),
                                                boundary: boundary.clone(),
                                            };
                                            self.phase = Phase::Publish;
                                            return Ok(Step::Ready(Event::Other(event)));
                                        }
                                    }
                                    self.phase = match lane.run {
                                        PesRun::Active(epochs, byte_run)
                                            if epochs != pes.epochs || byte_run != pes.byte_run_epoch => Phase::DrainLane(index),
                                        _ => Phase::Feed(index),
                                    };
                                } else { self.phase = Phase::Publish; }
                            }
                            TraceEvent::Diagnostic(fault) if matches!(fault.kind, crate::FaultKind::Pes | crate::FaultKind::Continuity | crate::FaultKind::Transport) => {
                                if let Some(index) = self.lanes.as_slice().iter().position(|lane| Some(lane.track.stream().pid) == fault.pid) {
                                    self.phase = Phase::FaultLane(index);
                                } else if fault.pid.is_none() {
                                    self.rebuild = self.map.clone();
                                    self.phase = Phase::DrainAll { index: 0, retained: 0 };
                                } else { self.phase = Phase::Publish; }
                            }
                            TraceEvent::Diagnostic(fault) if fault.kind == crate::FaultKind::Program => {
                                self.rebuild = self.map.clone();
                                self.phase = Phase::DrainAll { index: 0, retained: 0 };
                            }
                            _ => self.phase = Phase::Publish,
                        }
                    }
                    Phase::DrainAll {
                        index,
                        mut retained,
                    } => {
                        if index < self.lanes.len() {
                            let cause = self.pending.as_ref().expect("pending boundary");
                            if matches!(cause.event(), TraceEvent::Finished) {
                                if let Some(step) = self.lane_step(index, true, work, cancel)? {
                                    return Ok(step);
                                }
                            } else {
                                if !work.try_charge(1) {
                                    return Ok(Step::Yield);
                                }
                                if self.keeps_lane(index, cause) {
                                    self.lanes.as_mut_slice().swap(index, retained);
                                    retained += 1;
                                } else if let Some(elementary_range) =
                                    self.lanes.as_slice()[index].unpublished()
                                {
                                    let event = ReaderEvent::Interrupted {
                                        track: self.lanes.as_slice()[index].track.clone(),
                                        elementary_range,
                                        cause: cause.clone(),
                                    };
                                    self.phase = Phase::DrainAll {
                                        index: index + 1,
                                        retained,
                                    };
                                    return Ok(Step::Ready(Event::Other(event)));
                                }
                            }
                            self.phase = Phase::DrainAll {
                                index: index + 1,
                                retained,
                            };
                        } else {
                            if self.deferred_fragment
                                && matches!(
                                    self.pending.as_ref().expect("pending boundary").event(),
                                    TraceEvent::Finished
                                )
                            {
                                // No transport or framer can produce more bytes.
                                // Release opaque parser storage before auditing
                                // the remaining owners for fragment publication.
                                self.stream = None;
                                self.boundary = None;
                                self.active = None;
                                self.phase = Phase::Fragments(0);
                                continue;
                            }
                            if !matches!(
                                self.pending.as_ref().expect("pending boundary").event(),
                                TraceEvent::Finished
                            ) {
                                self.boundary = self.pending.clone();
                                self.lanes.truncate(retained);
                                self.staged_tracks.clear();
                            }
                            self.active = None;
                            self.phase = if self.rebuild.is_some() {
                                Phase::Build(0)
                            } else {
                                Phase::Publish
                            };
                        }
                    }
                    Phase::Build(index) => {
                        let source = self.rebuild.as_ref().expect("pending map");
                        if index < source.map().streams.len() {
                            if !work.try_charge(1) {
                                return Ok(Step::Yield);
                            }
                            self.lanes
                                .try_reserve_exact(source.map().streams.len() - self.lanes.len())?;
                            self.staged_tracks.try_reserve_exact(
                                source.map().streams.len() - self.staged_tracks.len(),
                            )?;
                            // Remember the exact open PES in a reused route.
                            // Physical offsets can restart in every entity.
                            let (epochs, pending_header) = self
                                .stream
                                .as_ref()
                                .expect("active transport")
                                .route_state(index, source.map().streams.as_slice()[index].pid)
                                .ok_or(Error::Invalid("MPEG-TS track route missing"))?;
                            let track = Track {
                                source: source.clone(),
                                index,
                                epoch: epochs.configuration,
                            };
                            let retained = self.lanes.as_slice()[index..]
                                .iter()
                                .position(|lane| lane.track.stream().pid == track.stream().pid);
                            let position = if let Some(retained) = retained {
                                let position = index + retained;
                                self.lanes.as_mut_slice()[position].track = track.clone();
                                position
                            } else {
                                let mut lane = Lane::new(&self.pool, track.clone(), self.limits)?;
                                if let Some(start) = pending_header {
                                    lane.run = PesRun::BeforeBoundary(start);
                                }
                                let position = self.lanes.len();
                                self.lanes.try_push(lane).map_err(|e| e.error)?;
                                position
                            };
                            self.lanes.as_mut_slice().swap(index, position);
                            self.staged_tracks.try_push(track).map_err(|e| e.error)?;
                            self.phase = Phase::Build(index + 1);
                        } else {
                            self.unsupported_at = Some(0);
                            self.phase = Phase::Publish;
                        }
                    }
                    Phase::DrainLane(index) | Phase::FaultLane(index) => {
                        let fault = matches!(self.phase, Phase::FaultLane(_));
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let previous = &self.lanes.as_slice()[index];
                        let track = previous.track.clone();
                        let interrupted = previous.unpublished().map(|elementary_range| {
                            ReaderEvent::Interrupted {
                                track: track.clone(),
                                elementary_range,
                                cause: self.pending.as_ref().expect("pending PES").clone(),
                            }
                        });
                        let mut lane = Lane::new(&self.pool, track, self.limits)?;
                        // A clock fault can restart framing while the transport
                        // still holds the PES that began before the program change.
                        if let PesRun::BeforeBoundary(start) = previous.run
                            && self
                                .stream
                                .as_ref()
                                .and_then(|stream| {
                                    stream.route_state(index, lane.track.stream().pid)
                                })
                                .is_some_and(|(_, pending)| pending == Some(start))
                        {
                            lane.run = previous.run;
                        }
                        self.lanes.as_mut_slice()[index] = lane;
                        self.phase = if fault {
                            Phase::Publish
                        } else {
                            Phase::Feed(index)
                        };
                        if let Some(event) = interrupted {
                            return Ok(Step::Ready(Event::Other(event)));
                        }
                    }
                    Phase::Fragments(index) => {
                        if index == self.lanes.len() {
                            self.phase = Phase::Publish;
                        } else if self.lanes.as_slice()[index].has_incomplete() {
                            let mut storage = match self.storage.take() {
                                Some(storage) => storage,
                                None => StorageScan::new(self)?,
                            };
                            if !storage.step(self, work, cancel)? {
                                self.storage = Some(storage);
                                return Ok(Step::Yield);
                            }
                            self.lanes.as_mut_slice()[index].set_reader_storage(storage.bytes());
                            self.phase = Phase::FragmentOutput(index);
                        } else {
                            if !work.try_charge(1) {
                                return Ok(Step::Yield);
                            }
                            self.phase = Phase::Fragments(index + 1);
                        }
                    }
                    Phase::FragmentOutput(index) => {
                        if let Some(step) = self.lane_step(index, true, work, cancel)? {
                            return Ok(step);
                        }
                        self.phase = Phase::Fragments(index + 1);
                    }
                    Phase::Feed(_) | Phase::Publish => {}
                }
                // Source admission can advance through queueing and publication
                // under the same grant. Each phase remains resumable.
                if let Phase::Feed(index) = self.phase {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let source = self.pending.as_ref().expect("pending PES");
                    let pes = source.pes();
                    let track = &self.lanes.as_slice()[index].track;
                    let key = ClockKey {
                        tsid: self.tsid.ok_or(Error::Invalid("MPEG-TS PES before PAT"))?,
                        program: track.program(),
                        pcr_pid: track.program_map().pcr_pid,
                        epoch: pes.epochs.clock,
                    };
                    let clock = ClockContext {
                        key,
                        quarantined: self
                            .stream
                            .as_ref()
                            .expect("active transport")
                            .clock_is_quarantined(),
                    };
                    self.queued_pes = Some((index, source.clone(), clock));
                    self.phase = Phase::Publish;
                }
                if matches!(self.phase, Phase::Publish) {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if let Some(map) = self.rebuild.take() {
                        self.map = Some(map);
                        self.tracks =
                            std::mem::replace(&mut self.staged_tracks, BudgetVec::new(&self.pool));
                    }
                    let source = self.pending.take().expect("published source");
                    match source.event() {
                        TraceEvent::Pat(_)
                            if !self
                                .stream
                                .as_ref()
                                .expect("active transport")
                                .has_active_map() =>
                        {
                            self.map = None;
                            self.tracks.clear();
                            self.boundary = None;
                        }
                        TraceEvent::Discontinuity { .. } => {
                            self.map = None;
                            self.tracks.clear();
                            self.tsid = None;
                            self.boundary = None;
                        }
                        TraceEvent::Finished => self.finished = true,
                        _ => (),
                    }
                    return Ok(Step::Ready(Event::Other(ReaderEvent::Source(source))));
                }
                continue;
            }
            if let Some((index, source, key)) = self.queued_pes.take() {
                let mut source = Some(source);
                match self.lanes.as_mut_slice()[index].push(&mut source, key, work) {
                    Ok(true) => self.active = Some(index),
                    retry => {
                        self.queued_pes = Some((index, source.expect("queued PES on retry"), key));
                        retry?;
                        return Ok(Step::Yield);
                    }
                }
                continue;
            }
            if let Some(index) = self.unsupported_at {
                if !work.try_charge(1) {
                    return Ok(Step::Yield);
                }
                self.unsupported_at = (index + 1 < self.lanes.len()).then_some(index + 1);
                if let Some(lane) = self.lanes.as_slice().get(index)
                    && let Some(error) = lane.unsupported
                {
                    return Ok(Step::Ready(Event::Other(ReaderEvent::Unsupported {
                        track: lane.track.clone(),
                        error,
                    })));
                }
                continue;
            }
            if let Some(index) = self.active {
                if let Some(step) = self.lane_step(index, false, work, cancel)? {
                    return Ok(step);
                }
                self.active = None;
                continue;
            }
            if self.finished {
                return Ok(Step::End(
                    self.end
                        .ok_or(Error::Invalid("MPEG-TS missing source end"))?,
                ));
            }
            let stream = self
                .stream
                .as_mut()
                .ok_or(Error::Invalid("MPEG-TS reader closed"))?;
            let input = self
                .input
                .as_mut()
                .ok_or(Error::Invalid("MPEG-TS reader closed"))?;
            let mut observed = false;
            let result = stream.poll_observing::<OBSERVING>(input, work, cancel, |event| {
                observed = true;
                observe(event);
            });
            if observed {
                self.storage = None;
            }
            let step = match result {
                Err(error @ Error::Backpressure { .. }) => {
                    self.audit_pes_growth(error, work, cancel)?
                }
                result => {
                    // Any transport progress ends the snapshot used for the audit.
                    self.storage = None;
                    result?
                }
            };
            match step {
                Step::Ready(event) => {
                    // Prepare, begin and publish can share a grant for events
                    // that require no additional state or storage.
                    if Self::is_inline_source(&event) && work.remaining() >= 3 {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        work.take(3);
                        self.phase = Phase::Publish;
                        return Ok(Self::inline_source(event));
                    }
                    self.raw = Some(event);
                    self.phase = Phase::Prepare;
                }
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(location) => {
                    let stream = self.stream.as_mut().expect("active transport");
                    let input = self.input.as_mut().expect("active input");
                    if let Some(next) = self.next_entity {
                        let Some(end) = stream.media_entity_end(input, work, cancel)? else {
                            return Ok(Step::Yield);
                        };
                        if next.transition == EntityTransition::Discontinuity {
                            stream.discontinuity()?;
                        }
                        input.continue_source(next.source, next.capabilities)?;
                        end.validate_continuation(input)?;
                        self.source = next.source;
                        self.caps = next.capabilities;
                        self.next_entity = None;
                    } else {
                        stream.end_of_source()?;
                        self.end = Some(location);
                    }
                }
            }
        }
    }

    #[cold]
    fn audit_pes_growth(
        &mut self,
        error: Error,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<TraceEvent>> {
        let stream = self.stream.as_ref().expect("blocked transport");
        let Some(floor) = stream.blocked_pes_growth_floor()? else {
            self.storage = None;
            return Err(error);
        };
        let mut scan = match self.storage.take() {
            Some(scan) => scan,
            None => StorageScan::pes_growth(self, floor)?,
        };
        if !scan.step(self, work, cancel)? {
            self.storage = Some(scan);
            return Ok(Step::Yield);
        }
        if scan.bytes() > self.pool.limit() {
            if !work.try_charge(1) {
                self.storage = Some(scan);
                return Ok(Step::Yield);
            }
            let limit = crate::limit(scan.bytes(), self.pool.limit());
            let stream = self.stream.as_mut().expect("blocked transport");
            return Ok(Step::Ready(stream.refuse_blocked_pes_growth(limit)));
        }
        self.storage = Some(scan);
        Err(error)
    }
}

impl ContainerReader for Reader {
    type Packet = Packet;
    type Track = Track;
    type Other = ReaderEvent;
    fn provide(&mut self, window: &InputWindow) -> Result<()> {
        Self::provide(self, window)
    }
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        Self::finish(self, source, token)
    }
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()> {
        Self::fail(self, source, token, failure)
    }
    fn reset(&mut self) -> Result<InputRequest> {
        Self::reset(self)
    }
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        Self::step(self, work, cancel)
    }
    fn request_seek(&mut self, _request: SeekRequest) -> Result<InputRequest> {
        self.healthy()?;
        Err(Error::Unsupported("MPEG-TS access-unit seeking"))
    }
    fn track_count(&self) -> usize {
        self.tracks.len()
    }
    fn track(&self, index: usize) -> Option<&Track> {
        self.tracks.as_slice().get(index)
    }
    fn capabilities(&self) -> SourceCapabilities {
        self.caps
    }
}
