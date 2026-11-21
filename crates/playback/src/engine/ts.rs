//! The transport stream engine: source events with their clocks, and the demux
//! PES of selected or discovered tracks.

pub(crate) mod feed;
pub(crate) mod tracks;

use crate::compat::pes::{AdmittedCarriage, Carriage, PidSelection};
use crate::engine::ts::feed::{FeedPacket, FeedStep, TsStepProjection, map_payload};
use crate::engine::ts::tracks::{PesTrack, PesTracks, TrackUpdate};
use crate::engine::{TrackFact, track_kind};
use crate::event::TsEvent;
use crate::source::SourceReply;
use crate::{Result, SeekIntent, TrackKey};
use std::sync::atomic::AtomicBool;
use triergon_core::{BudgetVec, Error, InputCursor, Step, WorkBudget};
use triergon_mpegts::stream::{FeedContinuation, FeedItem, TraceEvent, TransportStream};
use triergon_mpegts::{
    self as ts,
    clock::{ClockKey, SourceTimePolicy, source_timestamp},
};
use ts::compat::pes::{DemuxClock, ReadyContinuation, SourceContext, selected_track_kind};
use ts::psi::DescriptorNamespace;
/// What a TS engine delivers and where its tracks come from. One byte: the
/// discriminants are the bit patterns the same state had as an
/// `Option` of the five discovering modes, so every test compiles as before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Delivery {
    /// Source events, plus demux PES of the tracks found in the segment's own PMT.
    Segment = 0,
    /// Demux PES only, owned payloads, to a progressive reader.
    Progressive = 1,
    /// Demux PES only, compact payloads, to a progressive reader.
    CompactProgressive = 2,
    /// Demux PES only, to an AVC and AAC segment reader.
    AvcAacFeed = 3,
    /// Demux PES only, to a media segment reader (HLS audio carriage rules).
    MediaFeed = 4,
    /// Source events of a declared program, plus demux PES of the selected PIDs.
    Program = 5,
}

impl Delivery {
    /// Whether the tracks come from the stream's own PMT.
    #[inline]
    pub(crate) const fn discovers_tracks(self) -> bool {
        !matches!(self, Self::Program)
    }

    /// Whether a progressive reader takes the PES.
    #[inline]
    pub(crate) const fn is_progressive(self) -> bool {
        matches!(self, Self::Progressive | Self::CompactProgressive)
    }

    /// Whether only the PES feed is delivered, not the source events.
    #[inline]
    pub(crate) const fn is_feed(self) -> bool {
        matches!(
            self,
            Self::Progressive | Self::CompactProgressive | Self::AvcAacFeed | Self::MediaFeed
        )
    }
}

/// The transport stream demuxer of one program: the engine of
/// [`Engine::Ts`](crate::Engine::Ts) and of the PES feed.
///
/// `bound_context` with `grid_lost`, `ended` with `draining`, and
/// `entity_sequence` with `entity_end` are each one state in two fields on
/// purpose: `poll` tests them per carrier, and one enum for each pair cost 0.04
/// to 0.21% of the instructions of a program read.
#[derive(Debug)]
pub struct TsEngine {
    /// The startup context the observers are bound to, if any.
    pub(crate) bound_context: Option<ts::compat::input::InputContext>,
    /// Whether a bound M2TS prefix input lost its carrier grid, after which every
    /// selected track is refused.
    pub(crate) grid_lost: bool,
    pub(crate) input: InputCursor,
    pub(crate) stream: TransportStream,
    pub(crate) format: ts::PacketFormat,
    pub(crate) program: u16,
    pub(crate) limits: ts::ParseLimits,
    pub(crate) policy: SourceTimePolicy,
    pub(crate) namespace: DescriptorNamespace,
    pub(crate) source_pes_clock: Option<DemuxClock>,
    pub(crate) tsid: Option<u16>,
    pub(crate) pcr_pid: Option<u16>,
    pub(crate) facts: BudgetVec<TrackFact>,
    pub(crate) pending: Option<TraceEvent>,
    /// Whether the stream reported its end. Only after `draining`: the stream
    /// reports its end only after the source's end started the drain.
    pub(crate) ended: bool,
    /// Whether the source ended and the stream and the tracks drain.
    pub(crate) draining: bool,
    pub(crate) pes_tracks: PesTracks,
    pub(crate) pes_active: Option<usize>,
    pub(crate) pes_adopting: usize,
    pub(crate) pes_pending: Option<FeedPacket>,
    pub(crate) pes_clock: Option<DemuxClock>,
    pub(crate) event_ordinal: u64,
    pub(crate) suppressed_events: u64,
    pub(crate) pes_updates_from: Option<usize>,
    pub(crate) delivery: Delivery,
    /// Whether the source is a sequence of entities, each of whose ends is reported.
    pub(crate) entity_sequence: bool,
    /// The reported end of the current entity, until the caller continues with
    /// the next entity or finishes the logical source.
    pub(crate) entity_end: Option<ts::stream::MediaEntityEnd>,
}

impl TsEngine {
    /// Starts replaying the stream from its start with fresh tables, clocks and observers.
    pub(crate) fn begin_seek(&mut self, intent: &SeekIntent) -> Result<()> {
        if self.delivery.discovers_tracks() {
            return Err(Error::Unsupported("seek needs a fresh TS segment"));
        }
        if self.bound_context.is_some() {
            return Err(Error::Unsupported(
                "bound native startup needs a fresh observed context after seek",
            ));
        }
        if !self.input.capabilities().random_access {
            return Err(Error::Unsupported("TS replay needs source rewind"));
        }
        let TrackKey::Ts { program, pid: _ } = intent.track else {
            return Err(Error::Invalid("seek track format"));
        };
        if program != self.program {
            return Err(Error::Invalid("seek source program"));
        }
        if matches!(self.policy, SourceTimePolicy::Unanchored)
            && intent.clock == crate::ClockChoice::Source
        {
            return Err(Error::Unsupported("unanchored TS source time"));
        }
        self.input.reset(0)?;
        self.stream = TransportStream::new_with_namespace(
            self.input.budget().clone(),
            self.format,
            self.limits,
            self.program,
            self.namespace,
        )?;
        self.source_pes_clock = Some(DemuxClock::new(self.program)?);
        self.tsid = None;
        self.pcr_pid = None;
        self.pending = None;
        self.ended = false;
        self.draining = false;
        for track in self.pes_tracks.as_mut_slice() {
            track.observer = None;
            track.update = None;
        }
        self.pes_active = None;
        self.pes_adopting = 0;
        self.pes_pending = None;
        self.pes_clock = Some(DemuxClock::new(self.program)?);
        self.event_ordinal = 0;
        self.pes_updates_from = None;
        self.entity_end = None;
        Ok(())
    }
    fn from_stream(
        input: InputCursor,
        format: ts::PacketFormat,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
        stream: TransportStream,
    ) -> Result<Self> {
        let pool = input.budget().clone();
        let program = stream.selected_program();
        let clock = || match program {
            0 => Ok(None),
            program => DemuxClock::new(program).map(Some),
        };
        Ok(Self {
            bound_context: None,
            grid_lost: false,
            stream,
            source_pes_clock: clock()?,
            input,
            format,
            program,
            limits,
            policy,
            namespace,
            tsid: None,
            pcr_pid: None,
            facts: BudgetVec::new(&pool),
            pending: None,
            ended: false,
            draining: false,
            pes_tracks: PesTracks::Inline(None),
            pes_active: None,
            pes_adopting: 0,
            pes_pending: None,
            pes_clock: clock()?,
            event_ordinal: 0,
            suppressed_events: 0,
            pes_updates_from: None,
            delivery: Delivery::Program,
            entity_sequence: false,
            entity_end: None,
        })
    }

    /// The source events of `program`; see [`Engine::ts`].
    pub(crate) fn program(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
    ) -> Result<Self> {
        let stream = TransportStream::new_with_namespace(
            input.budget().clone(),
            format,
            limits,
            program,
            namespace,
        )?;
        Self::from_stream(input, format, limits, policy, namespace, stream)
    }

    /// The source events of a TS188 `program` and the demux PES of the selected
    /// PIDs: 1 to 64 distinct PIDs from 1 to 0x1ffe.
    pub(crate) fn with_selections(
        input: InputCursor,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
        selections: &[PidSelection],
    ) -> Result<Self> {
        if selections.is_empty() {
            return Err(Error::Invalid("native program selection is empty"));
        }
        if selections.len() > 64 {
            return Err(Error::Limit {
                requested: selections.len(),
                maximum: 64,
            });
        }
        if selections
            .iter()
            .any(|selection| !(1..=0x1ffe).contains(&selection.pid))
        {
            return Err(Error::Invalid("native program selection PID"));
        }
        let engine = Self::program(
            input,
            ts::PacketFormat::Ts188,
            program,
            limits,
            policy,
            namespace,
        )?;
        let pes_tracks = if let [selection] = selections {
            PesTracks::Inline(Some(PesTrack::new(*selection)))
        } else {
            let mut tracks = BudgetVec::new(engine.input.budget());
            tracks.try_reserve_exact(selections.len())?;
            for selection in selections {
                tracks
                    .try_push(PesTrack::new(*selection))
                    .map_err(|e| e.error)?;
            }
            tracks.as_mut_slice().sort_unstable_by_key(PesTrack::pid);
            if tracks
                .as_slice()
                .windows(2)
                .any(|pair| pair[0].pid() == pair[1].pid())
            {
                return Err(Error::Invalid("duplicate native program selection PID"));
            }
            PesTracks::Multiple(tracks)
        };
        Ok(Self {
            pes_tracks,
            ..engine
        })
    }

    /// The source events of `program` and the demux PES of one PID, with the
    /// observer bound to the startup context of this input.
    pub(crate) fn with_bound_input(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        selection: PidSelection,
        context: ts::compat::input::InputContext,
    ) -> Result<Self> {
        context.validate_cursor(&input)?;
        let engine = Self::program(
            input,
            format,
            program,
            limits,
            policy,
            DescriptorNamespace::Unspecified,
        )?;
        Ok(Self {
            pes_tracks: PesTracks::Inline(Some(PesTrack::new(selection))),
            bound_context: Some(context),
            ..engine
        })
    }

    /// `delivery` is one of the modes that discover their tracks.
    pub(crate) fn segment(
        input: InputCursor,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
        discovery: Delivery,
    ) -> Result<Self> {
        let budget = input.budget().clone();
        let stream = if discovery.is_progressive() {
            TransportStream::new_single_program_progressive_feed(budget, limits, namespace)?
        } else if discovery.is_feed() {
            TransportStream::new_single_program_feed(budget, limits, namespace)?
        } else {
            TransportStream::new_single_program_segment(budget, limits, namespace)?
        };
        let engine = Self::from_stream(
            input,
            ts::PacketFormat::Ts188,
            limits,
            policy,
            namespace,
            stream,
        )?;
        Ok(Self {
            delivery: discovery,
            ..engine
        })
    }

    /// A progressive PES feed of a single-program TS188 source: AVC and AAC
    /// tracks found in its PMT, delivered as owned or, with `compact`, compact
    /// payloads. Poll it with [`Self::poll_progressive_payload`].
    pub fn progressive(input: InputCursor, limits: ts::ParseLimits, compact: bool) -> Result<Self> {
        Self::segment(
            input,
            limits,
            SourceTimePolicy::Unanchored,
            DescriptorNamespace::Unspecified,
            if compact {
                Delivery::CompactProgressive
            } else {
                Delivery::Progressive
            },
        )
    }

    /// The input cursor the feed reads.
    #[inline]
    pub fn input(&self) -> &InputCursor {
        &self.input
    }

    /// Hands a source reply to the input; true when it was accepted.
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        reply.provide_to(&mut self.input)
    }

    /// The next step of a feed made by [`Self::progressive`].
    pub fn poll_progressive_payload(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<FeedStep> {
        if !self.delivery.is_progressive() {
            return Err(Error::Invalid("compact TS payload selection required"));
        }
        self.poll::<FeedStep>(work, cancel)
    }

    /// Source events the feed consumed without delivering since the last call.
    pub fn take_suppressed_events(&mut self) -> u64 {
        std::mem::take(&mut self.suppressed_events)
    }

    /// Hands a consumed PES's lists back to the observer of `pid` for its next PES.
    pub fn reuse_pes_lists(&mut self, pid: u16, lists: feed::SpentLists) {
        let Some(index) = self.pes_tracks.index(pid) else {
            return;
        };
        if let Some(observer) = self.pes_tracks.as_mut_slice()[index].observer.as_mut() {
            if let Some(spans) = lists.spans {
                observer.reuse_payload_spans(spans);
            }
            if let Some(header) = lists.header {
                observer.reuse_header_ranges(header);
            }
        }
    }

    fn offer_transport(
        &mut self,
        index: usize,
        mut packet: ts::transport::TransportPacket,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let track = &mut self.pes_tracks.as_mut_slice()[index];
        if let Some(observer) = track.observer.as_mut() {
            match observer.append_ready_continuation(&mut packet, context, work, cancel)? {
                ReadyContinuation::Buffered => return Ok(true),
                ReadyContinuation::Complete => {
                    self.pes_active = Some(index);
                    return Ok(false);
                }
                ReadyContinuation::Unhandled => {}
            }
            let pushed = if self.bound_context.is_some() {
                observer.push_with_input(packet, context, &self.input)
            } else {
                observer.push(packet, context)
            };
            match pushed {
                Ok(()) => self.pes_active = Some(index),
                Err(rejected) => {
                    track.refuse(rejected.error);
                    self.pes_updates_from = Some(index);
                }
            }
            return Ok(false);
        }
        Ok(true)
    }

    fn source_context(&self) -> SourceContext {
        SourceContext {
            source_event_ordinal: self.event_ordinal,
            epochs: self.stream.source_epochs(),
            byte_run_epoch: None,
        }
    }

    const FEED_EVENT_QUANTUM: u64 = 64;

    fn poll_feed_stream(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<TraceEvent>> {
        let tracks = &mut self.pes_tracks;
        let active = &mut self.pes_active;
        let ordinal = self.event_ordinal;
        let suppressed = self.suppressed_events;
        self.stream.poll_feed_with_payload_runs(
            &mut self.input,
            work,
            cancel,
            &mut |item, omitted, epochs, work| {
                let packet = match item {
                    FeedItem::Carrier(packet) => packet,
                    FeedItem::PayloadRun(run) => {
                        // Leave the carrier that triggers a yield to the
                        // ordinary path, including its deferred continuity event.
                        let maximum = (Self::FEED_EVENT_QUANTUM - 1)
                            .saturating_sub(suppressed.saturating_add(omitted))
                            .div_ceil(2) as usize;
                        if maximum >= 2
                            && active.is_none()
                            && let Some(source_event_ordinal) = ordinal.checked_add(omitted)
                            && let Some(index) = tracks.index(run.pid())
                            && let Some(observer) = tracks.as_mut_slice()[index].observer.as_mut()
                            && run.append_ready(
                                observer,
                                SourceContext {
                                    source_event_ordinal,
                                    epochs,
                                    byte_run_epoch: None,
                                },
                                maximum,
                                work,
                            )?
                        {
                            return Ok(FeedContinuation::Buffered);
                        }
                        return Ok(FeedContinuation::Emit);
                    }
                };
                // Leave one unit for event consumption in addition to the append.
                if work.remaining() < 3 {
                    return Ok(FeedContinuation::Emit);
                }
                if let Some(index) = tracks.index(packet.pid())
                    && let Some(observer) = tracks.as_mut_slice()[index].observer.as_mut()
                {
                    let context = SourceContext {
                        source_event_ordinal: ordinal
                            .checked_add(omitted)
                            .ok_or(Error::Overflow)?,
                        epochs,
                        byte_run_epoch: None,
                    };
                    match packet.append_ready_continuation(observer, context, work, cancel)? {
                        ReadyContinuation::Unhandled => return Ok(FeedContinuation::Emit),
                        ReadyContinuation::Buffered => {}
                        ReadyContinuation::Complete => *active = Some(index),
                    }
                }
                let charged = work.try_charge(1);
                debug_assert!(charged);
                if active.is_some()
                    || work.remaining() == 0
                    || suppressed.saturating_add(omitted) >= Self::FEED_EVENT_QUANTUM - 1
                {
                    Ok(FeedContinuation::Yield)
                } else {
                    Ok(FeedContinuation::Buffered)
                }
            },
        )
    }

    fn project_pending_packet<T: TsStepProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<T> {
        let packet = self
            .pes_pending
            .as_ref()
            .ok_or(Error::Invalid("native packet missing"))?;
        let projector = self
            .pes_clock
            .as_mut()
            .ok_or(Error::Invalid("native TS packet before program discovery"))?;
        let (pid, epochs, header) = match packet {
            FeedPacket::Owned(packet) => (packet.pid, packet.source_epochs, packet.time_header()),
            FeedPacket::Compact(packet) => (packet.pid, packet.source_epochs, packet.time_header()),
        };
        let Some(clock) = projector.project_header(pid, epochs, header, work, cancel)? else {
            return T::project(FeedStep::Yield);
        };
        let packet = self
            .pes_pending
            .take()
            .ok_or(Error::Invalid("native packet missing"))?;
        T::project(FeedStep::Packet { packet, clock })
    }

    /// The next selected track's carriage or refusal from track `at` on, if any
    /// is waiting.
    #[inline(never)]
    #[cold]
    fn next_track_update(&mut self, at: usize) -> Option<FeedStep> {
        let (index, pid, update) = self
            .pes_tracks
            .as_mut_slice()
            .iter_mut()
            .enumerate()
            .skip(at)
            .find_map(|(i, track)| track.update.take().map(|update| (i, track.pid(), update)))?;
        self.pes_updates_from = Some(index + 1);
        Some(match update {
            TrackUpdate::Configuration(configuration) => FeedStep::Configuration(configuration),
            TrackUpdate::Refusal(error) => FeedStep::Refusal {
                track: TrackKey::Ts {
                    program: self.program,
                    pid,
                },
                error,
                source_event_ordinal: self.event_ordinal,
            },
        })
    }

    /// Ends the current entity: a segment reports its end, and the source's
    /// end starts draining the stream and every selected track.
    #[cold]
    fn end_entity<T: TsStepProjection>(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<T> {
        if self.draining {
            return Err(Error::Invalid("TS repeated undrained entity End"));
        }
        if self.entity_sequence {
            if let Some(end) = self.stream.media_entity_end(&mut self.input, w, c)? {
                self.entity_end = Some(end.clone());
                return T::project(FeedStep::EntityEnd(end));
            }
            return T::project(FeedStep::Yield);
        }
        if self.delivery.discovers_tracks() && self.pcr_pid.is_none() {
            return Err(Error::Invalid("TS segment ended without an active PMT"));
        }
        self.stream.end_of_source()?;
        self.draining = true;
        for track in self.pes_tracks.as_mut_slice() {
            if let Some(observer) = track.observer.as_mut() {
                observer.end_of_source()?;
            }
        }
        self.pes_active = Some(0);
        T::project(FeedStep::Yield)
    }

    /// Refuses every selected track once an M2TS input loses its carrier grid.
    #[cold]
    fn refuse_after_lost_grid(&mut self) {
        self.grid_lost = true;
        for track in self.pes_tracks.as_mut_slice() {
            track.refuse(Error::Unsupported("native M2TS carrier grid lost"));
        }
        self.pes_updates_from = Some(0);
    }

    // One function on purpose: the measured per-carrier loop, compiled once for a
    // player (`EngineStep`) and once for a progressive reader (`FeedStep`, in which
    // `T::PROGRESSIVE` folds the mode tests). Its rare parts are the cold functions
    // around it.
    pub(crate) fn poll<T: TsStepProjection>(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<T> {
        if let Some(at) = self.pes_updates_from.take()
            && self
                .pes_tracks
                .as_slice()
                .get(at..)
                .is_some_and(|tracks| tracks.iter().any(|track| track.update.is_some()))
            && let Some(event) = self.next_track_update(at)
        {
            return T::project(event);
        }
        if self.pes_pending.is_some() {
            return self.project_pending_packet(w, c);
        }
        while let Some(index) = self.pes_active {
            let Some(track) = self.pes_tracks.as_mut_slice().get_mut(index) else {
                self.pes_active = None;
                break;
            };
            if let Some(observer) = track.observer.as_mut() {
                let result = if observer.uses_compact_payloads() {
                    let step = if self.bound_context.is_some() {
                        observer.poll_compact_payload_with_input(&self.input, w, c)
                    } else {
                        observer.poll_compact_payload(w, c)
                    };
                    step.map(|step| map_payload(step, FeedPacket::Compact))
                } else {
                    let step = if self.bound_context.is_some() {
                        observer.poll_with_input(&self.input, w, c)
                    } else {
                        observer.poll(w, c)
                    };
                    step.map(|step| map_payload(step, FeedPacket::Owned))
                };
                match result {
                    Ok(ts::AssemblyStep::Output(packet)) => {
                        self.pes_pending = Some(packet);
                        if T::PROGRESSIVE
                            || self.delivery == Delivery::Program
                            || self.delivery.is_progressive()
                        {
                            return self.project_pending_packet(w, c);
                        }
                        return T::project(FeedStep::Yield);
                    }
                    Ok(ts::AssemblyStep::Yield) => return T::project(FeedStep::Yield),
                    Ok(ts::AssemblyStep::NeedPayload | ts::AssemblyStep::Finished) => {}
                    Err(error @ (Error::Backpressure { .. } | Error::Cancelled)) => {
                        return Err(error);
                    }
                    Err(error) => {
                        track.refuse(error);
                        self.pes_updates_from = Some(index);
                        self.pes_active = self.draining.then_some(index + 1);
                        return T::project(FeedStep::Yield);
                    }
                }
            }
            self.pes_active = self.draining.then_some(index + 1);
        }
        let progressive = T::PROGRESSIVE || self.delivery.is_progressive();
        let feed_only = progressive || self.delivery.is_feed();
        if self.ended {
            return T::project(FeedStep::End);
        }
        if self.entity_end.is_some() {
            return T::project(FeedStep::AwaitEntity);
        }
        loop {
            if self.pending.is_none() {
                let step = if feed_only {
                    self.poll_feed_stream(w, c)
                } else {
                    self.stream.poll(&mut self.input, w, c)
                };
                if feed_only {
                    let omitted = self.stream.take_feed_omissions();
                    self.event_ordinal = self
                        .event_ordinal
                        .checked_add(omitted)
                        .ok_or(Error::Overflow)?;
                    self.suppressed_events += omitted;
                }
                match step? {
                    Step::Ready(e) => self.pending = Some(e),
                    Step::NeedInput(r) => return T::project(FeedStep::NeedInput(r)),
                    Step::Yield => return T::project(FeedStep::Yield),
                    Step::End(_) => return self.end_entity(w, c),
                }
            }
            if (feed_only || !matches!(self.pending, Some(TraceEvent::Pes(_)))) && !w.try_charge(1)
            {
                return T::project(FeedStep::Yield);
            }
            if let Some(TraceEvent::Diagnostic(fault)) = &self.pending
                && fault.kind == ts::stream::FaultKind::Transport
                && matches!(
                    fault.error,
                    Error::Invalid("TS sync byte") | Error::Truncated
                )
                && !self.grid_lost
                && self
                    .bound_context
                    .as_ref()
                    .is_some_and(triergon_mpegts::compat::input::InputContext::is_prefix)
            {
                self.refuse_after_lost_grid();
            }
            if matches!(
                self.pending,
                Some(TraceEvent::ProgramMap { active: true, .. } | TraceEvent::Pat(_))
            ) && self.adopt_table(feed_only)?
            {
                return T::project(FeedStep::Yield);
            }
            let event = self
                .pending
                .as_ref()
                .ok_or(Error::Invalid("TS event missing"))?;
            // A feed consumes the payload carriers and the source facts its
            // observers need. The source parser still validates every carrier.
            if feed_only
                && !matches!(
                    event,
                    TraceEvent::ProgramMap { active: true, .. } | TraceEvent::Diagnostic(_)
                )
                && !(progressive
                    && matches!(
                        event,
                        TraceEvent::Pat(_)
                            | TraceEvent::Continuity { .. }
                            | TraceEvent::Timeline { .. }
                    ))
            {
                let event = self
                    .pending
                    .take()
                    .ok_or(Error::Invalid("TS event missing"))?;
                let mut drained = true;
                match event {
                    TraceEvent::Transport(packet) => {
                        if let Some(index) = self.pes_tracks.index(packet.pid) {
                            let context = self.source_context();
                            drained = self.offer_transport(index, packet, context, w, c)?;
                        }
                    }
                    TraceEvent::Pes(_) if self.program == 0 => {
                        return Err(Error::Invalid("TS PES before program discovery"));
                    }
                    TraceEvent::Finished => self.ended = true,
                    _ => {}
                }
                self.event_ordinal = self.event_ordinal.checked_add(1).ok_or(Error::Overflow)?;
                self.suppressed_events += 1;
                // Return periodically to the player's cancellation and event cap.
                if !drained
                    || self.ended
                    || w.remaining() == 0
                    || self.suppressed_events >= Self::FEED_EVENT_QUANTUM
                {
                    return T::project(FeedStep::Yield);
                }
                continue;
            }
            // A feed delivers only its rare table, clock and fault events whole.
            return if feed_only {
                self.deliver_feed_event(w, c)
            } else {
                self.deliver_event(w, c)
            };
        }
    }

    /// Adopts the pending PAT or active PMT: the program, the selected tracks and
    /// their observers, and the track facts. True while the PMT's next track
    /// still needs its observer, which leaves the event pending.
    #[cold]
    fn adopt_table(&mut self, feed_only: bool) -> Result<bool> {
        let Some(event) = self.pending.as_ref() else {
            return Ok(false);
        };
        if let TraceEvent::ProgramMap {
            map,
            active: true,
            configuration_epoch,
        } = event
        {
            if map.streams.len() > 64 {
                return Err(Error::Limit {
                    requested: map.streams.len(),
                    maximum: 64,
                });
            }
            if self.delivery.discovers_tracks() && self.pes_adopting == 0 {
                self.pes_tracks = self.discover_tracks(map)?;
            }
            if !self.grid_lost
                && let Some(track) = self.pes_tracks.as_slice().get(self.pes_adopting)
            {
                let observer = self.observer_for(track.selection, map, *configuration_epoch);
                let track = &mut self.pes_tracks.as_mut_slice()[self.pes_adopting];
                match observer {
                    Ok((mut observer, carriage)) => {
                        if let Some(context) = self.bound_context.as_ref() {
                            observer.bind_input(context, &self.input)?;
                        }
                        track.observer = Some(observer);
                        track.update = (self.delivery.discovers_tracks()
                            || track.selection.carriage == Carriage::AudioFromPmt)
                            .then_some(TrackUpdate::Configuration(AdmittedCarriage {
                                track: TrackKey::Ts {
                                    program: self.program,
                                    pid: track.pid(),
                                },
                                configuration_epoch: *configuration_epoch,
                                carriage,
                            }));
                    }
                    Err(error) => track.refuse(error),
                }
                self.pes_adopting += 1;
                if self.pes_adopting < self.pes_tracks.as_slice().len() {
                    return Ok(true);
                }
            }
            self.facts
                .try_reserve_exact(map.streams.len().saturating_sub(self.facts.len()))?;
            self.facts.clear();
            for e in map.streams.as_slice() {
                let observer = self
                    .pes_tracks
                    .index(e.pid)
                    .and_then(|i| self.pes_tracks.as_slice()[i].observer.as_ref());
                let kind = track_kind(selected_track_kind(e, observer));
                self.facts
                    .try_push(TrackFact {
                        key: TrackKey::Ts {
                            program: self.program,
                            pid: e.pid,
                        },
                        kind,
                        enabled: true,
                    })
                    .map_err(|e| e.error)?;
            }
            self.pes_adopting = 0;
            self.pes_updates_from = self
                .pes_tracks
                .as_slice()
                .iter()
                .position(|track| track.update.is_some());
            self.pcr_pid = Some(map.pcr_pid);
        }
        if let TraceEvent::Pat(table) = event {
            if self.program == 0 {
                let program = self.stream.selected_program();
                if !feed_only {
                    self.source_pes_clock = Some(DemuxClock::new(program)?);
                }
                self.pes_clock = Some(DemuxClock::new(program)?);
                self.program = program;
            }
            self.tsid = Some(table.header.extension);
        }
        Ok(false)
    }

    /// Delivers the pending source event with its source and demux clocks,
    /// offering a transport packet to its track's observer first.
    #[inline(always)]
    fn deliver_event<T: TsStepProjection>(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<T> {
        let event = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("TS event missing"))?;
        let key = match (self.tsid, self.pcr_pid) {
            (Some(tsid), Some(pcr_pid)) => Some(ClockKey {
                tsid,
                program: self.program,
                pcr_pid,
                epoch: match event {
                    TraceEvent::Pes(p) => p.epochs.clock,
                    _ => self.stream.source_epochs().clock,
                },
            }),
            _ => None,
        };
        let mut source_pts = None;
        let mut source_dts = None;
        let mut demux_clock = None;
        if let TraceEvent::Pes(pes) = event {
            if let Some(key) = key {
                source_pts = Some(source_timestamp(pes.header.pts_raw33, key, self.policy)?);
                source_dts = Some(source_timestamp(pes.header.dts_raw33, key, self.policy)?);
            }
            // The demux clock charges its own work and may yield, which leaves the
            // source event pending. No demux time is copied into the source fields.
            let projector = self
                .source_pes_clock
                .as_mut()
                .ok_or(Error::Invalid("TS PES before program discovery"))?;
            demux_clock = projector.project(pes, w, c)?;
            if demux_clock.is_none() {
                return T::project(FeedStep::Yield);
            }
        }
        let event = self
            .pending
            .take()
            .ok_or(Error::Invalid("TS event missing"))?;
        if let TraceEvent::Transport(packet) = &event
            && let Some(index) = self.pes_tracks.index(packet.pid)
        {
            let context = self.source_context();
            let track = &mut self.pes_tracks.as_mut_slice()[index];
            if let Some(observer) = track.observer.as_mut() {
                // Keep one unit for delivery of the original source event.
                let ready = if w.remaining() >= 3 {
                    observer.append_ready_borrowed_continuation(
                        packet,
                        context,
                        &self.input,
                        w,
                        c,
                    )?
                } else {
                    ReadyContinuation::Unhandled
                };
                match ready {
                    ReadyContinuation::Buffered => {}
                    ReadyContinuation::Complete => self.pes_active = Some(index),
                    ReadyContinuation::Unhandled => match observer.push_borrowed(
                        packet,
                        context,
                        self.bound_context.as_ref().map(|_| &self.input),
                    ) {
                        Ok(()) => self.pes_active = Some(index),
                        Err(error) => {
                            track.refuse(error);
                            self.pes_updates_from = Some(index);
                        }
                    },
                }
            }
        }
        self.event_ordinal = self.event_ordinal.checked_add(1).ok_or(Error::Overflow)?;
        if matches!(event, TraceEvent::Finished) {
            self.ended = true;
        }
        T::project(FeedStep::Event(TsEvent {
            source: event,
            source_pts,
            source_dts,
            demux_clock,
            clock_key: key,
        }))
    }

    /// [`Self::deliver_event`] for a feed, which consumes its payload events and
    /// delivers only the rare ones whole.
    #[cold]
    fn deliver_feed_event<T: TsStepProjection>(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<T> {
        self.deliver_event(w, c)
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use crate::event::{EngineStep, Event};
    use ts::compat::pes::{DemuxProfile, DemuxTimeView};
    use ts::{
        SourceEpochs,
        clock::SourceTimePolicy,
        compat::pes::{ClockOrigin, TimestampOrigin},
    };

    #[test]
    fn ts_projection_preserves_source_clock_fields_and_refusal_ordinal() {
        let key = ClockKey {
            tsid: 3,
            program: 7,
            pcr_pid: 256,
            epoch: 11,
        };
        let event = TsEvent {
            source: TraceEvent::Timeline {
                program: key.program,
                clock_epoch: key.epoch,
            },
            source_pts: Some(
                source_timestamp(Some(9), key, SourceTimePolicy::RelativeEpochZero).unwrap(),
            ),
            source_dts: Some(source_timestamp(None, key, SourceTimePolicy::Unanchored).unwrap()),
            demux_clock: Some(DemuxTimeView {
                profile: DemuxProfile::PesPackets,
                program: key.program,
                pid: 257,
                source_epochs: SourceEpochs {
                    clock: key.epoch,
                    configuration: 13,
                },
                source_pts_raw33: Some(9),
                source_dts_raw33: None,
                pts90: Some(-17),
                dts90: Some(-17),
                pts_origin: TimestampOrigin::SourcePts,
                dts_origin: TimestampOrigin::PtsOnlyCompatibility,
                origin: Some(ClockOrigin {
                    first_reference_raw90: 9,
                    interval_origin90: -26,
                }),
            }),
            clock_key: Some(key),
        };
        let expected = format!("{event:?}");
        let step = FeedStep::project(FeedStep::Event(event)).unwrap();
        assert_eq!(step.payload_bytes(), Some(0));
        let EngineStep::Event(Event::Ts(event)) = EngineStep::project(step).unwrap() else {
            panic!("source event was not delivered");
        };
        assert_eq!(format!("{event:?}"), expected);

        let track = TrackKey::Ts {
            program: 7,
            pid: 257,
        };
        let error = Error::Unsupported("test carriage");
        let step = FeedStep::project(FeedStep::Refusal {
            track,
            error,
            source_event_ordinal: 109,
        })
        .unwrap();
        assert_eq!(step.payload_bytes(), Some(0));
        assert!(
            matches!(EngineStep::project(step), Ok(EngineStep::Event(Event::PesRefusal {
            track: actual_track, error: actual_error, source_event_ordinal: 109,
        })) if actual_track == track && actual_error == error)
        );
        assert_eq!(FeedStep::End.payload_bytes(), None);
        assert_eq!(FeedStep::AwaitEntity.payload_bytes(), None);
    }
}
