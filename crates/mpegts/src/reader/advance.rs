//! The reader's step loop: polling the transport and admitting each source event to the lanes.
use super::lane::{Lane, LaneStep, PesRun, StorageScan};
use super::model::{ClockContext, Shared};
use super::{
    AccessUnit, EntityTransition, Packet, Reader, ReaderEvent, SourceEvent, Track, Transport, seek,
};
use crate::clock::ClockKey;
use crate::compat::adts;
use crate::stream::{CarrierObservation, TraceEvent, TransportStream};
use crate::transport::ContinuityOutcome;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{Event, ReaderStep, TrackId};
use triergon_core::{BudgetVec, Error, Result, SourceLocation, Step, WorkBudget};

/// What the step loop does next.
#[allow(
    clippy::large_enum_variant,
    reason = "returned by value in place, as an `Option<ReaderStep>` would be"
)]
enum Flow {
    /// Go round the loop again.
    Continue,
    /// Return this step to the caller.
    Return(ReaderStep<Reader>),
}

#[derive(Clone, Copy)]
pub(super) enum Phase {
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
            .and_then(|position| self.stream()?.route_state(position, stream.pid))
            .is_some_and(|(epochs, _)| {
                epochs.configuration == lane.track.epoch
                    && !matches!(lane.run, PesRun::Active(old, _) if old != epochs)
                    && !matches!(lane.run, PesRun::BeforeBoundary(_))
            })
    }

    pub(super) fn step_observing<const OBSERVING: bool>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        mut observe: impl FnMut(CarrierObservation<'_>),
    ) -> Result<ReaderStep<Self>> {
        self.healthy()?;
        let result = loop {
            let result = if self.seek.is_empty() {
                self.advance::<OBSERVING, false>(work, cancel, &mut observe)
            } else {
                self.advance_seek(work, cancel)
            };
            if matches!(
                &result,
                Ok(Step::Backpressure(_)) | Err(Error::Backpressure { .. })
            ) && self.seek_index.take().is_some()
            {
                self.storage = None;
                continue;
            }
            break result;
        };
        result.or_else(|error| {
            error.into_step().inspect_err(|&error| {
                if error != Error::Allocation {
                    self.stop(error);
                }
            })
        })
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
        let delivery = self
            .seek
            .as_slice()
            .first()
            .map_or(seek::PictureDelivery::Full, |seek| {
                seek.picture_delivery(TrackId(
                    self.lanes.as_slice()[index].track.stream().pid.into(),
                ))
            });
        loop {
            match self.lanes.as_mut_slice()[index].step(draining, delivery, work, cancel)? {
                LaneStep::PictureProbe(picture) => self.observe_seek_picture(picture)?,
                LaneStep::Packet(packet) => return Ok(Some(Step::Ready(Self::output(packet)))),
                LaneStep::IncompleteVideo(video) => {
                    return Ok(Some(Step::Ready(Event::Other(
                        ReaderEvent::IncompleteVideo(video),
                    ))));
                }
                LaneStep::LatmPes(packet) => {
                    return Ok(Some(Step::Ready(Event::Other(ReaderEvent::LatmPes(
                        packet,
                    )))));
                }
                LaneStep::UnframedAudio(audio) => {
                    return Ok(Some(Step::Ready(Event::Other(ReaderEvent::UnframedAudio(
                        audio,
                    )))));
                }
                LaneStep::Yield => return Ok(Some(Step::Yield)),
                LaneStep::DeferredFragment => {
                    self.deferred_fragment = true;
                    return Ok(None);
                }
                LaneStep::End | LaneStep::NeedPayload => return Ok(None),
            }
        }
    }
    // One loop per observer, kept out of line so each carrier event is built in the caller's
    // return slot. Only the transport poll and the inline fast return depend on the observer;
    // admission and lane work are compiled once.
    #[inline(never)]
    pub(super) fn advance<const OBSERVING: bool, const SEEKING: bool>(
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
            if self.raw.is_some() {
                match self.share_raw(work, SEEKING)? {
                    Flow::Continue => continue,
                    Flow::Return(step) => return Ok(step),
                }
            }
            if self.pending.is_some() {
                match self.admit(work, cancel, SEEKING)? {
                    Flow::Continue => continue,
                    Flow::Return(step) => return Ok(step),
                }
            }
            if self.queued_pes.is_some() || self.unsupported_at.is_some() || self.active.is_some() {
                match self.run_lane_work(work, cancel)? {
                    Flow::Continue => continue,
                    Flow::Return(step) => return Ok(step),
                }
            }
            if self.finished {
                return Ok(Step::End(
                    self.end
                        .ok_or(Error::Invalid("MPEG-TS missing source end"))?,
                ));
            }
            let Transport::Open { input, stream } = &mut self.transport else {
                return Err(Error::Invalid("MPEG-TS reader closed"));
            };
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
                Step::NeedInput(request) => {
                    if request.maximum == 0
                        && self
                            .seek_entities
                            .as_deref()
                            .and_then(|plan| plan.next())
                            .is_some()
                    {
                        self.finish(request.source, request.token)?;
                        continue;
                    }
                    return Ok(Step::NeedInput(request));
                }
                Step::Yield => return Ok(Step::Yield),
                Step::End(location) => {
                    if let Flow::Return(step) = self.end_entity(location, work, cancel)? {
                        return Ok(step);
                    }
                }
            }
        }
    }

    /// Shares a transport event that needs storage, or moves an inline one through its begin and
    /// publish work units.
    fn share_raw(&mut self, work: &mut WorkBudget, seeking: bool) -> Result<Flow> {
        let Some(raw) = &self.raw else {
            return Ok(Flow::Continue);
        };
        if matches!(self.phase, Phase::Prepare) {
            if !work.try_charge(1) {
                return Ok(Flow::Return(Step::Yield));
            }
            if !Self::is_inline_source(raw) {
                let charge = Shared::<TraceEvent>::reserve(&self.pool).map_err(|error| {
                    if matches!(error, Error::Backpressure { .. } | Error::Limit { .. })
                        && let TraceEvent::Diagnostic(fault) = raw
                        && fault.kind == crate::stream::FaultKind::Pes
                        && matches!(fault.error, Error::Limit { .. })
                    {
                        // Return the permanent limit directly when a
                        // shared diagnostic would exceed the budget.
                        fault.error
                    } else {
                        error
                    }
                })?;
                let event = self.raw.take().expect("raw source");
                self.pending = Some(SourceEvent(Shared::new(event, charge)));
            }
            self.phase = Phase::Begin;
        }
        if self.raw.is_some() {
            // Inline source events keep the original begin and publish
            // work boundaries.
            if !work.try_charge(1) {
                return Ok(Flow::Return(Step::Yield));
            }
            if matches!(self.phase, Phase::Begin) {
                self.phase = Phase::Publish;
                return Ok(Flow::Continue);
            }
            let raw = self.raw.take().expect("inline source");
            if seeking {
                return Ok(Flow::Continue);
            }
            return Ok(Flow::Return(Self::inline_source(raw)));
        }
        Ok(Flow::Continue)
    }

    /// Admits the pending source event to the lanes, one resumable phase after another, and
    /// publishes it.
    fn admit(&mut self, work: &mut WorkBudget, cancel: &AtomicBool, seeking: bool) -> Result<Flow> {
        match self.phase {
            Phase::Prepare => unreachable!("pending source is prepared"),
            Phase::Begin => {
                if let Some(step) = self.begin(work)? {
                    return Ok(Flow::Return(step));
                }
            }
            Phase::DrainAll { index, retained } => {
                if let Flow::Return(step) = self.drain_all(index, retained, work, cancel)? {
                    return Ok(Flow::Return(step));
                }
                if matches!(self.phase, Phase::Fragments(_)) {
                    return Ok(Flow::Continue);
                }
            }
            Phase::Build(index) => {
                if let Some(step) = self.build_lane(index, work)? {
                    return Ok(Flow::Return(step));
                }
            }
            Phase::DrainLane(index) | Phase::FaultLane(index) => {
                if let Some(step) = self.restart_lane(index, work)? {
                    return Ok(Flow::Return(step));
                }
            }
            Phase::Fragments(index) => {
                if let Some(step) = self.fragments(index, work, cancel)? {
                    return Ok(Flow::Return(step));
                }
            }
            Phase::FragmentOutput(index) => {
                if let Some(step) = self.lane_step(index, true, work, cancel)? {
                    return Ok(Flow::Return(step));
                }
                self.phase = Phase::Fragments(index + 1);
            }
            Phase::Feed(_) | Phase::Publish => {}
        }
        // Source admission can advance through queueing and publication
        // under the same grant. Each phase remains resumable.
        if let Phase::Feed(index) = self.phase
            && let Some(step) = self.feed(index, work, cancel)?
        {
            return Ok(Flow::Return(step));
        }
        if matches!(self.phase, Phase::Publish) {
            return self.publish(work, cancel, seeking);
        }
        Ok(Flow::Continue)
    }

    /// Chooses how the pending source event is admitted.
    fn begin(&mut self, work: &mut WorkBudget) -> Result<Option<ReaderStep<Self>>> {
        if !work.try_charge(1) {
            return Ok(Some(Step::Yield));
        }
        let source = self.pending.as_ref().expect("pending source");
        match source.event() {
            TraceEvent::Pat(table) => {
                self.tsid = Some(table.header.extension);
                self.phase = if self.map.is_some()
                    && !self.stream().is_some_and(TransportStream::has_active_map)
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
                            return Ok(Some(Step::Ready(Event::Other(event))));
                        }
                    }
                    self.phase = match lane.run {
                        PesRun::Active(epochs, byte_run)
                            if epochs != pes.epochs || byte_run != pes.byte_run_epoch => Phase::DrainLane(index),
                        _ => Phase::Feed(index),
                    };
                } else { self.phase = Phase::Publish; }
            }
            TraceEvent::Diagnostic(fault) if matches!(fault.kind, crate::stream::FaultKind::Pes | crate::stream::FaultKind::Continuity | crate::stream::FaultKind::Transport) => {
                if let Some(index) = self.lanes.as_slice().iter().position(|lane| Some(lane.track.stream().pid) == fault.pid) {
                    self.phase = Phase::FaultLane(index);
                } else if fault.pid.is_none() {
                    self.rebuild = self.map.clone();
                    self.phase = Phase::DrainAll { index: 0, retained: 0 };
                } else { self.phase = Phase::Publish; }
            }
            TraceEvent::Diagnostic(fault) if fault.kind == crate::stream::FaultKind::Program => {
                self.rebuild = self.map.clone();
                self.phase = Phase::DrainAll { index: 0, retained: 0 };
            }
            _ => self.phase = Phase::Publish,
        }
        Ok(None)
    }

    /// Keeps or interrupts every lane before a boundary; on `Finished`, drains them instead.
    #[inline(never)]
    fn drain_all(
        &mut self,
        index: usize,
        mut retained: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Flow> {
        if index < self.lanes.len() {
            let cause = self.pending.as_ref().expect("pending boundary");
            if matches!(cause.event(), TraceEvent::Finished) {
                if let Some(step) = self.lane_step(index, true, work, cancel)? {
                    return Ok(Flow::Return(step));
                }
            } else {
                if !work.try_charge(1) {
                    return Ok(Flow::Return(Step::Yield));
                }
                if self.keeps_lane(index, cause) {
                    self.lanes.as_mut_slice().swap(index, retained);
                    retained += 1;
                } else if let Some(elementary_range) = self.lanes.as_slice()[index].unpublished() {
                    let event = ReaderEvent::Interrupted {
                        track: self.lanes.as_slice()[index].track.clone(),
                        elementary_range,
                        cause: cause.clone(),
                    };
                    self.phase = Phase::DrainAll {
                        index: index + 1,
                        retained,
                    };
                    return Ok(Flow::Return(Step::Ready(Event::Other(event))));
                }
            }
            self.phase = Phase::DrainAll {
                index: index + 1,
                retained,
            };
            return Ok(Flow::Continue);
        }
        if self.deferred_fragment
            && matches!(
                self.pending.as_ref().expect("pending boundary").event(),
                TraceEvent::Finished
            )
        {
            // No transport or framer can produce more bytes.
            // Release opaque parser storage before auditing
            // the remaining owners for fragment publication.
            self.release_transport();
            self.boundary = None;
            self.active = None;
            self.phase = Phase::Fragments(0);
            return Ok(Flow::Continue);
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
        Ok(Flow::Continue)
    }

    /// Builds or reuses the lane of stream `index` of the new program map.
    #[inline(never)]
    fn build_lane(
        &mut self,
        index: usize,
        work: &mut WorkBudget,
    ) -> Result<Option<ReaderStep<Self>>> {
        let source = self.rebuild.as_ref().expect("pending map");
        if index < source.map().streams.len() {
            if !work.try_charge(1) {
                return Ok(Some(Step::Yield));
            }
            self.lanes
                .try_reserve_exact(source.map().streams.len() - self.lanes.len())?;
            self.staged_tracks
                .try_reserve_exact(source.map().streams.len() - self.staged_tracks.len())?;
            // Remember the exact open PES in a reused route.
            // Physical offsets can restart in every entity.
            let (epochs, pending_header) = self
                .stream()
                .and_then(|stream| {
                    stream.route_state(index, source.map().streams.as_slice()[index].pid)
                })
                .ok_or(Error::Invalid("MPEG-TS track route missing"))?;
            let stream = &source.map().streams.as_slice()[index];
            let unlabelled_dts_core = self.limits.unlabelled_dts_core.is_some_and(|selection| {
                selection.program == source.map().header.extension
                    && selection.pid == stream.pid
                    && stream.stream_type == crate::psi::PES_PRIVATE_DATA
                    && stream.descriptor_bytes.is_empty()
                    && source.map().program_descriptors.is_empty()
            });
            let track = Track {
                source: source.clone(),
                // A PMT cannot have more entries than the 13-bit PID space.
                index: u16::try_from(index).map_err(|_| Error::Overflow)?,
                epoch: epochs.configuration,
                unlabelled_dts_core,
            };
            let retained = self.lanes.as_slice()[index..]
                .iter()
                .position(|lane| lane.track.stream().pid == track.stream().pid);
            let position = if let Some(retained) = retained {
                let position = index + retained;
                self.lanes.as_mut_slice()[position].track = track.clone();
                position
            } else {
                let frame = self.frames_track(track.stream().pid);
                let mut lane = Lane::new(&self.pool, track.clone(), self.limits, frame)?;
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
        Ok(None)
    }

    /// Replaces lane `index` with a fresh one after a PES run change or a fault on its PID.
    #[inline(never)]
    fn restart_lane(
        &mut self,
        index: usize,
        work: &mut WorkBudget,
    ) -> Result<Option<ReaderStep<Self>>> {
        let fault = matches!(self.phase, Phase::FaultLane(_));
        if !work.try_charge(1) {
            return Ok(Some(Step::Yield));
        }
        let previous = &self.lanes.as_slice()[index];
        let track = previous.track.clone();
        let interrupted = previous
            .unpublished()
            .map(|elementary_range| ReaderEvent::Interrupted {
                track: track.clone(),
                elementary_range,
                cause: self.pending.as_ref().expect("pending PES").clone(),
            });
        let frame = self.frames_track(track.stream().pid);
        let mut lane = Lane::new(&self.pool, track, self.limits, frame)?;
        // A clock fault can restart framing while the transport
        // still holds the PES that began before the program change.
        if let PesRun::BeforeBoundary(start) = previous.run
            && self
                .stream()
                .and_then(|stream| stream.route_state(index, lane.track.stream().pid))
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
        Ok(interrupted.map(|event| Step::Ready(Event::Other(event))))
    }

    /// Audits storage for and copies the unfinished tail of lane `index` after `Finished`.
    #[inline(never)]
    fn fragments(
        &mut self,
        index: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<ReaderStep<Self>>> {
        if index == self.lanes.len() {
            self.phase = Phase::Publish;
        } else if self.lanes.as_slice()[index].has_incomplete() {
            let mut storage = match self.storage.take() {
                Some(storage) => storage,
                None => StorageScan::new(self)?,
            };
            if !storage.step(self, work, cancel)? {
                self.storage = Some(storage);
                return Ok(Some(Step::Yield));
            }
            self.lanes.as_mut_slice()[index].set_reader_storage(storage.bytes());
            self.phase = Phase::FragmentOutput(index);
        } else {
            if !work.try_charge(1) {
                return Ok(Some(Step::Yield));
            }
            self.phase = Phase::Fragments(index + 1);
        }
        Ok(None)
    }

    /// Queues the pending PES for lane `index` with its clock.
    fn feed(
        &mut self,
        index: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<ReaderStep<Self>>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(Some(Step::Yield));
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
                .stream()
                .is_some_and(TransportStream::clock_is_quarantined),
        };
        self.queued_pes = Some((index, source.clone(), clock));
        self.phase = Phase::Publish;
        Ok(None)
    }

    /// Commits the pending source event's effects and returns it.
    fn publish(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        seeking: bool,
    ) -> Result<Flow> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(Flow::Return(Step::Yield));
        }
        if let Some(map) = self.rebuild.take() {
            self.map = Some(map);
            self.tracks = std::mem::replace(&mut self.staged_tracks, BudgetVec::new(&self.pool));
        }
        let source = self.pending.take().expect("published source");
        match source.event() {
            TraceEvent::Pat(_) if !self.stream().is_some_and(TransportStream::has_active_map) => {
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
        if seeking && !self.seek.as_slice()[0].publishes_source(source.event()) {
            return Ok(Flow::Continue);
        }
        Ok(Flow::Return(Step::Ready(Event::Other(
            ReaderEvent::Source(source),
        ))))
    }

    /// Pushes a queued PES into its lane, reports unsupported lanes after a rebuild, or steps the
    /// active lane.
    fn run_lane_work(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Flow> {
        if let Some((index, source, key)) = self.queued_pes.take() {
            let mut source = Some(source);
            match self.lanes.as_mut_slice()[index].push(&mut source, key, work) {
                Ok(true) => self.active = Some(index),
                retry => {
                    self.queued_pes = Some((index, source.expect("queued PES on retry"), key));
                    retry?;
                    return Ok(Flow::Return(Step::Yield));
                }
            }
            return Ok(Flow::Continue);
        }
        if let Some(index) = self.unsupported_at {
            if !work.try_charge(1) {
                return Ok(Flow::Return(Step::Yield));
            }
            self.unsupported_at = (index + 1 < self.lanes.len()).then_some(index + 1);
            if let Some(lane) = self.lanes.as_slice().get(index)
                && let Some(error) = lane.unsupported
            {
                return Ok(Flow::Return(Step::Ready(Event::Other(
                    ReaderEvent::Unsupported {
                        track: lane.track.clone(),
                        error,
                    },
                ))));
            }
            return Ok(Flow::Continue);
        }
        if let Some(index) = self.active {
            if let Some(step) = self.lane_step(index, false, work, cancel)? {
                return Ok(Flow::Return(step));
            }
            self.active = None;
        }
        Ok(Flow::Continue)
    }

    /// Continues with the queued next entity, or records the logical end of the source.
    #[inline(never)]
    fn end_entity(
        &mut self,
        location: SourceLocation,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Flow> {
        let Transport::Open { input, stream } = &mut self.transport else {
            return Err(Error::Invalid("MPEG-TS reader closed"));
        };
        if let Some(next) = self.next_entity {
            let Some(end) = stream.media_entity_end(input, work, cancel)? else {
                return Ok(Flow::Return(Step::Yield));
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
        Ok(Flow::Continue)
    }

    #[cold]
    fn audit_pes_growth(
        &mut self,
        error: Error,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<TraceEvent>> {
        let floor = match self.stream() {
            Some(stream) => stream.blocked_pes_growth_floor()?,
            None => None,
        };
        let Some(floor) = floor else {
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
            let Transport::Open { stream, .. } = &mut self.transport else {
                return Err(error);
            };
            return Ok(Step::Ready(stream.refuse_blocked_pes_growth(limit)));
        }
        self.storage = Some(scan);
        Err(error)
    }
}
