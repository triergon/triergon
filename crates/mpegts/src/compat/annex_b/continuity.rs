use crate::compat::pes::{
    DemuxClock, DemuxObserver, DemuxPacket, DemuxProfile, DemuxScope, DemuxTimeView,
    PesFaultPolicy, RegisteredCarriage, SourceContext, TransportFaultRecovery,
    TransportObservation,
};
use crate::{
    AssemblyStep, ContinuityOutcome, FaultKind, Reader, ReaderLimits, SourceEvent, TraceEvent,
    TransportPacket, TransportStream,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    BudgetVec, Charged, Error, InputCursor, InputToken, Result, SourceLocation, WorkBudget,
};

const HISTORY: usize = 16;
const UNOBSERVED: Error = Error::Unsupported("unobserved video continuity context");

/// Original evidence that admitted a continuity change. The carrier includes
/// its complete original bytes and the event retains any strict diagnostic.
#[derive(Debug)]
pub struct ContinuityEvidence {
    pub carrier: TransportPacket,
    pub carrier_context: SourceContext,
    pub event: SourceEvent,
    pub context: SourceContext,
}

/// One nonempty unframed video run at the original logical EOF.
#[derive(Debug)]
pub struct ContinuityResidue(Charged<Box<ResidueData>>);

#[derive(Debug)]
struct ResidueData {
    end: SourceLocation,
    clock: DemuxTimeView,
    chunks: BudgetVec<DemuxPacket>,
    evidence: BudgetVec<ContinuityEvidence>,
    bytes: usize,
}

impl ContinuityResidue {
    pub fn source_end(&self) -> SourceLocation {
        self.0.end
    }

    /// Compatibility time of the first contributing header. The chunks retain
    /// the original raw clocks and source epochs.
    pub fn clock(&self) -> &DemuxTimeView {
        &self.0.clock
    }

    /// Carrier-assembled input chunks in parser order, with exact payload spans,
    /// header ranges and source contexts. Chunk boundaries are demux boundaries.
    pub fn chunks(&self) -> &[DemuxPacket] {
        self.0.chunks.as_slice()
    }

    pub fn evidence(&self) -> &[ContinuityEvidence] {
        self.0.evidence.as_slice()
    }

    pub fn payload_len(&self) -> usize {
        self.0.bytes
    }

    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.chunks().iter().flat_map(DemuxPacket::payload)
    }
}

struct Carrier {
    packet: TransportPacket,
    context: SourceContext,
}

struct LastCarrier {
    carrier: Carrier,
    diagnostic: Option<Error>,
    confirmed: bool,
}

struct Route {
    scope: DemuxScope,
    pcr_pid: u16,
    carriage: RegisteredCarriage,
    token: InputToken,
    history: [Option<Carrier>; HISTORY],
    used: usize,
    replay: usize,
    last: Option<LastCarrier>,
    run: Option<Run>,
    source_finished: bool,
    ending: bool,
}

pub(super) struct SourceJob {
    pub(super) source: SourceEvent,
    pub(super) published: bool,
    effect: Option<Effect>,
}

impl SourceJob {
    pub(super) fn new(source: SourceEvent, published: bool) -> Self {
        Self {
            source,
            published,
            effect: None,
        }
    }
}

#[derive(Clone, Copy)]
enum Effect {
    Forward,
    Evidence(SourceContext),
}

pub(super) enum EofStep {
    Yield,
    Done,
    Residue(ContinuityResidue),
}

pub(super) struct Proof {
    enabled: bool,
    program: Option<u16>,
    ordinal: u64,
    route: Option<Route>,
}

impl Proof {
    pub(super) fn new() -> Self {
        Self {
            enabled: true,
            program: None,
            ordinal: 0,
            route: None,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn retains_carriers(&self) -> bool {
        self.route
            .as_ref()
            .is_some_and(|route| route.last.is_some())
    }

    pub(super) fn disable(&mut self) {
        self.enabled = false;
        self.route = None;
    }

    pub(super) fn matches(&self, event: &TraceEvent) -> bool {
        self.route
            .as_ref()
            .is_some_and(|route| route.matches(event))
    }

    pub(super) fn observe(
        &mut self,
        reader: &Reader,
        job: &mut SourceJob,
        limits: ReaderLimits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<bool>> {
        if !self.enabled {
            return Ok(Some(false));
        }
        let event = job.source.event();
        let effect = if let Some(effect) = job.effect {
            effect
        } else {
            if !work.try_charge(1) {
                return Ok(None);
            }
            let effect = self.admit(reader, event, limits)?;
            job.effect = Some(effect);
            effect
        };
        let Some(route) = &mut self.route else {
            return Ok(Some(false));
        };
        if route.drive(work, cancel)? == Progress::Yield {
            return Ok(None);
        }
        let Effect::Evidence(context) = effect else {
            return Ok(Some(false));
        };
        if !work.try_charge(1) {
            return Ok(None);
        }
        let run = route.run.as_mut().ok_or(UNOBSERVED)?;
        if matches!(event, TraceEvent::Timeline { .. }) {
            run.observer.confirm_source_timeline(event, context)?;
        }
        let last = route.last.as_ref().ok_or(UNOBSERVED)?;
        if run.evidence.len() == limits.transport.source_spans {
            return Err(UNOBSERVED);
        }
        run.evidence
            .try_push(ContinuityEvidence {
                carrier: last.carrier.packet.clone(),
                carrier_context: last.carrier.context,
                event: job.source.clone(),
                context,
            })
            .map_err(|error| error.error)?;
        Ok(Some(matches!(event, TraceEvent::Diagnostic(_))))
    }

    fn admit(
        &mut self,
        reader: &Reader,
        event: &TraceEvent,
        limits: ReaderLimits,
    ) -> Result<Effect> {
        let ordinal = self.ordinal;
        self.ordinal = ordinal.checked_add(1).ok_or(Error::Overflow)?;
        let (input, source) = reader.continuity_source().ok_or(UNOBSERVED)?;
        if let Some(route) = &mut self.route {
            return route.admit(input, source, ordinal, event, limits);
        }
        match event {
            TraceEvent::Pat(pat) => {
                let program = match pat.programs.as_slice() {
                    [one] if !one.is_network_pid() => Some(one.program_number),
                    [one, other] if one.is_network_pid() != other.is_network_pid() => {
                        Some(if one.is_network_pid() {
                            other.program_number
                        } else {
                            one.program_number
                        })
                    }
                    _ => None,
                };
                self.program =
                    program.filter(|p| limits.program.is_none_or(|selected| selected == *p));
            }
            TraceEvent::ProgramMap {
                map, active: true, ..
            } => {
                let [stream] = map.streams.as_slice() else {
                    return Err(UNOBSERVED);
                };
                let carriage = match stream.stream_type {
                    0x1b => RegisteredCarriage::Avc,
                    0x24 => RegisteredCarriage::Hevc,
                    _ => return Err(UNOBSERVED),
                };
                let source = source.ok_or(UNOBSERVED)?;
                if self.program != Some(map.header.extension) || source.clock_is_quarantined() {
                    return Err(UNOBSERVED);
                }
                self.route = Some(Route {
                    scope: DemuxScope {
                        program: map.header.extension,
                        pid: stream.pid,
                        stream_type: stream.stream_type,
                        stream_id: 0xe0,
                        entity: input.source(),
                        epochs: source.source_epochs(),
                    },
                    pcr_pid: map.pcr_pid,
                    carriage,
                    token: input.token(),
                    history: std::array::from_fn(|_| None),
                    used: 0,
                    replay: 0,
                    last: None,
                    run: None,
                    source_finished: false,
                    ending: false,
                });
            }
            TraceEvent::Diagnostic(_) => self.disable(),
            _ => (),
        }
        Ok(Effect::Forward)
    }

    pub(super) fn finish(
        &mut self,
        reader: &Reader,
        end: SourceLocation,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EofStep> {
        let Some(route) = &mut self.route else {
            return Ok(EofStep::Done);
        };
        let Some(run) = &mut route.run else {
            return Ok(EofStep::Done);
        };
        if !route.ending {
            if !work.try_charge(1) {
                return Ok(EofStep::Yield);
            }
            let (input, _) = reader.continuity_source().ok_or(UNOBSERVED)?;
            if route.token != input.token()
                || end.source != input.source()
                || end.offset != input.position()
                || !route.source_finished
            {
                return Err(UNOBSERVED);
            }
            run.observer.end_of_source()?;
            run.busy = true;
            route.ending = true;
        }
        match run.drive(work, cancel)? {
            Progress::Yield => return Ok(EofStep::Yield),
            Progress::Idle => return Err(UNOBSERVED),
            Progress::Finished => (),
        }
        if !run.nonzero {
            return Err(UNOBSERVED);
        }
        let first = run.chunks.as_slice().first().ok_or(UNOBSERVED)?;
        let header = first.time_header().ok_or(UNOBSERVED)?;
        let Some(clock) = DemuxClock::new(first.program)?.project_header(
            first.pid,
            first.source_epochs,
            Some(header),
            work,
            cancel,
        )?
        else {
            return Ok(EofStep::Yield);
        };
        let run = route.run.take().expect("finished continuity run");
        let pool = run.chunks.budget().clone();
        let residue = Charged::boxed(
            &pool,
            ResidueData {
                end,
                clock,
                chunks: run.chunks,
                evidence: run.evidence,
                bytes: run.bytes,
            },
        )?;
        Ok(EofStep::Residue(ContinuityResidue(residue)))
    }
}

impl Route {
    fn matches(&self, event: &TraceEvent) -> bool {
        let Some(last) = &self.last else { return false };
        match event {
            TraceEvent::Diagnostic(fault) => {
                fault.kind == FaultKind::Continuity
                    && fault.pid == Some(self.scope.pid)
                    && fault.location
                        == SourceLocation {
                            source: self.scope.entity,
                            offset: last.carrier.packet.source_offset,
                        }
                    && Some(fault.error) == last.diagnostic
                    && fault.rejected.is_none()
            }
            TraceEvent::Continuity {
                pid,
                outcome: ContinuityOutcome::SignalledDiscontinuity,
            } => *pid == self.scope.pid && last.carrier.packet.discontinuity_indicator(),
            TraceEvent::Timeline { program, .. } => {
                *program == self.scope.program && last.carrier.packet.discontinuity_indicator()
            }
            _ => false,
        }
    }

    fn unresolved_diagnostic(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|last| last.diagnostic.is_some() && !last.confirmed)
    }

    fn admit(
        &mut self,
        input: &InputCursor,
        source: Option<&TransportStream>,
        ordinal: u64,
        event: &TraceEvent,
        limits: ReaderLimits,
    ) -> Result<Effect> {
        if self.token != input.token() || input.source() != self.scope.entity {
            return Err(UNOBSERVED);
        }
        if matches!(event, TraceEvent::Finished) {
            if self.unresolved_diagnostic() {
                return Err(UNOBSERVED);
            }
            self.source_finished = true;
            return Ok(Effect::Forward);
        }
        let source = source.ok_or(UNOBSERVED)?;
        if source.clock_is_quarantined() {
            return Err(UNOBSERVED);
        }
        let context = SourceContext {
            source_event_ordinal: ordinal,
            epochs: source.source_epochs(),
            byte_run_epoch: None,
        };
        match event {
            TraceEvent::Transport(packet) => {
                if self.unresolved_diagnostic() {
                    return Err(UNOBSERVED);
                }
                if packet.pid == self.scope.pid {
                    self.carrier(input, packet, context)?;
                }
            }
            TraceEvent::Continuity { pid, outcome } if *pid == self.scope.pid => {
                let diagnostic = match outcome {
                    ContinuityOutcome::Lost { .. } => Some(Error::Invalid("TS continuity loss")),
                    ContinuityOutcome::ExcessDuplicate => {
                        Some(Error::Invalid("TS excess duplicate"))
                    }
                    ContinuityOutcome::Duplicate | ContinuityOutcome::SignalledDiscontinuity => {
                        None
                    }
                    _ => return Ok(Effect::Forward),
                };
                self.last.as_mut().ok_or(UNOBSERVED)?.diagnostic = diagnostic;
                self.activate(input, limits)?;
                return Ok(Effect::Evidence(context));
            }
            TraceEvent::Diagnostic(_) if self.matches(event) && self.unresolved_diagnostic() => {
                self.last.as_mut().expect("matching carrier").confirmed = true;
                return Ok(Effect::Evidence(context));
            }
            TraceEvent::Timeline { .. } if self.matches(event) => {
                self.activate(input, limits)?;
                return Ok(Effect::Evidence(context));
            }
            TraceEvent::Pat(_)
            | TraceEvent::ProgramMap { active: true, .. }
            | TraceEvent::Diagnostic(_)
            | TraceEvent::Timeline { .. }
            | TraceEvent::Discontinuity { .. } => return Err(UNOBSERVED),
            _ => (),
        }
        Ok(Effect::Forward)
    }

    fn carrier(
        &mut self,
        input: &InputCursor,
        packet: &TransportPacket,
        context: SourceContext,
    ) -> Result<()> {
        let end = packet
            .source_offset
            .checked_add(packet.source.len() as u64)
            .ok_or(Error::Overflow)?;
        if packet.transport_error
            || packet.scrambling != 0
            || packet.source_id != Some(input.source())
            || end > input.position()
        {
            return Err(UNOBSERVED);
        }
        self.last = Some(LastCarrier {
            carrier: Carrier {
                packet: packet.clone(),
                context,
            },
            diagnostic: None,
            confirmed: false,
        });
        let carrier = Carrier {
            packet: packet.clone(),
            context,
        };
        if let Some(run) = &mut self.run {
            return run.push(carrier);
        }
        *self.history.get_mut(self.used).ok_or(UNOBSERVED)? = Some(carrier);
        self.used += 1;
        Ok(())
    }

    fn activate(&mut self, input: &InputCursor, limits: ReaderLimits) -> Result<()> {
        if self.run.is_some() {
            return Ok(());
        }
        let pool = input.budget();
        let demux = DemuxObserver::new_registered(
            DemuxProfile::PesPackets,
            self.scope,
            self.carriage,
            pool.clone(),
            limits.transport,
        )?;
        let mut observer = TransportFaultRecovery::with_policy(
            demux,
            PesFaultPolicy {
                source_pcr_pid: Some(self.pcr_pid),
                ..PesFaultPolicy::default()
            },
        )
        .map_err(|error| error.error)?;
        observer.bind_source_input(input)?;
        self.run = Some(Run {
            observer,
            busy: false,
            finished: false,
            scan: None,
            chunks: BudgetVec::new(pool),
            evidence: BudgetVec::new(pool),
            bytes: 0,
            zeroes: 0,
            leading_zeroes: 0,
            nonzero: false,
            maximum_bytes: limits.video.maximum_access_unit_bytes,
            maximum_chunks: limits.maximum_pes_per_access_unit,
            maximum_padding: limits.video.nal.maximum_padding_bytes.saturating_add(3),
        });
        Ok(())
    }

    fn drive(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Progress> {
        let Some(run) = &mut self.run else {
            return Ok(Progress::Idle);
        };
        loop {
            let progress = run.drive(work, cancel)?;
            if progress != Progress::Idle || self.replay == self.used {
                return Ok(progress);
            }
            if !work.try_charge(1) {
                return Ok(Progress::Yield);
            }
            let carrier = self.history[self.replay].take().ok_or(UNOBSERVED)?;
            run.push(carrier)?;
            self.replay += 1;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Progress {
    Idle,
    Yield,
    Finished,
}

struct Scan {
    packet: DemuxPacket,
    span: usize,
    offset: usize,
    seen: usize,
    end: usize,
}

struct Run {
    observer: TransportFaultRecovery,
    busy: bool,
    finished: bool,
    scan: Option<Scan>,
    chunks: BudgetVec<DemuxPacket>,
    evidence: BudgetVec<ContinuityEvidence>,
    bytes: usize,
    zeroes: u8,
    leading_zeroes: usize,
    nonzero: bool,
    maximum_bytes: usize,
    maximum_chunks: usize,
    maximum_padding: usize,
}

impl Run {
    fn push(&mut self, carrier: Carrier) -> Result<()> {
        self.observer
            .push(carrier.packet, carrier.context)
            .map_err(|error| error.error)?;
        self.busy = true;
        Ok(())
    }

    fn drive(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Progress> {
        while self.busy {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(Progress::Yield);
            }
            if self.scan.is_some() && !self.scan(work, cancel)? {
                return Ok(Progress::Yield);
            }
            match self.observer.poll(work, cancel)? {
                AssemblyStep::Output(TransportObservation::Accepted(packet)) => {
                    let end = self
                        .bytes
                        .checked_add(packet.payload_len)
                        .ok_or(Error::Overflow)?;
                    if packet.payload_len == 0
                        || end > self.maximum_bytes
                        || self.chunks.len() == self.maximum_chunks
                    {
                        return Err(UNOBSERVED);
                    }
                    self.scan = Some(Scan {
                        packet,
                        span: 0,
                        offset: 0,
                        seen: 0,
                        end,
                    });
                }
                AssemblyStep::Output(_) => return Err(UNOBSERVED),
                AssemblyStep::Yield => return Ok(Progress::Yield),
                AssemblyStep::NeedPayload => self.busy = false,
                AssemblyStep::Finished => {
                    self.busy = false;
                    self.finished = true;
                }
            }
        }
        Ok(if self.finished {
            Progress::Finished
        } else {
            Progress::Idle
        })
    }

    fn scan(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        let scan = self.scan.as_mut().expect("pending continuity scan");
        while let Some(span) = scan.packet.spans.as_slice().get(scan.span) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let bytes = span.as_ref();
            if scan.offset == bytes.len() {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                scan.span += 1;
                scan.offset = 0;
                continue;
            }
            let count = work.take((bytes.len() - scan.offset).min(4096) as u64) as usize;
            if count == 0 {
                return Ok(false);
            }
            for &byte in &bytes[scan.offset..scan.offset + count] {
                if byte == 0 {
                    self.zeroes = (self.zeroes + 1).min(2);
                    if !self.nonzero {
                        self.leading_zeroes += 1;
                        if self.leading_zeroes > self.maximum_padding {
                            return Err(UNOBSERVED);
                        }
                    }
                } else {
                    if byte == 1 && self.zeroes == 2 {
                        return Err(UNOBSERVED);
                    }
                    self.nonzero = true;
                    self.zeroes = 0;
                }
            }
            scan.offset += count;
            scan.seen = scan.seen.checked_add(count).ok_or(Error::Overflow)?;
            if scan.seen > scan.packet.payload_len {
                return Err(UNOBSERVED);
            }
        }
        if scan.seen != scan.packet.payload_len {
            return Err(UNOBSERVED);
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        let scan = self.scan.take().expect("complete continuity scan");
        self.chunks
            .try_push(scan.packet)
            .map_err(|error| error.error)?;
        self.bytes = scan.end;
        Ok(true)
    }
}
