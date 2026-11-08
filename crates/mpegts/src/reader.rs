//! Source access units composed from the transport reader and existing framers.
mod lane;
mod model;
use crate::clock::{ClockKey, SourceTimePolicy};
use crate::psi::DescriptorNamespace;
use crate::{PacketFormat, ParseLimits, TraceEvent, TransportStream};
use lane::{Lane, LaneStep};
pub use model::{AccessUnit, Packet, PacketClock, PayloadSource, ReaderEvent, SourceEvent, Track};
use model::{ClockContext, Shared};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{ContainerReader, Event, ReaderStep, SeekRequest};
use triergon_core::{
    Backpressure, Budget, BudgetVec, Error, InputCursor, InputRequest, InputToken, InputWindow,
    Result, SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};
use triergon_framing::RawAnnexBLimits;

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
    pub maximum_audio_frame_bytes: usize,
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
            maximum_access_units: 32_000_000,
            maximum_pes_per_access_unit: 4096,
            elementary_input_window: 64 * 1024,
            time_policy: SourceTimePolicy::RelativeEpochZero,
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Begin,
    DrainAll(usize),
    Build(usize),
    DrainLane(usize),
    FaultLane(usize),
    Feed(usize),
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
    end: Option<SourceLocation>,
    finished: bool,
    failure: Option<Error>,
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
        let stream = TransportStream::new_with_program(
            pool.clone(),
            limits.format,
            limits.transport,
            limits.program,
            limits.descriptor_namespace,
        )?;
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
            end: None,
            finished: false,
            failure: None,
        })
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
            .provide(window)
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
    }
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        self.healthy()?;
        match self.advance(work, cancel) {
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
        if matches!(packet.access_unit(), AccessUnit::Video(unit) if !unit.has_vcl) {
            Event::Other(ReaderEvent::VideoHeaders(packet))
        } else {
            Event::Packet(packet)
        }
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
            LaneStep::Yield => Ok(Some(Step::Yield)),
            LaneStep::End | LaneStep::NeedPayload => Ok(None),
        }
    }
    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            if self.raw.is_some() {
                if !work.try_charge(1) {
                    return Ok(Step::Yield);
                }
                let charge = Shared::<TraceEvent>::reserve(&self.pool)?;
                self.pending = self
                    .raw
                    .take()
                    .map(|event| SourceEvent(Shared::new(event, charge)));
                self.phase = Phase::Begin;
            }
            if self.pending.is_some() {
                match self.phase {
                    Phase::Begin => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        let source = self.pending.as_ref().expect("pending source");
                        match source.event() {
                            TraceEvent::Pat(table) => { self.tsid = Some(table.header.extension); self.phase = Phase::Publish; }
                            TraceEvent::ProgramMap { active: true, configuration_epoch, .. }
                                if self.map.as_ref().is_none_or(|map| !matches!(map.event(), TraceEvent::ProgramMap { configuration_epoch: old, .. } if old == configuration_epoch)) => {
                                    self.rebuild = Some(source.clone()); self.phase = Phase::DrainAll(0);
                                }
                            TraceEvent::Discontinuity { .. } | TraceEvent::Timeline { .. } => {
                                self.rebuild = self.map.clone(); self.phase = Phase::DrainAll(0);
                            }
                            TraceEvent::Finished => { self.rebuild = None; self.phase = Phase::DrainAll(0); }
                            TraceEvent::Pes(pes) => {
                                if let Some(index) = self.lanes.as_slice().iter().position(|lane| lane.track.stream().pid == pes.pid) {
                                    let run = (pes.epochs.configuration, pes.epochs.clock, pes.byte_run_epoch);
                                    self.phase = if self.lanes.as_slice()[index].run.is_some_and(|old| old != run) {
                                        Phase::DrainLane(index)
                                    } else { Phase::Feed(index) };
                                } else { self.phase = Phase::Publish; }
                            }
                            TraceEvent::Diagnostic(fault) if matches!(fault.kind, crate::FaultKind::Pes | crate::FaultKind::Continuity | crate::FaultKind::Transport) => {
                                if let Some(index) = self.lanes.as_slice().iter().position(|lane| Some(lane.track.stream().pid) == fault.pid) {
                                    self.phase = Phase::FaultLane(index);
                                } else if fault.pid.is_none() {
                                    self.rebuild = self.map.clone(); self.phase = Phase::DrainAll(0);
                                } else { self.phase = Phase::Publish; }
                            }
                            TraceEvent::Diagnostic(fault) if fault.kind == crate::FaultKind::Program => {
                                self.rebuild = self.map.clone(); self.phase = Phase::DrainAll(0);
                            }
                            _ => self.phase = Phase::Publish,
                        }
                    }
                    Phase::DrainAll(index) => {
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
                                let lane = &self.lanes.as_slice()[index];
                                if let Some(elementary_range) = lane.unpublished() {
                                    let event = ReaderEvent::Interrupted {
                                        track: lane.track.clone(),
                                        elementary_range,
                                        cause: cause.clone(),
                                    };
                                    self.phase = Phase::DrainAll(index + 1);
                                    return Ok(Step::Ready(Event::Other(event)));
                                }
                            }
                            self.phase = Phase::DrainAll(index + 1);
                        } else {
                            if !matches!(
                                self.pending.as_ref().expect("pending boundary").event(),
                                TraceEvent::Finished
                            ) {
                                self.lanes.clear();
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
                            let epoch = match source.event() {
                                TraceEvent::ProgramMap {
                                    configuration_epoch,
                                    ..
                                } => *configuration_epoch,
                                _ => unreachable!(),
                            };
                            let track = Track {
                                source: source.clone(),
                                index,
                                epoch,
                            };
                            let lane = Lane::new(&self.pool, track, self.limits)?;
                            self.staged_tracks
                                .try_push(lane.track.clone())
                                .map_err(|e| e.error)?;
                            self.lanes.try_push(lane).map_err(|e| e.error)?;
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
                        let track = self.lanes.as_slice()[index].track.clone();
                        let interrupted =
                            self.lanes.as_slice()[index]
                                .unpublished()
                                .map(|elementary_range| ReaderEvent::Interrupted {
                                    track: track.clone(),
                                    elementary_range,
                                    cause: self.pending.as_ref().expect("pending PES").clone(),
                                });
                        let lane = Lane::new(&self.pool, track, self.limits)?;
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
                    Phase::Feed(index) => {
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
                    Phase::Publish => {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        if let Some(map) = self.rebuild.take() {
                            self.map = Some(map);
                            self.tracks = std::mem::replace(
                                &mut self.staged_tracks,
                                BudgetVec::new(&self.pool),
                            );
                        }
                        let source = self.pending.take().expect("published source");
                        if matches!(source.event(), TraceEvent::Finished) {
                            self.finished = true;
                        }
                        return Ok(Step::Ready(Event::Other(ReaderEvent::Source(source))));
                    }
                }
                continue;
            }
            if let Some((index, source, key)) = &self.queued_pes {
                let index = *index;
                if !self.lanes.as_mut_slice()[index].push(source, *key, work)? {
                    return Ok(Step::Yield);
                }
                self.queued_pes = None;
                self.active = Some(index);
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
            match stream.poll(input, work, cancel)? {
                Step::Ready(event) => self.raw = Some(event),
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(location) => {
                    stream.end_of_source()?;
                    self.end = Some(location);
                }
            }
        }
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
