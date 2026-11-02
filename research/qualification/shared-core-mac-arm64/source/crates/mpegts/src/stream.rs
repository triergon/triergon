//! Composed selected-program TS -> PSI -> PES source operation. No codec AU work.
use crate::psi::{
    DescriptorNamespace, PatCollector, PatReader, PatTable, PmtReader, ProgramMap, Section,
    TableStep,
};
use crate::{
    AssemblyStep, Continuity, ContinuityOutcome, PacketFormat, ParseLimits, Pes, PesAssembler,
    SectionAssembler, SourceEpochs, TransportFramer, TransportPacket,
};
use safe_media_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, Result, SourceId, SourceLocation, Step,
    WorkBudget,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    Transport,
    Continuity,
    Psi,
    Pes,
    Program,
}
#[derive(Clone, Copy, Debug)]
pub struct TraceFault {
    pub kind: FaultKind,
    pub pid: Option<u16>,
    pub location: SourceLocation,
    pub error: Error,
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
#[derive(Clone, Copy, Debug)]
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

/// The selected program is explicit and part of operation identity. All PAT
/// associations and raw transport/section facts remain visible; other programs'
/// PES are outside this named operation, not silently counted as selected output.
#[derive(Debug)]
pub struct TransportStream {
    budget: Budget,
    format: PacketFormat,
    limits: ParseLimits,
    program: u16,
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
        limits.validate()?;
        Ok(Self {
            framer: TransportFramer::new(budget.clone(), format),
            pat: PsiRoute::new(0, &budget, limits)?,
            collector: PatCollector::new(budget.clone(), limits)?,
            routes: BudgetVec::new(&budget),
            budget,
            format,
            limits,
            program,
            descriptor_namespace,
            pmt: None,
            pmt_source: None,
            pmt_version: None,
            tsid: None,
            pcr_pid: None,
            clock_continuity: None,
            clock_pending: false,
            clock_signal: false,
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
    pub fn selected_program(&self) -> u16 {
        self.program
    }
    pub fn source_epochs(&self) -> SourceEpochs {
        SourceEpochs {
            clock: self.clock_epoch,
            configuration: self.config_epoch,
        }
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
        self.boundary_event = true;
        Ok(())
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
        let pid = table
            .programs
            .as_slice()
            .iter()
            .find(|entry| entry.program_number == self.program)
            .map(|entry| entry.pid);
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
            self.config_epoch = epoch;
        }
        self.tsid = Some(table.header.extension);
        Ok(())
    }
    fn poll_map(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<TraceEvent>> {
        let build = match self.map_build.as_mut() {
            Some(build) => build,
            None => return Ok(None),
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
                    matches!(stream.stream_type, 0x1b | 0x24),
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
        self.routes = build.routes;
        self.pmt_source = Some(build.map.source.clone());
        self.pmt_version = Some(build.map.header.version);
        self.pcr_pid = pcr;
        self.clock_continuity = clock;
        self.config_epoch = build.epoch;
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
                    })
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
        let signalled = outcome == Some(ContinuityOutcome::SignalledDiscontinuity);
        if bad.is_some() || signalled {
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
    fn clock_transition(&mut self) -> Result<Option<TraceEvent>> {
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("TS packet missing"))?;
        if self.pcr_pid != Some(packet.pid) {
            return Ok(None);
        }
        let signalled = packet.discontinuity_indicator();
        if signalled && !self.clock_signal {
            self.clock_pending = true;
        }
        self.clock_signal = signalled;
        if self.clock_pending
            && packet
                .adaptation
                .as_ref()
                .is_some_and(|field| field.pcr.is_some())
        {
            self.clock_epoch = self.clock_epoch.checked_add(1).ok_or(Error::Overflow)?;
            self.clock_pending = false;
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
        Ok(None)
    }
    fn feed(&mut self) -> Result<()> {
        let packet = self
            .packet
            .as_ref()
            .ok_or(Error::Invalid("TS packet missing"))?;
        let payload = match packet.payload.as_ref() {
            Some(payload) => payload.clone(),
            None => {
                self.phase = 7;
                return Ok(());
            }
        };
        let entity = packet.source_id.ok_or(Error::SourceMismatch)?;
        let pid = packet.pid;
        let pusi = packet.payload_unit_start;
        let offset = packet.payload_source_offset;
        let pushed = match self.route {
            Route::Pat => self
                .pat
                .assembler
                .push(payload, pusi, entity, offset)
                .map_err(|error| error.error),
            Route::Pmt => self
                .pmt
                .as_mut()
                .ok_or(Error::Invalid("PMT route missing"))?
                .assembler
                .push(payload, pusi, entity, offset)
                .map_err(|error| error.error),
            Route::Pes(index) => self.routes.as_mut_slice()[index]
                .assembler
                .as_mut()
                .ok_or(Error::Invalid("PES route missing"))?
                .push(payload, pusi, entity, offset)
                .map_err(|error| error.error),
            Route::Clock | Route::Ignore => {
                self.phase = 7;
                return Ok(());
            }
        };
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
                match self.routes.as_mut_slice()[index]
                    .assembler
                    .as_mut()
                    .ok_or(Error::Invalid("PES route missing"))?
                    .poll(work, cancel)
                {
                    Ok(AssemblyStep::Output(pes)) => return Ok(Some(TraceEvent::Pes(pes))),
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
            match assembler.poll(work, cancel) {
                Ok(AssemblyStep::Output(pes)) => {
                    return Ok(Step::Ready(TraceEvent::Pes(pes)));
                }
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
        if self.finished {
            return Err(Error::Invalid("TS stream already finished"));
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.boundary_event {
                self.boundary_event = false;
                return Ok(Step::Ready(TraceEvent::Discontinuity {
                    epochs: self.source_epochs(),
                }));
            }
            if let Some(event) = self.poll_jobs(work, cancel)? {
                return Ok(Step::Ready(event));
            }
            if self.job.is_some() || self.section.is_some() || self.map_build.is_some() {
                return Ok(Step::Yield);
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
                        if let Some(outcome) = self.outcome.take() {
                            return Ok(Step::Ready(TraceEvent::Continuity {
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
                            return Ok(Step::Ready(event));
                        }
                    }
                    5 => self.feed()?,
                    6 => {
                        if let Some(event) = self.drain(work, cancel)? {
                            return Ok(Step::Ready(event));
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
                return self.eof(work, cancel);
            }
            match self.framer.poll(input, work, cancel) {
                Ok(Step::Ready(packet)) => {
                    self.entity_end = None;
                    self.route = self.find_route(packet.pid);
                    self.phase = 1;
                    let event = TraceEvent::Transport(packet.clone());
                    self.packet = Some(packet);
                    return Ok(Step::Ready(event));
                }
                Ok(Step::NeedInput(request)) => return Ok(Step::NeedInput(request)),
                Ok(Step::Yield) => return Ok(Step::Yield),
                Ok(Step::End(location)) => {
                    self.entity_end = Some(location);
                    return Ok(Step::End(location));
                }
                Err(error) if matches!(error, Error::Invalid(_) | Error::Truncated) => {
                    let location =
                        SourceLocation {
                            source: input.source(),
                            offset: self.framer.pending_or_last_packet_offset().unwrap_or_else(
                                || input.position().saturating_sub(self.format.bytes() as u64),
                            ),
                        };
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
                    return Ok(Step::Ready(TraceEvent::Diagnostic(TraceFault {
                        kind: FaultKind::Transport,
                        pid: None,
                        location,
                        error,
                    })));
                }
                Err(error) => return Err(error),
            }
        }
    }
}
