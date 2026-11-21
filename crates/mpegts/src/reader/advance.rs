//! The reader's step loop: polling the transport and admitting each source event to the lanes.
use super::lane::{Lane, LaneStep, PesRun, StorageScan};
use super::model::{ClockContext, Shared};
use super::{
    AccessUnit, EntityTransition, Packet, Reader, ReaderEvent, SourceEvent, Track, Transport, seek,
};
use crate::clock::ClockKey;
use crate::compat::adts;
use crate::stream::{CarrierObservation, FaultKind, TraceEvent, TransportStream};
use crate::transport::ContinuityOutcome;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{Event, ReaderStep, TrackId};
use triergon_core::{BudgetVec, Error, Result, SourceLocation, Step, WorkBudget};

/// A transport event on its way to the caller.
#[allow(
    clippy::large_enum_variant,
    reason = "an unshared event stays inline until it is shared or returned"
)]
pub(super) enum InFlight {
    /// No event is being admitted.
    Idle,
    /// Not shared yet. Carriers and ordinary continuity events stay here until they are returned.
    Raw { event: TraceEvent, step: RawStep },
    /// Shared and being admitted to the lanes.
    Source {
        source: SourceEvent,
        step: Admission,
    },
}

/// The work unit an unshared event pays next.
#[derive(Clone, Copy)]
pub(super) enum RawStep {
    /// Share the event, or start an inline one.
    Prepare,
    /// The begin unit of an inline event.
    Begin,
    /// The publish unit of an inline event.
    Publish,
}

/// How far a shared source event has been admitted to the lanes.
pub(super) enum Admission {
    /// Choose how the event is admitted.
    Begin,
    /// Keep or interrupt each lane from `index` on before a boundary, or drain them for
    /// `Finished`; then build lanes from `rebuild`.
    DrainAll {
        index: usize,
        /// Lanes kept so far; they are moved to the front.
        retained: usize,
        rebuild: Option<SourceEvent>,
        /// A drained lane holds an unfinished tail that needs a storage audit.
        deferred_fragment: bool,
    },
    /// Build or reuse the lane of stream `index` of `map`.
    Build { index: usize, map: SourceEvent },
    /// Restart lane `index` for a new PES run, then feed it the PES.
    DrainLane(usize),
    /// Restart lane `index` after a fault on its PID.
    FaultLane(usize),
    /// Queue the PES for lane `index`.
    Feed(usize),
    /// Audit storage for the unfinished tail of lane `index`, or of a later one.
    Fragments(usize),
    /// Publish the unfinished tail of lane `index`.
    FragmentOutput(usize),
    /// Commit the event and return it; `rebuilt` is the map the lanes were built from.
    Publish { rebuilt: Option<SourceEvent> },
}

/// Lane work owed after a source event has been published.
pub(super) enum LaneWork {
    /// Nothing is owed.
    Idle,
    /// Push the published PES into `lane`.
    Push {
        lane: usize,
        source: SourceEvent,
        clock: ClockContext,
    },
    /// Report the lanes from this index on that have no framer for their stream.
    Unsupported(usize),
    /// Step this lane until it wants more payload.
    Step(usize),
}

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

/// The outcome of one admission call.
#[allow(
    clippy::large_enum_variant,
    reason = "returned by value in place, as an `Option<ReaderStep>` would be"
)]
enum Admitted {
    /// The event stays in flight.
    Kept(Flow),
    /// The publish work unit is paid; `rebuilt` is the map the lanes were built from.
    Publish { rebuilt: Option<SourceEvent> },
}

/// What one lane step produced.
#[allow(
    clippy::large_enum_variant,
    reason = "returned by value in place, as an `Option<ReaderStep>` would be"
)]
enum LaneOutput {
    /// A step for the caller.
    Ready(ReaderStep<Reader>),
    /// The lane holds an unfinished tail that waits for the storage audit.
    Deferred,
    /// The lane wants more payload or has ended.
    Done,
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
    ) -> Result<LaneOutput> {
        let delivery = self
            .seek
            .as_slice()
            .first()
            .map_or(seek::PictureDelivery::Full, |seek| {
                seek.picture_delivery(TrackId(
                    self.lanes.as_slice()[index].track.stream().pid.into(),
                ))
            });
        let ready = |event| Ok(LaneOutput::Ready(Step::Ready(Event::Other(event))));
        loop {
            match self.lanes.as_mut_slice()[index].step(draining, delivery, work, cancel)? {
                LaneStep::PictureProbe(picture) => self.observe_seek_picture(picture)?,
                LaneStep::Packet(packet) => {
                    return Ok(LaneOutput::Ready(Step::Ready(Self::output(packet))));
                }
                LaneStep::IncompleteVideo(video) => {
                    return ready(ReaderEvent::IncompleteVideo(video));
                }
                LaneStep::LatmPes(packet) => return ready(ReaderEvent::LatmPes(packet)),
                LaneStep::UnframedAudio(audio) => return ready(ReaderEvent::UnframedAudio(audio)),
                LaneStep::Yield => return Ok(LaneOutput::Ready(Step::Yield)),
                LaneStep::DeferredFragment => return Ok(LaneOutput::Deferred),
                LaneStep::End | LaneStep::NeedPayload => return Ok(LaneOutput::Done),
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
            if !matches!(self.flight, InFlight::Idle) {
                let flow = match std::mem::replace(&mut self.flight, InFlight::Idle) {
                    InFlight::Idle => Flow::Continue,
                    InFlight::Raw { event, step } => self.share_raw(event, step, work, SEEKING)?,
                    InFlight::Source { source, step } => {
                        self.admit(source, step, work, cancel, SEEKING)?
                    }
                };
                match flow {
                    Flow::Continue => continue,
                    Flow::Return(step) => return Ok(step),
                }
            }
            if !matches!(self.lane_work, LaneWork::Idle) {
                match self.run_lane_work(work, cancel)? {
                    Flow::Continue => continue,
                    Flow::Return(step) => return Ok(step),
                }
            }
            if let super::Ending::Finished(at) = self.ending {
                return Ok(Step::End(at));
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
                        return Ok(Self::inline_source(event));
                    }
                    self.flight = InFlight::Raw {
                        event,
                        step: RawStep::Prepare,
                    };
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
    /// publish work units. The event is back in flight unless it was returned.
    fn share_raw(
        &mut self,
        event: TraceEvent,
        mut step: RawStep,
        work: &mut WorkBudget,
        seeking: bool,
    ) -> Result<Flow> {
        if let RawStep::Prepare = step {
            if !work.try_charge(1) {
                self.flight = InFlight::Raw { event, step };
                return Ok(Flow::Return(Step::Yield));
            }
            if !Self::is_inline_source(&event) {
                let charge = match Shared::<TraceEvent>::reserve(&self.pool) {
                    Ok(charge) => charge,
                    Err(error) => {
                        let error = match &event {
                            // Return the permanent limit directly when a
                            // shared diagnostic would exceed the budget.
                            TraceEvent::Diagnostic(fault)
                                if matches!(
                                    error,
                                    Error::Backpressure { .. } | Error::Limit { .. }
                                ) && fault.kind == FaultKind::Pes
                                    && matches!(fault.error, Error::Limit { .. }) =>
                            {
                                fault.error
                            }
                            _ => error,
                        };
                        self.flight = InFlight::Raw { event, step };
                        return Err(error);
                    }
                };
                self.flight = InFlight::Source {
                    source: SourceEvent(Shared::new(event, charge)),
                    step: Admission::Begin,
                };
                return Ok(Flow::Continue);
            }
            step = RawStep::Begin;
        }
        // Inline source events keep the original begin and publish
        // work boundaries.
        if !work.try_charge(1) {
            self.flight = InFlight::Raw { event, step };
            return Ok(Flow::Return(Step::Yield));
        }
        if let RawStep::Begin = step {
            self.flight = InFlight::Raw {
                event,
                step: RawStep::Publish,
            };
            return Ok(Flow::Continue);
        }
        if seeking {
            return Ok(Flow::Continue);
        }
        Ok(Flow::Return(Self::inline_source(event)))
    }

    /// Admits `source` to the lanes from `step` on and publishes it. Unless it is published, the
    /// event is back in flight, also after an error.
    fn admit(
        &mut self,
        source: SourceEvent,
        mut step: Admission,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        seeking: bool,
    ) -> Result<Flow> {
        match self.admission(&source, &mut step, work, cancel) {
            Ok(Admitted::Publish { rebuilt }) => Ok(self.publish(source, rebuilt, seeking)),
            Ok(Admitted::Kept(flow)) => {
                self.flight = InFlight::Source { source, step };
                Ok(flow)
            }
            Err(error) => {
                self.flight = InFlight::Source { source, step };
                Err(error)
            }
        }
    }

    /// Runs one resumable admission phase, then queueing and the publish work unit when the
    /// phase leads to them under the same grant.
    fn admission(
        &mut self,
        source: &SourceEvent,
        step: &mut Admission,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Admitted> {
        let keep = |flow| Ok(Admitted::Kept(flow));
        let yielded = || Ok(Admitted::Kept(Flow::Return(Step::Yield)));
        let ready = |event| {
            Ok(Admitted::Kept(Flow::Return(Step::Ready(Event::Other(
                event,
            )))))
        };
        match step {
            Admission::Begin => {
                if !work.try_charge(1) {
                    return yielded();
                }
                let (next, interrupted) = self.begin(source);
                *step = next;
                if let Some(event) = interrupted {
                    return ready(event);
                }
            }
            Admission::DrainAll {
                index,
                retained,
                rebuild,
                deferred_fragment,
            } => {
                if *index < self.lanes.len() {
                    if matches!(source.event(), TraceEvent::Finished) {
                        match self.lane_step(*index, true, work, cancel)? {
                            LaneOutput::Ready(step) => return keep(Flow::Return(step)),
                            LaneOutput::Deferred => *deferred_fragment = true,
                            LaneOutput::Done => (),
                        }
                    } else {
                        if !work.try_charge(1) {
                            return yielded();
                        }
                        if let Some(event) = self.keep_or_interrupt(*index, retained, source) {
                            *index += 1;
                            return ready(event);
                        }
                    }
                    *index += 1;
                    return keep(Flow::Continue);
                }
                if matches!(source.event(), TraceEvent::Finished) {
                    if *deferred_fragment {
                        // No transport or framer can produce more bytes.
                        // Release opaque parser storage before auditing
                        // the remaining owners for fragment publication.
                        self.release_transport();
                        self.boundary = None;
                        *step = Admission::Fragments(0);
                        return keep(Flow::Continue);
                    }
                } else {
                    self.boundary = Some(source.clone());
                    self.lanes.truncate(*retained);
                    self.staged_tracks.clear();
                }
                *step = match rebuild.take() {
                    Some(map) => Admission::Build { index: 0, map },
                    None => Admission::Publish { rebuilt: None },
                };
            }
            Admission::Build { index, map } => {
                if *index < map.map().streams.len() {
                    if !work.try_charge(1) {
                        return yielded();
                    }
                    self.build_lane(*index, map)?;
                    *index += 1;
                } else {
                    self.lane_work = LaneWork::Unsupported(0);
                    *step = match std::mem::replace(step, Admission::Begin) {
                        Admission::Build { map, .. } => Admission::Publish { rebuilt: Some(map) },
                        other => other,
                    };
                }
            }
            Admission::DrainLane(index) | Admission::FaultLane(index) => {
                let index = *index;
                let fault = matches!(step, Admission::FaultLane(_));
                if !work.try_charge(1) {
                    return yielded();
                }
                let interrupted = self.restart_lane(index, source)?;
                *step = if fault {
                    Admission::Publish { rebuilt: None }
                } else {
                    Admission::Feed(index)
                };
                if let Some(event) = interrupted {
                    return ready(event);
                }
            }
            Admission::Fragments(index) => {
                let index = *index;
                if index == self.lanes.len() {
                    *step = Admission::Publish { rebuilt: None };
                } else if self.lanes.as_slice()[index].has_incomplete() {
                    if !self.audit_fragment(index, source, work, cancel)? {
                        return yielded();
                    }
                    *step = Admission::FragmentOutput(index);
                } else {
                    if !work.try_charge(1) {
                        return yielded();
                    }
                    *step = Admission::Fragments(index + 1);
                }
            }
            Admission::FragmentOutput(index) => {
                if let LaneOutput::Ready(output) = self.lane_step(*index, true, work, cancel)? {
                    return keep(Flow::Return(output));
                }
                *step = Admission::Fragments(*index + 1);
            }
            Admission::Feed(_) | Admission::Publish { .. } => (),
        }
        // Source admission can advance through queueing and publication
        // under the same grant. Each phase remains resumable.
        if let Admission::Feed(index) = *step {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return yielded();
            }
            self.feed(index, source)?;
            *step = Admission::Publish { rebuilt: None };
        }
        if let Admission::Publish { rebuilt } = step {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return yielded();
            }
            return Ok(Admitted::Publish {
                rebuilt: rebuilt.take(),
            });
        }
        keep(Flow::Continue)
    }

    /// Chooses how `source` is admitted, and returns the PES it interrupts when its lane's run
    /// began before a boundary.
    fn begin(&mut self, source: &SourceEvent) -> (Admission, Option<ReaderEvent>) {
        let drain = |rebuild| Admission::DrainAll {
            index: 0,
            retained: 0,
            rebuild,
            deferred_fragment: false,
        };
        let publish = Admission::Publish { rebuilt: None };
        let lane_of = |reader: &Self, pid: u16| {
            reader
                .lanes
                .as_slice()
                .iter()
                .position(|lane| lane.track.stream().pid == pid)
        };
        let next = match source.event() {
            TraceEvent::Pat(table) => {
                self.tsid = Some(table.header.extension);
                if self.map.is_some() && !self.stream().is_some_and(TransportStream::has_active_map)
                {
                    drain(None)
                } else {
                    publish
                }
            }
            TraceEvent::ProgramMap {
                active: true,
                configuration_epoch,
                ..
            } if self.map.as_ref().is_none_or(|map| {
                !matches!(map.event(), TraceEvent::ProgramMap { configuration_epoch: old, .. }
                    if old == configuration_epoch)
            }) =>
            {
                drain(Some(source.clone()))
            }
            TraceEvent::Discontinuity { .. } | TraceEvent::Finished => drain(None),
            TraceEvent::Timeline { .. } => drain(self.map.clone()),
            TraceEvent::Continuity {
                pid,
                outcome: ContinuityOutcome::SignalledDiscontinuity,
            } => lane_of(self, *pid).map_or(publish, Admission::FaultLane),
            TraceEvent::Pes(pes) | TraceEvent::UnboundedAdtsPes(adts::UnboundedPes(pes)) => {
                let Some(index) = lane_of(self, pes.pid) else {
                    return (publish, None);
                };
                let lane = &mut self.lanes.as_mut_slice()[index];
                if let PesRun::BeforeBoundary(start) = lane.run {
                    lane.run = PesRun::Idle;
                    if let Some(boundary) = &self.boundary
                        && pes
                            .header_source_ranges
                            .as_slice()
                            .first()
                            .is_some_and(|range| {
                                range.entity == start.source && range.offset == start.offset
                            })
                    {
                        let event = ReaderEvent::InterruptedPes {
                            source: source.clone(),
                            boundary: boundary.clone(),
                        };
                        return (publish, Some(event));
                    }
                }
                match lane.run {
                    PesRun::Active(epochs, byte_run)
                        if epochs != pes.epochs || byte_run != pes.byte_run_epoch =>
                    {
                        Admission::DrainLane(index)
                    }
                    _ => Admission::Feed(index),
                }
            }
            TraceEvent::Diagnostic(fault)
                if matches!(
                    fault.kind,
                    FaultKind::Pes | FaultKind::Continuity | FaultKind::Transport
                ) =>
            {
                if let Some(index) = fault.pid.and_then(|pid| lane_of(self, pid)) {
                    Admission::FaultLane(index)
                } else if fault.pid.is_none() {
                    drain(self.map.clone())
                } else {
                    publish
                }
            }
            TraceEvent::Diagnostic(fault) if fault.kind == FaultKind::Program => {
                drain(self.map.clone())
            }
            _ => publish,
        };
        (next, None)
    }

    /// Before a boundary, moves lane `index` to the kept prefix or reports its unpublished bytes
    /// as interrupted by `cause`.
    fn keep_or_interrupt(
        &mut self,
        index: usize,
        retained: &mut usize,
        cause: &SourceEvent,
    ) -> Option<ReaderEvent> {
        if self.keeps_lane(index, cause) {
            self.lanes.as_mut_slice().swap(index, *retained);
            *retained += 1;
            return None;
        }
        let lane = &self.lanes.as_slice()[index];
        lane.unpublished()
            .map(|elementary_range| ReaderEvent::Interrupted {
                track: lane.track.clone(),
                elementary_range,
                cause: cause.clone(),
            })
    }

    /// Builds or reuses the lane of stream `index` of the new program map.
    #[inline(never)]
    fn build_lane(&mut self, index: usize, source: &SourceEvent) -> Result<()> {
        let map = source.map();
        self.lanes
            .try_reserve_exact(map.streams.len() - self.lanes.len())?;
        self.staged_tracks
            .try_reserve_exact(map.streams.len() - self.staged_tracks.len())?;
        // Remember the exact open PES in a reused route.
        // Physical offsets can restart in every entity.
        let stream = &map.streams.as_slice()[index];
        let (epochs, pending_header) = self
            .stream()
            .and_then(|transport| transport.route_state(index, stream.pid))
            .ok_or(Error::Invalid("MPEG-TS track route missing"))?;
        let unlabelled_dts_core = self.limits.unlabelled_dts_core.is_some_and(|selection| {
            selection.program == map.header.extension
                && selection.pid == stream.pid
                && stream.stream_type == crate::psi::PES_PRIVATE_DATA
                && stream.descriptor_bytes.is_empty()
                && map.program_descriptors.is_empty()
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
        Ok(())
    }

    /// Replaces lane `index` with a fresh one, and returns its unpublished bytes as interrupted
    /// by `cause`.
    #[inline(never)]
    fn restart_lane(&mut self, index: usize, cause: &SourceEvent) -> Result<Option<ReaderEvent>> {
        let previous = &self.lanes.as_slice()[index];
        let track = previous.track.clone();
        let interrupted = previous
            .unpublished()
            .map(|elementary_range| ReaderEvent::Interrupted {
                track: track.clone(),
                elementary_range,
                cause: cause.clone(),
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
        Ok(interrupted)
    }

    /// Audits the storage the unfinished tail of lane `index` may use; `false` when the audit
    /// yields.
    #[inline(never)]
    fn audit_fragment(
        &mut self,
        index: usize,
        finished: &SourceEvent,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let mut storage = match self.storage.take() {
            Some(storage) => storage,
            None => StorageScan::new(self, finished)?,
        };
        if !storage.step(self, work, cancel)? {
            self.storage = Some(storage);
            return Ok(false);
        }
        self.lanes.as_mut_slice()[index].set_reader_storage(storage.bytes());
        Ok(true)
    }

    /// Queues the PES of `source` for lane `index` with its clock.
    fn feed(&mut self, index: usize, source: &SourceEvent) -> Result<()> {
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
        self.lane_work = LaneWork::Push {
            lane: index,
            source: source.clone(),
            clock,
        };
        Ok(())
    }

    /// Commits the effects of `source` and returns it, unless a seek replay withholds it.
    fn publish(
        &mut self,
        source: SourceEvent,
        rebuilt: Option<SourceEvent>,
        seeking: bool,
    ) -> Flow {
        if let Some(map) = rebuilt {
            self.map = Some(map);
            self.tracks = std::mem::replace(&mut self.staged_tracks, BudgetVec::new(&self.pool));
        }
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
            TraceEvent::Finished => {
                if let super::Ending::Ended(at) = self.ending {
                    self.ending = super::Ending::Finished(at);
                }
            }
            _ => (),
        }
        if seeking && !self.seek.as_slice()[0].publishes_source(source.event()) {
            return Flow::Continue;
        }
        Flow::Return(Step::Ready(Event::Other(ReaderEvent::Source(source))))
    }

    /// Pushes a queued PES into its lane, reports unsupported lanes after a rebuild, or steps the
    /// active lane. The work stays owed unless it is done, also after an error.
    fn run_lane_work(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Flow> {
        match std::mem::replace(&mut self.lane_work, LaneWork::Idle) {
            LaneWork::Idle => Ok(Flow::Continue),
            LaneWork::Push {
                lane,
                source,
                clock,
            } => match self.lanes.as_mut_slice()[lane].push(source, clock, work) {
                Ok(None) => {
                    self.lane_work = LaneWork::Step(lane);
                    Ok(Flow::Continue)
                }
                Ok(Some(source)) => {
                    self.lane_work = LaneWork::Push {
                        lane,
                        source,
                        clock,
                    };
                    Ok(Flow::Return(Step::Yield))
                }
                Err(refused) => {
                    self.lane_work = LaneWork::Push {
                        lane,
                        source: refused.value,
                        clock,
                    };
                    Err(refused.error)
                }
            },
            LaneWork::Unsupported(index) => {
                if !work.try_charge(1) {
                    self.lane_work = LaneWork::Unsupported(index);
                    return Ok(Flow::Return(Step::Yield));
                }
                if index + 1 < self.lanes.len() {
                    self.lane_work = LaneWork::Unsupported(index + 1);
                }
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
                Ok(Flow::Continue)
            }
            LaneWork::Step(lane) => match self.lane_step(lane, false, work, cancel) {
                Ok(LaneOutput::Ready(step)) => {
                    self.lane_work = LaneWork::Step(lane);
                    Ok(Flow::Return(step))
                }
                Ok(LaneOutput::Deferred | LaneOutput::Done) => Ok(Flow::Continue),
                Err(error) => {
                    self.lane_work = LaneWork::Step(lane);
                    Err(error)
                }
            },
        }
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
        if let super::Ending::Continues(next) = self.ending {
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
            self.ending = super::Ending::Open;
        } else {
            stream.end_of_source()?;
            self.ending = super::Ending::Ended(location);
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
