//! Composed selected-program TS -> PSI -> PES source operation. No codec AU work.
use crate::carrier::{PLAIN_PAYLOAD_BYTES, TS188_BYTES, payload_only_continuation};
use crate::compat::pes::{DemuxObserver, ReadyContinuation, SourceContext};
use crate::framer::FramedCarrier;
use crate::psi::{
    DescriptorNamespace, PatCollector, PatReader, PatTable, PmtReader, ProgramMap, Section,
    TableStep,
};
use crate::{
    AssemblyStep, Continuity, ContinuityOutcome, NullCarrierRun, PacketFormat, ParseLimits, Pes,
    PesAssembler, SectionAssembler, SourceEpochs, TransportFramer, TransportPacket,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, Result, SourceId, SourceLocation, Step,
    WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    Transport,
    Continuity,
    Psi,
    Pes,
    Program,
}
#[derive(Clone, Debug)]
pub struct TraceFault {
    pub kind: FaultKind,
    pub pid: Option<u16>,
    pub location: SourceLocation,
    pub error: Error,
    /// Original owned rejected carrier; absent for non-carrier/operational faults.
    pub rejected: Option<crate::RejectedTransport>,
}
/// One result at a time. Every dynamic member owns shared charged storage; there
/// is no uncharged boxed variant or internal unbounded event queue.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one inline owned source event; heap storage inside variants is already charged"
)]
pub enum TraceEvent {
    Transport(TransportPacket),
    Continuity {
        pid: u16,
        outcome: ContinuityOutcome,
    },
    Section(Section),
    Pat(PatTable),
    ProgramMap {
        map: ProgramMap,
        active: bool,
        configuration_epoch: u64,
    },
    Pes(Pes),
    Timeline {
        program: u16,
        clock_epoch: u64,
    },
    Discontinuity {
        epochs: SourceEpochs,
    },
    Diagnostic(TraceFault),
    Finished,
}

/// Disposition of a validated PES body carrier offered directly to a feed.
#[derive(Debug)]
pub enum FeedContinuation {
    Emit,
    Buffered,
    Yield,
}

/// A validated continuation offered to a feed's native observer.
#[derive(Debug)]
pub struct FeedCarrier<'a>(&'a mut FramedCarrier);

impl FeedCarrier<'_> {
    pub fn pid(&self) -> u16 {
        self.0.pid()
    }

    /// Transfer the payload when the observer can append it without allocation.
    /// An unhandled carrier remains available for ordinary delivery.
    pub fn append_ready_continuation(
        self,
        observer: &mut DemuxObserver,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ReadyContinuation> {
        match self.0 {
            FramedCarrier::Packet(packet) => {
                observer.append_ready_continuation(packet, context, work, cancel)
            }
            FramedCarrier::Plain(packet) => {
                observer.append_plain_continuation(packet, context, work, cancel)
            }
        }
    }
}

/// An ordinary carrier or a shared-window prefix offered to a progressive feed.
#[derive(Debug)]
pub enum FeedItem<'a> {
    Carrier(FeedCarrier<'a>),
    PayloadRun(FeedPayloadRun<'a>),
}

// Per carrier: input, header inspection, source PES, native append and event.
const PAYLOAD_RUN_WORK: u64 = TS188_BYTES as u64 + 2 + (PLAIN_PAYLOAD_BYTES as u64 + 1) + 2 + 1;

fn matches_payload_header(ts: &[u8], pid: u16, counter: u8) -> bool {
    let expected = 0x4700_0010 | (u32::from(pid) << 8) | u32::from(counter);
    // Transport priority is the only bit ignored by the continuation check.
    ts.first_chunk::<4>()
        .is_some_and(|header| u32::from_be_bytes(*header) & !0x0020_0000 == expected)
}

/// A same-PID continuation prefix that may be appended to a native observer.
#[derive(Debug)]
pub struct FeedPayloadRun<'a> {
    input: &'a mut InputCursor,
    framer: &'a mut TransportFramer,
    continuity: &'a mut Continuity,
    assembler: &'a mut PesAssembler,
    omissions: &'a mut u64,
    cancel: &'a AtomicBool,
    epochs: SourceEpochs,
    pid: u16,
    counter: u8,
    maximum: usize,
}

impl FeedPayloadRun<'_> {
    pub fn pid(&self) -> u16 {
        self.pid
    }

    /// Accept a run of at least two carriers, up to the caller's event limit.
    /// False leaves input, work, observer and source state ready for the
    /// ordinary carrier path.
    pub fn append_ready(
        self,
        observer: &mut DemuxObserver,
        context: SourceContext,
        maximum: usize,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        if context.epochs != self.epochs || context.byte_run_epoch.is_some() {
            return Ok(false);
        }
        let Some(carrier) = self.input.buffered_owner() else {
            return Ok(false);
        };
        let start = SourceLocation {
            source: self.input.source(),
            offset: self.input.position(),
        };
        let (slots, growth) =
            observer.payload_run_capacity(carrier, self.pid, self.counter, start, context);
        let growth_work = growth.as_ref().map_or(0, |plan| plan.remaining);
        let work_limit = work
            .remaining()
            .saturating_sub(1)
            .saturating_sub(growth_work)
            / PAYLOAD_RUN_WORK;
        let maximum = maximum
            .min(self.maximum)
            .min(usize::try_from(work_limit).unwrap_or(usize::MAX))
            .min(slots);
        if maximum < 2 {
            return Ok(false);
        }

        // The first two headers were checked before routing the run. Finish
        // the probe inside this window, stopping before the first exception.
        let mut carriers = 2;
        for ts in self
            .input
            .buffered()
            .chunks_exact(TS188_BYTES)
            .take(maximum)
            .skip(2)
        {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let expected = self.counter.wrapping_add(carriers as u8) & 15;
            if !matches_payload_header(ts, self.pid, expected) {
                break;
            }
            carriers += 1;
        }
        let events = 2 * carriers as u64;
        let Some(omissions) = self.omissions.checked_add(events) else {
            return Ok(false);
        };
        if context.source_event_ordinal.checked_add(events).is_none() {
            return Ok(false);
        }
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let replacement = if let Some(growth) = growth {
            let Some(replacement) = observer.prepare_payload_run_growth(&growth) else {
                return Ok(false);
            };
            Some(replacement)
        } else {
            None
        };

        // The cursor charges the input bytes. The remaining checks and span
        // writes have already been bounded and need no allocation.
        let bytes = self
            .framer
            .read_payload_run(self.input, carriers, work, self.cancel)?;
        let charged = work
            .try_charge(growth_work + (PAYLOAD_RUN_WORK - TS188_BYTES as u64) * carriers as u64);
        debug_assert!(charged);
        observer.append_payload_run(bytes, start, context, self.counter, replacement, |last| {
            self.continuity.record_payload_run(self.counter, last);
        })?;
        self.assembler
            .record_body_carriers(carriers, carriers * PLAIN_PAYLOAD_BYTES);
        *self.omissions = omissions;
        Ok(true)
    }
}

const FEED_FALLBACK_PHASE: u8 = 8;

#[derive(Debug)]
struct PsiRoute {
    pid: u16,
    continuity: Continuity,
    assembler: SectionAssembler,
}
impl PsiRoute {
    fn new(pid: u16, budget: &Budget, limits: ParseLimits) -> Result<Self> {
        Ok(Self {
            pid,
            continuity: Continuity::new(pid)?,
            assembler: SectionAssembler::new(pid, budget.clone(), limits)?,
        })
    }
}
#[derive(Debug)]
struct PesRoute {
    pid: u16,
    stream_type: u8,
    descriptors: Bytes,
    epochs: SourceEpochs,
    continuity: Option<Continuity>,
    assembler: Option<PesAssembler>,
    reuse: Option<usize>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Pat,
    Pmt,
    Pes(usize),
    Clock,
    Ignore,
}
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one bounded in-progress source table parser; no extra boxed allocator outside Budget"
)]
enum Job {
    Pat(PatReader),
    Pmt(PmtReader),
}
#[derive(Debug)]
struct MapBuild {
    map: ProgramMap,
    routes: BudgetVec<PesRoute>,
    position: usize,
    epoch: u64,
}

/// Opt-in source view. A run expands to one transport event per carrier,
/// in physical order, with no intervening continuity or clock events.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "keep one owned source event inline"
)]
pub enum CompactTraceEvent {
    Event(TraceEvent),
    NullRun(NullCarrierRun),
}

// Select the returned event type before entering the shared parser loop. Ordinary
// feeds never construct a compact event just to unpack its source event again.
trait PollOutput: Sized {
    const COMPACT: bool = false;

    fn event(event: TraceEvent) -> Self;

    fn null_run(
        _framer: &mut TransportFramer,
        _input: &mut InputCursor,
        _work: &mut WorkBudget,
        _cancel: &AtomicBool,
    ) -> Result<Option<Self>> {
        Ok(None)
    }
}

impl PollOutput for TraceEvent {
    fn event(event: Self) -> Self {
        event
    }
}

impl PollOutput for CompactTraceEvent {
    const COMPACT: bool = true;

    fn event(event: TraceEvent) -> Self {
        Self::Event(event)
    }

    fn null_run(
        framer: &mut TransportFramer,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Self>> {
        Ok(framer.try_null_run(input, work, cancel)?.map(Self::NullRun))
    }
}

/// The caller selects the program explicitly or requests single-program
/// discovery. All PAT associations and raw transport/section facts remain
/// visible.
#[derive(Debug)]
pub struct TransportStream {
    budget: Budget,
    format: PacketFormat,
    limits: ParseLimits,
    program: u16,
    discover_program: bool,
    feed_omissions: Option<u64>,
    feed_validated_carriers: bool,
    deferred_feed_events: u64,
    descriptor_namespace: DescriptorNamespace,
    framer: TransportFramer,
    pat: PsiRoute,
    pmt: Option<PsiRoute>,
    collector: PatCollector,
    routes: BudgetVec<PesRoute>,
    pmt_source: Option<Bytes>,
    pmt_version: Option<u8>,
    tsid: Option<u16>,
    pcr_pid: Option<u16>,
    clock_continuity: Option<Continuity>,
    clock_pending: bool,
    clock_signal: bool,
    // None until the first explicitly accepted new-clock boundary; then saturate
    // at the normative minimum of two samples before another boundary.
    clock_samples: Option<u8>,
    clock_quarantined: bool,
    clock_epoch: u64,
    config_epoch: u64,
    packet: Option<TransportPacket>,
    phase: u8,
    route: Route,
    outcome: Option<ContinuityOutcome>,
    fault: Option<TraceFault>,
    section: Option<Section>,
    job: Option<Job>,
    map_build: Option<MapBuild>,
    entity_end: Option<SourceLocation>,
    logical_end: bool,
    eof_route: usize,
    finished: bool,
    boundary_event: bool,
}

fn retryable(error: Error) -> bool {
    matches!(
        error,
        Error::Backpressure { .. } | Error::Allocation | Error::Cancelled
    )
}

impl TransportStream {
    pub fn new(
        budget: Budget,
        format: PacketFormat,
        limits: ParseLimits,
        program: u16,
    ) -> Result<Self> {
        Self::new_with_namespace(
            budget,
            format,
            limits,
            program,
            DescriptorNamespace::Unspecified,
        )
    }
    /// An explicit, immutable caller contract supplies descriptor namespace evidence.
    /// Raw source facts and clock/configuration epochs are unaffected by this choice.
    pub fn new_with_namespace(
        budget: Budget,
        format: PacketFormat,
        limits: ParseLimits,
        program: u16,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        if program == 0 {
            return Err(Error::Invalid("network PID is not a selected program"));
        }
        Self::new_with_program(budget, format, limits, Some(program), descriptor_namespace)
    }

    /// Read one TS188 segment with an in-band PAT and PMT. The complete PAT
    /// must identify exactly one media program. Program zero is a network PID.
    pub fn new_single_program_segment(
        budget: Budget,
        limits: ParseLimits,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        Self::new_with_program(
            budget,
            PacketFormat::Ts188,
            limits,
            None,
            descriptor_namespace,
        )
    }

    /// Read one independent segment, sharing complete carriers with the
    /// input window and validating source PES without retaining it. Feed
    /// callers still receive transport packets, program tables and faults.
    pub fn new_single_program_feed(
        budget: Budget,
        limits: ParseLimits,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        let mut stream = Self::new_single_program_segment(budget, limits, descriptor_namespace)?;
        stream.framer.share_input();
        stream.feed_omissions = Some(0);
        Ok(stream)
    }

    /// A sequential feed publishes PES carriers after continuity validation.
    /// PAT, PMT, continuity breaks, clock transitions and faults remain visible.
    pub fn new_single_program_progressive_feed(
        budget: Budget,
        limits: ParseLimits,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        let mut stream = Self::new_single_program_segment(budget, limits, descriptor_namespace)?;
        stream.framer.compact_progressive_input();
        stream.feed_omissions = Some(0);
        stream.feed_validated_carriers = true;
        Ok(stream)
    }

    /// Count source events suppressed since the last call, for consumers
    /// that track positions in the full source trace.
    pub fn take_feed_omissions(&mut self) -> u64 {
        self.feed_omissions.as_mut().map_or(0, std::mem::take)
    }

    fn omit_feed_events(&mut self, count: u64) -> Result<()> {
        let omissions = self
            .feed_omissions
            .as_mut()
            .ok_or(Error::Invalid("feed event omitted from source trace"))?;
        *omissions = omissions.checked_add(count).ok_or(Error::Overflow)?;
        Ok(())
    }

    pub(crate) fn new_with_program(
        budget: Budget,
        format: PacketFormat,
        limits: ParseLimits,
        program: Option<u16>,
        descriptor_namespace: DescriptorNamespace,
    ) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            framer: TransportFramer::new(budget.clone(), format),
            pat: PsiRoute::new(0, &budget, limits)?,
            collector: PatCollector::new(budget.clone(), limits)?,
            routes: BudgetVec::new(&budget),
            budget,
            format,
            limits,
            program: program.unwrap_or_default(),
            discover_program: program.is_none(),
            feed_omissions: None,
            feed_validated_carriers: false,
            deferred_feed_events: 0,
            descriptor_namespace,
            pmt: None,
            pmt_source: None,
            pmt_version: None,
            tsid: None,
            pcr_pid: None,
            clock_continuity: None,
            clock_pending: false,
            clock_signal: false,
            clock_samples: None,
            clock_quarantined: false,
            clock_epoch: 0,
            config_epoch: 0,
            packet: None,
            phase: 0,
            route: Route::Ignore,
            outcome: None,
            fault: None,
            section: None,
            job: None,
            map_build: None,
            entity_end: None,
            logical_end: false,
            eof_route: 0,
            finished: false,
            boundary_event: false,
        })
    }
    /// Zero until the first complete PAT in single-program discovery mode.
    pub fn selected_program(&self) -> u16 {
        self.program
    }
    pub fn source_epochs(&self) -> SourceEpochs {
        SourceEpochs {
            clock: self.clock_epoch,
            configuration: self.config_epoch,
        }
    }
    /// Invalid signalling keeps its raw facts/diagnostics but cannot supply a
    /// trusted clock interpretation until an explicit format discontinuity/reset.
    pub fn clock_is_quarantined(&self) -> bool {
        self.clock_quarantined
    }
    fn location(&self) -> SourceLocation {
        self.packet.as_ref().map_or(
            self.entity_end.unwrap_or(SourceLocation {
                source: SourceId(0),
                offset: 0,
            }),
            |packet| SourceLocation {
                source: packet.source_id.unwrap_or(SourceId(0)),
                offset: packet.source_offset,
            },
        )
    }
    fn diagnostic(&self, kind: FaultKind, pid: Option<u16>, error: Error) -> TraceEvent {
        TraceEvent::Diagnostic(TraceFault {
            kind,
            pid,
            error,
            rejected: None,
            location: self.location(),
        })
    }
    fn find_route(&self, pid: u16) -> Route {
        if pid == 0 {
            Route::Pat
        } else if self.pmt.as_ref().is_some_and(|pmt| pmt.pid == pid) {
            Route::Pmt
        } else if let Some(index) = self
            .routes
            .as_slice()
            .iter()
            .position(|route| route.pid == pid)
        {
            Route::Pes(index)
        } else if self.pcr_pid == Some(pid) {
            Route::Clock
        } else {
            Route::Ignore
        }
    }
    fn find_feed_route(&self, pid: u16) -> Route {
        // Elementary PIDs are unique. PAT/PMT changes clear or replace the
        // table before another carrier is read; recheck the indexed PID.
        if let Route::Pes(index) = self.route
            && self
                .routes
                .as_slice()
                .get(index)
                .is_some_and(|route| route.pid == pid)
        {
            return self.route;
        }
        self.find_route(pid)
    }
    fn discard_route(&mut self, route: Route) {
        match route {
            Route::Pat => self.pat.assembler.discard(),
            Route::Pmt => {
                if let Some(pmt) = self.pmt.as_mut() {
                    pmt.assembler.discard();
                }
            }
            Route::Pes(index) => {
                if let Some(PesRoute {
                    assembler: Some(assembler),
                    epochs,
                    ..
                }) = self.routes.as_mut_slice().get_mut(index)
                {
                    assembler.discard(*epochs);
                }
            }
            Route::Clock | Route::Ignore => {}
        }
    }
    /// Explicit logical discontinuity, at an entity boundary after jobs drain.
    /// Source IDs and core generations are not rewritten into clock/config epochs.
    pub fn discontinuity(&mut self) -> Result<()> {
        if self.packet.is_some()
            || self.deferred_feed_events != 0
            || self.job.is_some()
            || self.section.is_some()
            || self.map_build.is_some()
        {
            return Err(Error::Invalid(
                "discontinuity while TS operation is pending",
            ));
        }
        let clock = self.clock_epoch.checked_add(1).ok_or(Error::Overflow)?;
        let config = self.config_epoch.checked_add(1).ok_or(Error::Overflow)?;
        self.clock_epoch = clock;
        self.config_epoch = config;
        self.framer.discard();
        self.pat = PsiRoute::new(0, &self.budget, self.limits)?;
        self.collector.discard();
        self.pmt = None;
        self.routes = BudgetVec::new(&self.budget);
        self.pmt_source = None;
        self.pmt_version = None;
        self.tsid = None;
        self.pcr_pid = None;
        self.clock_continuity = None;
        self.clock_pending = false;
        self.clock_signal = false;
        self.clock_samples = None;
        self.clock_quarantined = false;
        self.boundary_event = true;
        Ok(())
    }
    /// Seal the actual core End after this source operation has drained. Pending
    /// PSI/PES may remain: an entity End is not a logical EOF or discontinuity.
    pub fn media_entity_end(
        &self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::MediaEntityEnd>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !self.budget.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        let location = self
            .entity_end
            .ok_or(Error::Invalid("media entity End not observed"))?;
        if self.logical_end
            || self.finished
            || self.packet.is_some()
            || self.job.is_some()
            || self.section.is_some()
            || self.map_build.is_some()
            || self.boundary_event
            || location.source != input.source()
            || location.offset != input.position()
            || !self.framer.observed_entity_end(input)
        {
            return Err(Error::Invalid("media entity End not drained"));
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        match input.read_into(&mut [0_u8; 1], work, cancel)? {
            Step::End(actual) if actual == location => Ok(Some(crate::MediaEntityEnd {
                location,
                token: input.token(),
                epochs: self.source_epochs(),
                program: self.program,
                format: self.format,
                budget: self.budget.clone(),
            })),
            _ => Err(Error::Invalid("media entity End not confirmed by core")),
        }
    }
    /// Call after an observed entity End and only when that is the logical EOF.
    pub fn end_of_source(&mut self) -> Result<()> {
        if self.entity_end.is_none()
            || self.packet.is_some()
            || self.job.is_some()
            || self.section.is_some()
            || self.map_build.is_some()
        {
            return Err(Error::Invalid("logical EOF before TS entity drained"));
        }
        self.logical_end = true;
        Ok(())
    }
    fn apply_pat(&mut self, table: &PatTable) -> Result<()> {
        let programs = table.programs.as_slice();
        let entry = if self.discover_program {
            let mut media = programs.iter().filter(|entry| entry.program_number != 0);
            let entry = media
                .next()
                .ok_or(Error::Invalid("segment PAT has no media program"))?;
            if media.next().is_some() {
                return Err(Error::Unsupported(
                    "segment PAT has multiple media programs",
                ));
            }
            if self.program != 0 && self.program != entry.program_number {
                return Err(Error::Unsupported("segment PAT changed media program"));
            }
            Some(entry)
        } else {
            programs
                .iter()
                .find(|entry| entry.program_number == self.program)
        };
        let pid = entry.map(|entry| entry.pid);
        if self.pmt.as_ref().map(|pmt| pmt.pid) != pid || self.tsid != Some(table.header.extension)
        {
            let epoch = self.config_epoch.checked_add(1).ok_or(Error::Overflow)?;
            let pmt = pid
                .map(|pid| PsiRoute::new(pid, &self.budget, self.limits))
                .transpose()?;
            self.pmt = pmt;
            self.routes = BudgetVec::new(&self.budget);
            self.pmt_source = None;
            self.pmt_version = None;
            self.pcr_pid = None;
            self.clock_continuity = None;
            self.clock_pending = false;
            self.clock_signal = false;
            self.clock_samples = None;
            self.config_epoch = epoch;
        }
        if let Some(entry) = entry {
            self.program = entry.program_number;
        }
        self.tsid = Some(table.header.extension);
        Ok(())
    }
    fn poll_map(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<TraceEvent>> {
        let Some(build) = self.map_build.as_mut() else {
            return Ok(None);
        };
        while build.position < build.map.streams.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(None);
            }
            let stream = &build.map.streams.as_slice()[build.position];
            let reuse = self.routes.as_slice().iter().position(|old| {
                old.pid == stream.pid
                    && old.stream_type == stream.stream_type
                    && old.descriptors.as_ref() == stream.descriptor_bytes.as_ref()
            });
            let epochs = SourceEpochs {
                clock: self.clock_epoch,
                configuration: build.epoch,
            };
            let assembler = if reuse.is_some() {
                None
            } else {
                Some(PesAssembler::new(
                    stream.pid,
                    is_video_stream_type(stream.stream_type),
                    self.budget.clone(),
                    self.limits,
                    epochs,
                )?)
            };
            build
                .routes
                .try_push(PesRoute {
                    pid: stream.pid,
                    stream_type: stream.stream_type,
                    descriptors: stream.descriptor_bytes.clone(),
                    epochs,
                    continuity: Some(Continuity::new(stream.pid)?),
                    assembler,
                    reuse,
                })
                .map_err(|error| error.error)?;
            build.position += 1;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        // Atomic commit: all allocations and validation precede these bounded moves.
        let mut build = self
            .map_build
            .take()
            .ok_or(Error::Invalid("PMT transaction missing"))?;
        for route in build.routes.as_mut_slice() {
            if let Some(index) = route.reuse.take() {
                let previous = &mut self.routes.as_mut_slice()[index];
                route.assembler = previous.assembler.take();
                route.continuity = previous.continuity.take();
                route.epochs = previous.epochs;
            }
        }
        let pcr = if build.map.pcr_pid == 0x1fff {
            None
        } else {
            Some(build.map.pcr_pid)
        };
        let clock = match pcr {
            Some(pid) => Some(Continuity::new(pid)?),
            None => None,
        };
        let binding_changed = self.pcr_pid != pcr;
        let pending_rebind = if binding_changed && self.clock_pending {
            Some(TraceFault {
                kind: FaultKind::Program,
                pid: self.pcr_pid,
                location: self.location(),
                error: Error::Invalid("pending clock signal changed PCR binding"),
                rejected: None,
            })
        } else {
            None
        };
        self.routes = build.routes;
        self.pmt_source = Some(build.map.source.clone());
        self.pmt_version = Some(build.map.header.version);
        self.pcr_pid = pcr;
        self.clock_continuity = clock;
        self.config_epoch = build.epoch;
        if binding_changed {
            self.clock_pending = false;
            self.clock_signal = false;
        }
        if let Some(fault) = pending_rebind {
            self.clock_quarantined = true;
            // poll_jobs emits this immediately after the committed raw PMT event.
            self.fault = Some(fault);
        }
        Ok(Some(TraceEvent::ProgramMap {
            map: build.map,
            active: true,
            configuration_epoch: build.epoch,
        }))
    }
    fn poll_jobs(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<TraceEvent>> {
        if let Some(fault) = self.fault.take() {
            return Ok(Some(TraceEvent::Diagnostic(fault)));
        }
        if self.map_build.is_some() {
            return self.poll_map(work, cancel);
        }
        if let Some(section) = self.section.as_ref() {
            if self.feed_omissions.is_some()
                && matches!(self.route, Route::Pmt)
                && section.crc_remainder() == Some(0)
                && self
                    .pmt_source
                    .as_ref()
                    .is_some_and(|current| current.as_ref() == section.bytes().as_ref())
            {
                // An identical CRC-checked section already passed the PMT
                // parser in this program and descriptor namespace.
                let section = self
                    .section
                    .take()
                    .ok_or(Error::Invalid("PSI source missing"))?;
                return Ok(Some(TraceEvent::Section(section)));
            }
            let job = match self.route {
                Route::Pat => PatReader::new(section, &self.budget, self.limits).map(Job::Pat),
                Route::Pmt => PmtReader::new_with_namespace(
                    section,
                    &self.budget,
                    self.limits,
                    self.descriptor_namespace,
                )
                .map(Job::Pmt),
                _ => return Err(Error::Invalid("PSI source without PSI route")),
            };
            match job {
                Ok(job) => self.job = Some(job),
                Err(error) if retryable(error) => return Err(error),
                Err(error) => {
                    self.fault = Some(TraceFault {
                        kind: FaultKind::Psi,
                        pid: Some(section.pid()),
                        location: self.location(),
                        error,
                        rejected: None,
                    });
                }
            }
            let section = self
                .section
                .take()
                .ok_or(Error::Invalid("PSI source missing"))?;
            return Ok(Some(TraceEvent::Section(section)));
        }
        if let Some(job) = self.job.as_mut() {
            match job {
                Job::Pat(reader) => match reader.poll(work, cancel) {
                    Ok(TableStep::Output(section)) => {
                        self.job = None;
                        self.collector
                            .submit(section)
                            .map_err(|error| error.error)?;
                    }
                    Ok(TableStep::Yield) => return Ok(None),
                    Ok(TableStep::Done) => self.job = None,
                    Err(error) if retryable(error) => return Err(error),
                    Err(error) => {
                        self.job = None;
                        return Ok(Some(self.diagnostic(FaultKind::Psi, Some(0), error)));
                    }
                },
                Job::Pmt(reader) => match reader.poll(work, cancel) {
                    Ok(TableStep::Output(map)) => {
                        self.job = None;
                        if map.header.extension != self.program {
                            return Ok(Some(self.diagnostic(
                                FaultKind::Program,
                                self.pmt.as_ref().map(|pmt| pmt.pid),
                                Error::Invalid("PMT program differs from active PAT"),
                            )));
                        }
                        if !map.header.current_next {
                            return Ok(Some(TraceEvent::ProgramMap {
                                map,
                                active: false,
                                configuration_epoch: self.config_epoch,
                            }));
                        }
                        if self
                            .pmt_source
                            .as_ref()
                            .is_some_and(|old| old.as_ref() == map.source.as_ref())
                        {
                            return Ok(None);
                        }
                        if self.pmt_version == Some(map.header.version) {
                            return Ok(Some(self.diagnostic(
                                FaultKind::Program,
                                self.pmt.as_ref().map(|pmt| pmt.pid),
                                Error::Invalid("PMT conflicting repeat"),
                            )));
                        }
                        let epoch = self.config_epoch.checked_add(1).ok_or(Error::Overflow)?;
                        self.map_build = Some(MapBuild {
                            map,
                            routes: BudgetVec::new(&self.budget),
                            position: 0,
                            epoch,
                        });
                        return self.poll_map(work, cancel);
                    }
                    Ok(TableStep::Yield) => return Ok(None),
                    Ok(TableStep::Done) => self.job = None,
                    Err(error) if retryable(error) => return Err(error),
                    Err(error) => {
                        self.job = None;
                        return Ok(Some(self.diagnostic(
                            FaultKind::Psi,
                            self.pmt.as_ref().map(|pmt| pmt.pid),
                            error,
                        )));
                    }
                },
            }
        }
        match self.collector.poll(work, cancel) {
            Ok(TableStep::Output(table)) => {
                self.apply_pat(&table)?;
                Ok(Some(TraceEvent::Pat(table)))
            }
            Ok(TableStep::Yield | TableStep::Done) => Ok(None),
            Err(error) if retryable(error) => Err(error),
            Err(error) => {
                self.collector.discard();
                Ok(Some(self.diagnostic(FaultKind::Program, Some(0), error)))
            }
        }
    }
    fn observe(&mut self, work: &mut WorkBudget) -> Result<bool> {
        if !work.try_charge(1) {
            return Ok(false);
        }
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("TS packet missing"))?;
        let outcome = match self.route {
            Route::Pat => Some(self.pat.continuity.observe(packet)?),
            Route::Pmt => Some(
                self.pmt
                    .as_mut()
                    .ok_or(Error::Invalid("PMT route missing"))?
                    .continuity
                    .observe(packet)?,
            ),
            Route::Pes(index) => Some(
                self.routes.as_mut_slice()[index]
                    .continuity
                    .as_mut()
                    .ok_or(Error::Invalid("PES continuity missing"))?
                    .observe(packet)?,
            ),
            Route::Clock => Some(
                self.clock_continuity
                    .as_mut()
                    .ok_or(Error::Invalid("PCR continuity missing"))?
                    .observe(packet)?,
            ),
            Route::Ignore => None,
        };
        let pid = packet.pid;
        let start = packet.payload_unit_start;
        let bad = if packet.transport_error {
            Some(Error::Invalid("TS transport error indicator"))
        } else if packet.scrambling != 0 {
            Some(Error::Unsupported("scrambled TS payload"))
        } else if matches!(outcome, Some(ContinuityOutcome::Lost { .. })) {
            Some(Error::Invalid("TS continuity loss"))
        } else if outcome == Some(ContinuityOutcome::ExcessDuplicate) {
            Some(Error::Invalid("TS excess duplicate"))
        } else {
            None
        };
        // The outcome describes an actual counter break, not the raw flag.
        // Continuous flagged PSI/PES can continue; PCR clock transitions are
        // evaluated separately below against the selected PCR PID and raw signal.
        let counter_break = outcome == Some(ContinuityOutcome::SignalledDiscontinuity);
        if bad.is_some() || counter_break {
            self.discard_route(self.route);
        }
        if let Some(error) = bad {
            let kind = if matches!(
                outcome,
                Some(ContinuityOutcome::Lost { .. } | ContinuityOutcome::ExcessDuplicate)
            ) {
                FaultKind::Continuity
            } else {
                FaultKind::Transport
            };
            self.fault = Some(TraceFault {
                kind,
                pid: Some(pid),
                location: self.location(),
                error,
                rejected: None,
            });
            self.phase = if kind == FaultKind::Continuity
                && start
                && outcome != Some(ContinuityOutcome::ExcessDuplicate)
            {
                2
            } else {
                7
            };
        } else if outcome == Some(ContinuityOutcome::Duplicate) {
            self.phase = 7;
        } else {
            self.phase = 2;
        }
        self.outcome = outcome;
        Ok(true)
    }

    fn validate_feed_continuation(
        &mut self,
        carrier: &FramedCarrier,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        let pid = carrier.pid();
        if self.feed_omissions.is_none() || (self.pcr_pid == Some(pid) && self.clock_pending) {
            return Ok(false);
        }
        let Route::Pes(index) = self.route else {
            return Ok(false);
        };
        let (counter, payload_len) = match carrier {
            FramedCarrier::Packet(packet) => {
                if packet.payload_unit_start
                    || packet.discontinuity_indicator()
                    || packet.transport_error
                    || packet.scrambling != 0
                    || (self.pcr_pid == Some(pid)
                        && packet.adaptation.as_ref().is_some_and(|a| a.pcr.is_some()))
                {
                    return Ok(false);
                }
                let Some(payload) = packet.payload.as_ref() else {
                    return Ok(false);
                };
                (packet.continuity_counter, payload.len())
            }
            FramedCarrier::Plain(packet) => (packet.counter(), 188 - packet.payload_start()),
        };
        let route = &mut self.routes.as_mut_slice()[index];
        let continuity = route
            .continuity
            .as_mut()
            .ok_or(Error::Invalid("PES continuity missing"))?;
        if payload_len == 0 || !continuity.expects_payload_counter(counter) {
            return Ok(false);
        }
        let assembler = route
            .assembler
            .as_mut()
            .ok_or(Error::Invalid("PES route missing"))?;
        let Some(completed) = assembler.validate_body_carrier(payload_len, work)? else {
            return Ok(false);
        };
        match carrier {
            FramedCarrier::Packet(packet) => continuity.record_continuation(packet),
            FramedCarrier::Plain(packet) => {
                let source = packet
                    .source
                    .as_ref()
                    .ok_or(Error::Invalid("missing continuation source"))?;
                continuity.record_plain_continuation(source.as_ref(), counter);
            }
        }
        if self.pcr_pid == Some(pid) {
            self.clock_signal = false;
        }
        // Transport is observed first in the full trace. Count continuity
        // only when the consumer asks for the next source event.
        self.deferred_feed_events = 1 + u64::from(completed);
        Ok(true)
    }

    fn try_feed_payload_run(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: &mut impl FnMut(
            FeedItem<'_>,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<bool> {
        if work.remaining() <= 2 * PAYLOAD_RUN_WORK
            || input.buffered().len() < 2 * TS188_BYTES
            || !self.framer.retained_payload_window(input)
        {
            return Ok(false);
        }
        let buffered = input.buffered();
        let Some((pid, counter)) = payload_only_continuation(&buffered[..TS188_BYTES]) else {
            return Ok(false);
        };
        if !matches_payload_header(
            &buffered[TS188_BYTES..2 * TS188_BYTES],
            pid,
            (counter + 1) & 15,
        ) || (self.pcr_pid == Some(pid) && self.clock_pending)
        {
            return Ok(false);
        }
        let Route::Pes(index) = self.find_feed_route(pid) else {
            return Ok(false);
        };
        let epochs = self.source_epochs();
        let route = &mut self.routes.as_mut_slice()[index];
        let (Some(continuity), Some(assembler), Some(omissions)) = (
            route.continuity.as_mut(),
            route.assembler.as_mut(),
            self.feed_omissions.as_mut(),
        ) else {
            return Ok(false);
        };
        let maximum = assembler.body_run_capacity(PLAIN_PAYLOAD_BYTES);
        if maximum < 2 || !continuity.expects_payload_counter(counter) {
            return Ok(false);
        }
        let before = *omissions;
        let result = consume(
            FeedItem::PayloadRun(FeedPayloadRun {
                input,
                framer: &mut self.framer,
                continuity,
                assembler,
                omissions,
                cancel,
                epochs,
                pid,
                counter,
                maximum,
            }),
            before,
            epochs,
            work,
        );
        let accepted = *omissions != before;
        if accepted {
            self.entity_end = None;
            self.route = Route::Pes(index);
            if self.pcr_pid == Some(pid) {
                self.clock_signal = false;
            }
        }
        result?;
        Ok(accepted)
    }

    fn clock_transition(&mut self) -> Result<Option<TraceEvent>> {
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("TS packet missing"))?;
        if self.pcr_pid != Some(packet.pid) {
            return Ok(None);
        }
        let signalled = packet.discontinuity_indicator();
        let pid = packet.pid;
        let has_pcr = packet
            .adaptation
            .as_ref()
            .is_some_and(|field| field.pcr.is_some());
        if self.clock_quarantined {
            self.clock_signal = signalled;
            return Ok(None);
        }
        if self.clock_pending && !signalled {
            self.clock_signal = false;
            return Ok(Some(
                self.quarantine_clock(pid, "PCR discontinuity signal dropped"),
            ));
        }
        if signalled && !self.clock_signal {
            self.clock_pending = true;
        }
        self.clock_signal = signalled;
        if !has_pcr {
            return Ok(None);
        }
        if self.clock_pending {
            if self.clock_samples == Some(1) {
                return Ok(Some(self.quarantine_clock(
                    pid,
                    "clock discontinuity before two PCR samples",
                )));
            }
            self.clock_epoch = self.clock_epoch.checked_add(1).ok_or(Error::Overflow)?;
            self.clock_pending = false;
            self.clock_samples = Some(1);
            for route in self.routes.as_mut_slice() {
                route.epochs.clock = self.clock_epoch;
                if let Some(assembler) = route.assembler.as_mut() {
                    assembler.discard(route.epochs);
                }
            }
            return Ok(Some(TraceEvent::Timeline {
                program: self.program,
                clock_epoch: self.clock_epoch,
            }));
        }
        if self.clock_samples.is_some() {
            self.clock_samples = Some(2);
        }
        Ok(None)
    }
    fn quarantine_clock(&mut self, pid: u16, reason: &'static str) -> TraceEvent {
        self.clock_pending = false;
        self.clock_quarantined = true;
        self.diagnostic(FaultKind::Transport, Some(pid), Error::Invalid(reason))
    }
    fn feed(&mut self, work: &mut WorkBudget) -> Result<()> {
        let packet = self
            .packet
            .as_mut()
            .ok_or(Error::Invalid("TS packet missing"))?;
        let Some(payload) = packet.payload.as_ref() else {
            self.phase = 7;
            return Ok(());
        };
        let entity = packet.source_id.ok_or(Error::SourceMismatch)?;
        let pid = packet.pid;
        let pusi = packet.payload_unit_start;
        let offset = packet.payload_source_offset;
        if self.feed_omissions.is_some()
            && pusi
            && work.remaining() >= payload.len() as u64
            && let [0, section_and_stuffing @ ..] = payload.as_ref()
        {
            let accepted = match self.route {
                Route::Pat if self.pat.assembler.idle() => self.collector.active_single_section(),
                Route::Pmt if self.pmt.as_ref().is_some_and(|pmt| pmt.assembler.idle()) => {
                    self.pmt_source.as_ref()
                }
                _ => None,
            };
            if let Some(section) = accepted
                && let Some(stuffing) = section_and_stuffing.strip_prefix(section.as_ref())
                && stuffing.iter().all(|&byte| byte == 0xff)
            {
                // These exact section bytes already passed CRC and table
                // validation. There is no other table in this carrier.
                work.take(payload.len() as u64);
                self.omit_feed_events(1)?;
                self.phase = 7;
                return Ok(());
            }
        }
        let payload = &mut packet.payload;
        let pushed = match self.route {
            Route::Pat => self.pat.assembler.push(
                payload.take().ok_or(Error::Invalid("TS payload missing"))?,
                pusi,
                entity,
                offset,
            ),
            Route::Pmt => self
                .pmt
                .as_mut()
                .ok_or(Error::Invalid("PMT route missing"))?
                .assembler
                .push(
                    payload.take().ok_or(Error::Invalid("TS payload missing"))?,
                    pusi,
                    entity,
                    offset,
                ),
            Route::Pes(index) => self.routes.as_mut_slice()[index]
                .assembler
                .as_mut()
                .ok_or(Error::Invalid("PES route missing"))?
                .push(
                    payload.take().ok_or(Error::Invalid("TS payload missing"))?,
                    pusi,
                    entity,
                    offset,
                ),
            Route::Clock | Route::Ignore => {
                self.phase = 7;
                return Ok(());
            }
        };
        let pushed = pushed.map_err(|rejected| {
            *payload = Some(rejected.value);
            rejected.error
        });
        match pushed {
            Ok(()) => self.phase = 6,
            Err(error)
                if matches!(self.route, Route::Pat | Route::Pmt)
                    && error == Error::Invalid("PSI pointer") =>
            {
                // This wire-syntax fault is rejected by push before acceptance.
                // Use the same explicit source-diagnostic recovery as poll faults;
                // invariant/ownership/resource errors still propagate unchanged.
                self.discard_route(self.route);
                self.fault = Some(TraceFault {
                    kind: FaultKind::Psi,
                    pid: Some(pid),
                    location: self.location(),
                    error,
                    rejected: None,
                });
                self.phase = 7;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }
    fn drain(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<TraceEvent>> {
        let result = match self.route {
            Route::Pat => self
                .pat
                .assembler
                .poll(work, cancel)
                .map(|step| match step {
                    AssemblyStep::Output(section) => {
                        self.section = Some(section);
                        false
                    }
                    AssemblyStep::Yield => false,
                    _ => true,
                }),
            Route::Pmt => self
                .pmt
                .as_mut()
                .ok_or(Error::Invalid("PMT route missing"))?
                .assembler
                .poll(work, cancel)
                .map(|step| match step {
                    AssemblyStep::Output(section) => {
                        self.section = Some(section);
                        false
                    }
                    AssemblyStep::Yield => false,
                    _ => true,
                }),
            Route::Pes(index) => {
                let assembler = self.routes.as_mut_slice()[index]
                    .assembler
                    .as_mut()
                    .ok_or(Error::Invalid("PES route missing"))?;
                let step = if self.feed_omissions.is_some() {
                    assembler.poll_source::<false>(work, cancel)
                } else {
                    assembler.poll_source::<true>(work, cancel)
                };
                match step {
                    Ok(AssemblyStep::Output(Some(pes))) => return Ok(Some(TraceEvent::Pes(pes))),
                    Ok(AssemblyStep::Output(None)) => {
                        self.omit_feed_events(1)?;
                        Ok(false)
                    }
                    Ok(AssemblyStep::Yield) => Ok(false),
                    Ok(_) => Ok(true),
                    Err(error) => Err(error),
                }
            }
            Route::Clock | Route::Ignore => Ok(true),
        };
        match result {
            Ok(true) => {
                self.phase = 7;
                Ok(None)
            }
            Ok(false) => Ok(None),
            Err(error) if retryable(error) => Err(error),
            Err(error) => {
                let pid = self.packet.as_ref().map(|packet| packet.pid);
                let kind = if matches!(self.route, Route::Pes(_)) {
                    FaultKind::Pes
                } else {
                    FaultKind::Psi
                };
                self.discard_route(self.route);
                self.phase = 7;
                Ok(Some(self.diagnostic(kind, pid, error)))
            }
        }
    }
    fn eof(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<TraceEvent>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.eof_route == 0 || self.eof_route == 1 {
                let state = if self.eof_route == 0 {
                    Some(&mut self.pat)
                } else {
                    self.pmt.as_mut()
                };
                if let Some(state) = state {
                    state.assembler.end_of_source();
                    let pid = state.pid;
                    match state.assembler.poll(work, cancel) {
                        Ok(AssemblyStep::Yield) => return Ok(Step::Yield),
                        Err(error) if retryable(error) => return Err(error),
                        Err(error) => {
                            state.assembler.discard();
                            self.eof_route += 1;
                            return Ok(Step::Ready(self.diagnostic(
                                FaultKind::Psi,
                                Some(pid),
                                error,
                            )));
                        }
                        Ok(AssemblyStep::Output(_)) => {
                            return Err(Error::Invalid("undrained PSI at logical EOF"));
                        }
                        Ok(_) => {}
                    }
                }
                self.eof_route += 1;
                continue;
            }
            let index = self.eof_route - 2;
            if index == self.routes.len() {
                self.finished = true;
                return Ok(Step::Ready(TraceEvent::Finished));
            }
            let route = &mut self.routes.as_mut_slice()[index];
            let pid = route.pid;
            let assembler = route
                .assembler
                .as_mut()
                .ok_or(Error::Invalid("PES route missing at EOF"))?;
            assembler.end_of_source();
            let step = if self.feed_omissions.is_some() {
                assembler.poll_source::<false>(work, cancel)
            } else {
                assembler.poll_source::<true>(work, cancel)
            };
            match step {
                Ok(AssemblyStep::Output(Some(pes))) => {
                    return Ok(Step::Ready(TraceEvent::Pes(pes)));
                }
                Ok(AssemblyStep::Output(None)) => self.omit_feed_events(1)?,
                Ok(AssemblyStep::Yield) => return Ok(Step::Yield),
                Err(error) if retryable(error) => return Err(error),
                Err(error) => {
                    assembler.discard(route.epochs);
                    self.eof_route += 1;
                    return Ok(Step::Ready(self.diagnostic(
                        FaultKind::Pes,
                        Some(pid),
                        error,
                    )));
                }
                Ok(_) => self.eof_route += 1,
            }
        }
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<TraceEvent>> {
        self.poll_feed(input, work, cancel, &mut |_, _, _, _| {
            Ok(FeedContinuation::Emit)
        })
    }

    /// Offer stable PES continuations to a feed before publishing a trace event.
    /// The callback receives the number of preceding events omitted in this poll
    /// window and the current source epochs. Buffered consumes the carrier,
    /// and Yield consumes it and returns control to the feed. Both advance
    /// the omitted count. Emit and errors must leave the carrier unchanged.
    /// After an error, the carrier is retained for ordinary transport delivery.
    pub fn poll_feed(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: &mut impl FnMut(
            &mut TransportPacket,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<Step<TraceEvent>> {
        self.poll_feed_inner::<false, false>(input, work, cancel, |item, omitted, epochs, work| {
            let FeedItem::Carrier(carrier) = item else {
                unreachable!("literal carrier delivery");
            };
            match carrier.0 {
                FramedCarrier::Packet(packet) => consume(packet, omitted, epochs, work),
                FramedCarrier::Plain(_) => unreachable!("literal carrier delivery"),
            }
        })
    }

    /// Like poll_feed, with compact carriers for ordinary progressive TS188
    /// continuations. Emit and errors retain the full transport event.
    pub fn poll_feed_carriers(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: &mut impl FnMut(
            FeedCarrier<'_>,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<Step<TraceEvent>> {
        let mut single = |item: FeedItem<'_>, omitted, epochs, work: &mut WorkBudget| {
            let FeedItem::Carrier(carrier) = item else {
                unreachable!("single carrier delivery");
            };
            consume(carrier, omitted, epochs, work)
        };
        if self.feed_validated_carriers {
            self.poll_feed_inner::<true, false>(input, work, cancel, &mut single)
        } else {
            self.poll_feed_inner::<false, false>(input, work, cancel, &mut single)
        }
    }

    /// Like poll_feed_carriers, with optional shared-window runs for progressive
    /// feeds. A run is accepted by FeedPayloadRun::append_ready. The returned
    /// FeedContinuation applies to ordinary carriers.
    pub fn poll_feed_with_payload_runs(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: &mut impl FnMut(
            FeedItem<'_>,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<Step<TraceEvent>> {
        if self.feed_validated_carriers {
            self.poll_feed_inner::<true, true>(input, work, cancel, consume)
        } else {
            self.poll_feed_inner::<false, false>(input, work, cancel, consume)
        }
    }

    fn poll_feed_inner<const DIRECT: bool, const RUNS: bool>(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        consume: impl FnMut(
            FeedItem<'_>,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<Step<TraceEvent>> {
        self.poll_inner::<TraceEvent, DIRECT, RUNS>(input, work, cancel, consume)
    }

    pub fn poll_compact(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<CompactTraceEvent>> {
        self.poll_inner::<CompactTraceEvent, false, false>(input, work, cancel, |_, _, _, _| {
            Ok(FeedContinuation::Emit)
        })
    }

    fn poll_inner<T: PollOutput, const DIRECT: bool, const RUNS: bool>(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        mut consume: impl FnMut(
            FeedItem<'_>,
            u64,
            SourceEpochs,
            &mut WorkBudget,
        ) -> Result<FeedContinuation>,
    ) -> Result<Step<T>> {
        let ready = |event| Step::Ready(T::event(event));
        if self.finished {
            return Err(Error::Invalid("TS stream already finished"));
        }
        'events: loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.deferred_feed_events != 0 {
                if self.phase == FEED_FALLBACK_PHASE {
                    if work.remaining() == 0 {
                        return Ok(Step::Yield);
                    }
                    let packet = self
                        .packet
                        .take()
                        .ok_or(Error::Invalid("TS packet missing"))?;
                    self.phase = 0;
                    return Ok(ready(TraceEvent::Transport(packet)));
                }
                self.omit_feed_events(self.deferred_feed_events)?;
                self.deferred_feed_events = 0;
            }
            if self.boundary_event {
                self.boundary_event = false;
                return Ok(ready(TraceEvent::Discontinuity {
                    epochs: self.source_epochs(),
                }));
            }
            if self.fault.is_some()
                || self.map_build.is_some()
                || self.section.is_some()
                || self.job.is_some()
                || self.collector.has_pending()
            {
                if let Some(event) = self.poll_jobs(work, cancel)? {
                    return Ok(ready(event));
                }
                if self.job.is_some() || self.section.is_some() || self.map_build.is_some() {
                    return Ok(Step::Yield);
                }
            } else if cancel.load(Ordering::Relaxed) {
                // An idle collector still checks cancellation before returning Done.
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            if self.packet.is_some() {
                match self.phase {
                    1 => {
                        if !self.observe(work)? {
                            return Ok(Step::Yield);
                        }
                        let outcome = self.outcome.take();
                        if self.feed_validated_carriers {
                            let packet = self
                                .packet
                                .as_ref()
                                .ok_or(Error::Invalid("TS packet missing"))?;
                            match outcome {
                                Some(ContinuityOutcome::First | ContinuityOutcome::Continuous)
                                    if matches!(self.route, Route::Pes(_)) && self.phase == 2 =>
                                {
                                    // Count the hidden continuity event after the
                                    // carrier so its full-trace ordinal is retained.
                                    self.deferred_feed_events = 1;
                                    return Ok(ready(TraceEvent::Transport(packet.clone())));
                                }
                                Some(ContinuityOutcome::SignalledDiscontinuity) => {
                                    let pid = packet.pid;
                                    self.omit_feed_events(1)?;
                                    return Ok(ready(TraceEvent::Continuity {
                                        pid,
                                        outcome: ContinuityOutcome::SignalledDiscontinuity,
                                    }));
                                }
                                Some(_) => self.omit_feed_events(2)?,
                                None => self.omit_feed_events(1)?,
                            }
                            continue;
                        }
                        if let Some(outcome) = outcome {
                            if self.feed_omissions.is_some() {
                                self.omit_feed_events(1)?;
                                continue;
                            }
                            return Ok(ready(TraceEvent::Continuity {
                                pid: self
                                    .packet
                                    .as_ref()
                                    .ok_or(Error::Invalid("TS packet missing"))?
                                    .pid,
                                outcome,
                            }));
                        }
                    }
                    2 => {
                        self.phase = 5;
                        if let Some(event) = self.clock_transition()? {
                            return Ok(ready(event));
                        }
                    }
                    5 => self.feed(work)?,
                    6 => {
                        if let Some(event) = self.drain(work, cancel)? {
                            return Ok(ready(event));
                        }
                    }
                    7 => {
                        self.packet = None;
                        self.phase = 0;
                    }
                    _ => return Err(Error::Invalid("TS packet stage")),
                }
                continue;
            }
            if self.logical_end {
                return match self.eof(work, cancel)? {
                    Step::Ready(event) => Ok(ready(event)),
                    Step::NeedInput(request) => Ok(Step::NeedInput(request)),
                    Step::Yield => Ok(Step::Yield),
                    Step::End(location) => Ok(Step::End(location)),
                };
            }
            if T::COMPACT
                && matches!(self.find_route(0x1fff), Route::Ignore)
                && let Some(event) = T::null_run(&mut self.framer, input, work, cancel)?
            {
                self.entity_end = None;
                return Ok(Step::Ready(event));
            }
            // Reading carriers advances the cursor within the same source
            // request. The feed callback never receives the cursor.
            self.framer.bind_input(input)?;
            loop {
                if RUNS && self.try_feed_payload_run(input, work, cancel, &mut consume)? {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    continue;
                }
                match self
                    .framer
                    .poll_bound_carrier::<DIRECT>(input, work, cancel)
                {
                    Ok(Step::Ready(mut carrier)) => {
                        self.entity_end = None;
                        self.route = if DIRECT {
                            self.find_feed_route(carrier.pid())
                        } else {
                            self.find_route(carrier.pid())
                        };
                        if self.validate_feed_continuation(&carrier, work)? {
                            match consume(
                                FeedItem::Carrier(FeedCarrier(&mut carrier)),
                                self.feed_omissions.unwrap_or(0),
                                self.source_epochs(),
                                work,
                            ) {
                                Ok(FeedContinuation::Emit) => {}
                                Ok(FeedContinuation::Buffered) => {
                                    self.omit_feed_events(1)?;
                                    if cancel.load(Ordering::Relaxed) {
                                        return Err(Error::Cancelled);
                                    }
                                    // A validated continuation never starts a table or
                                    // clock update. Resume framing at the next carrier.
                                    let deferred = std::mem::take(&mut self.deferred_feed_events);
                                    self.omit_feed_events(deferred)?;
                                    if work.remaining() == 0 {
                                        return Ok(Step::Yield);
                                    }
                                    continue;
                                }
                                Ok(FeedContinuation::Yield) => {
                                    self.omit_feed_events(1)?;
                                    return Ok(Step::Yield);
                                }
                                Err(error) => {
                                    self.packet = Some(carrier.into_packet()?);
                                    self.phase = FEED_FALLBACK_PHASE;
                                    return Err(error);
                                }
                            }
                            return Ok(ready(TraceEvent::Transport(carrier.into_packet()?)));
                        }
                        let packet = carrier.into_packet()?;
                        self.phase = 1;
                        if self.feed_validated_carriers {
                            self.packet = Some(packet);
                            continue 'events;
                        }
                        let event = TraceEvent::Transport(packet.clone());
                        self.packet = Some(packet);
                        return Ok(ready(event));
                    }
                    Ok(Step::NeedInput(request)) => return Ok(Step::NeedInput(request)),
                    Ok(Step::Yield) => return Ok(Step::Yield),
                    Ok(Step::End(location)) => {
                        self.entity_end = Some(location);
                        return Ok(Step::End(location));
                    }
                    Err(error) if matches!(error, Error::Invalid(_) | Error::Truncated) => {
                        let location = SourceLocation {
                            source: input.source(),
                            offset: self.framer.pending_or_last_packet_offset().unwrap_or_else(
                                || input.position().saturating_sub(self.format.bytes() as u64),
                            ),
                        };
                        let rejected = self.framer.take_rejection();
                        self.framer.discard();
                        self.pat.assembler.discard();
                        if let Some(pmt) = self.pmt.as_mut() {
                            pmt.assembler.discard();
                        }
                        for route in self.routes.as_mut_slice() {
                            if let Some(assembler) = route.assembler.as_mut() {
                                assembler.discard(route.epochs);
                            }
                        }
                        return Ok(ready(TraceEvent::Diagnostic(TraceFault {
                            kind: FaultKind::Transport,
                            pid: None,
                            location,
                            error,
                            rejected,
                        })));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

/// Video stream types, whose PES packets may leave `PES_packet_length` 0
/// (ISO/IEC 13818-1 2.4.3.7): MPEG-1 and MPEG-2 video, H.264 and HEVC.
fn is_video_stream_type(stream_type: u8) -> bool {
    matches!(stream_type, 0x01 | 0x02 | 0x1b | 0x24)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod dispatch_tests {
    use super::*;

    fn stream() -> TransportStream {
        let pool = Budget::new(64 * 1024).unwrap();
        let limits = ParseLimits::default();
        let mut stream = TransportStream::new_single_program_progressive_feed(
            pool.clone(),
            limits,
            DescriptorNamespace::Unspecified,
        )
        .unwrap();
        stream.pmt = Some(PsiRoute::new(100, &pool, limits).unwrap());
        stream.pcr_pid = Some(300);
        for pid in [256, 257] {
            stream
                .routes
                .try_push(PesRoute {
                    pid,
                    stream_type: 0x1b,
                    descriptors: Bytes::copy_from_slice(&pool, &[]).unwrap(),
                    epochs: stream.source_epochs(),
                    continuity: Some(Continuity::new(pid).unwrap()),
                    assembler: None,
                    reuse: None,
                })
                .unwrap();
        }
        stream
    }

    #[test]
    fn selected_feed_route_preserves_control_and_interleaved_pid_dispatch() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TransportStream>();
        let mut stream = stream();
        let expected = [
            (0, Route::Pat),
            (100, Route::Pmt),
            (256, Route::Pes(0)),
            (257, Route::Pes(1)),
            (300, Route::Clock),
            (0x1fff, Route::Ignore),
        ];
        for previous in [Route::Ignore, Route::Pmt, Route::Pes(0), Route::Pes(1)] {
            stream.route = previous;
            for (pid, expected) in expected {
                let actual = stream.find_feed_route(pid);
                assert_eq!(actual, expected);
                stream.route = actual;
                assert_eq!(stream.find_feed_route(pid), expected);
            }
        }
    }

    #[test]
    fn selected_feed_route_rechecks_replaced_and_cleared_tables() {
        let mut stream = stream();
        stream.route = Route::Pes(1);
        stream.routes.as_mut_slice().swap(0, 1);
        assert_eq!(stream.find_feed_route(257), Route::Pes(0));
        assert_eq!(stream.find_feed_route(256), Route::Pes(1));
        stream.route = Route::Pes(1);
        stream.routes.clear();
        assert_eq!(stream.find_feed_route(256), Route::Ignore);
        assert_eq!(stream.find_feed_route(100), Route::Pmt);
        assert_eq!(stream.find_feed_route(300), Route::Clock);
    }
}
