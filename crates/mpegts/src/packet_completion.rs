//! Explicit control-only packet completion from physical source facts.
//!
//! H.222.0 (08/2023) 2.4.3.3 and 2.4.4 distinguish control PIDs/sections
//! from PES payload. No validated PES scope, source table, timestamp or clock
//! can be constructed from this operation's confirmation. Non-control payload needs
//! the existing real-scope observer; it can never select an empty fallback.
use crate::demux_pes::SourceContext;
use crate::media_entity::{ObservedTrace, TraceOrigin};
use crate::psi::{PatReader, PatSection, Section, SectionAssembler, TableStep};
use crate::{
    AssemblyStep, CompactTraceEvent, FaultKind, MediaEntityEnd, NullCarrierRun, PacketFormat,
    ParseLimits, TraceEvent, TraceFault, TransportInspection, TransportInspector, TransportPacket,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, Result, SourceId, SourceLocation, Step,
    WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlCarrierKind {
    PatPid,
    PmtSection { association: usize },
    NullPid,
    NoPayload,
    RequiresScopedPacketObserver,
}
#[derive(Debug)]
pub struct PhysicalCarrier {
    packet: TransportPacket,
    context: SourceContext,
    kind: ControlCarrierKind,
}
impl PhysicalCarrier {
    pub fn packet(&self) -> &TransportPacket {
        &self.packet
    }
    pub fn context(&self) -> SourceContext {
        self.context
    }
    pub fn kind(&self) -> ControlCarrierKind {
        self.kind
    }
}
/// Physical carrier evidence compacted by the opt-in source poll.
#[derive(Debug)]
pub struct PhysicalNullRun {
    run: NullCarrierRun,
    first_context: SourceContext,
}
impl PhysicalNullRun {
    pub fn run(&self) -> &NullCarrierRun {
        &self.run
    }
    pub fn first_context(&self) -> SourceContext {
        self.first_context
    }
}
#[derive(Debug)]
pub struct ControlPatFact {
    section: PatSection,
    context: SourceContext,
}
impl ControlPatFact {
    pub fn section(&self) -> &PatSection {
        &self.section
    }
    pub fn context(&self) -> SourceContext {
        self.context
    }
}
#[derive(Debug)]
struct ControlRoute {
    pid: u16,
    program: u16,
    association: usize,
    assembler: SectionAssembler,
    touched: bool,
    anchored: bool,
    sections: usize,
}
/// Proof of absence of media carriers, not proof of a source-validated PES scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoMediaConfirmation {
    source: SourceId,
    source_bytes: u64,
    physical_carriers: usize,
    pat_facts: usize,
    pmt_sections: usize,
    source_finished: SourceContext,
}
impl NoMediaConfirmation {
    pub fn source(self) -> SourceId {
        self.source
    }
    pub fn source_bytes(self) -> u64 {
        self.source_bytes
    }
    pub fn physical_carriers(self) -> usize {
        self.physical_carriers
    }
    pub fn pat_facts(self) -> usize {
        self.pat_facts
    }
    pub fn pmt_sections(self) -> usize {
        self.pmt_sections
    }
    pub fn source_finished(self) -> SourceContext {
        self.source_finished
    }
}
#[derive(Debug)]
pub struct PacketCompletion {
    budget: Budget,
    limits: ParseLimits,
    program: u16,
    format: PacketFormat,
    source: Option<SourceId>,
    origin: Option<TraceOrigin>,
    next_offset: u64,
    last_context: Option<SourceContext>,
    carriers: BudgetVec<PhysicalCarrier>,
    null_runs: BudgetVec<PhysicalNullRun>,
    compact_carrier_count: usize,
    pats: BudgetVec<ControlPatFact>,
    routes: BudgetVec<ControlRoute>,
    diagnostics: BudgetVec<(TraceFault, SourceContext)>,
    no_media_blocker: Option<Error>,
    fault: Option<Error>,
    physical_end: Option<MediaEntityEnd>,
    source_finished: Option<SourceContext>,
    confirmation: Option<NoMediaConfirmation>,
    pat_pid_seen: bool,
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
impl PacketCompletion {
    pub fn new(budget: Budget, limits: ParseLimits, program: u16) -> Result<Self> {
        if program == 0 {
            return Err(Error::Invalid("packet completion selected program"));
        }
        limits.validate()?;
        Ok(Self {
            carriers: BudgetVec::new(&budget),
            null_runs: BudgetVec::new(&budget),
            compact_carrier_count: 0,
            pats: BudgetVec::new(&budget),
            routes: BudgetVec::new(&budget),
            diagnostics: BudgetVec::new(&budget),
            budget,
            limits,
            program,
            format: PacketFormat::Ts188,
            source: None,
            origin: None,
            next_offset: 0,
            last_context: None,
            no_media_blocker: None,
            fault: None,
            physical_end: None,
            source_finished: None,
            confirmation: None,
            pat_pid_seen: false,
        })
    }
    /// Individually admitted carriers. With compact polling, merge these
    /// with physical_null_runs by source offset for the complete evidence.
    pub fn physical_carriers(&self) -> &[PhysicalCarrier] {
        self.carriers.as_slice()
    }
    pub fn physical_null_runs(&self) -> &[PhysicalNullRun] {
        self.null_runs.as_slice()
    }
    pub fn physical_carrier_count(&self) -> usize {
        self.carriers.len() + self.compact_carrier_count
    }
    pub fn raw_pat_facts(&self) -> &[ControlPatFact] {
        self.pats.as_slice()
    }
    pub fn source_diagnostics(&self) -> &[(TraceFault, SourceContext)] {
        self.diagnostics.as_slice()
    }
    pub fn confirmation(&self) -> Option<NoMediaConfirmation> {
        self.confirmation
    }
    /// Explicit physical format. The default constructor stays TS188.
    pub fn new_for_format(
        budget: Budget,
        limits: ParseLimits,
        program: u16,
        format: PacketFormat,
    ) -> Result<Self> {
        let mut value = Self::new(budget, limits, program)?;
        value.format = format;
        Ok(value)
    }
    pub fn fault(&self) -> Option<Error> {
        self.fault
    }
    pub fn empty_refusal(&self) -> Option<Error> {
        self.no_media_blocker
    }
    /// Only the exact last admitted physical carrier can bypass a no-scope guard.
    pub fn is_control_transport(&self, pid: u16, context: SourceContext) -> bool {
        self.carriers.as_slice().last().is_some_and(|r| {
            r.packet.pid == pid
                && r.context == context
                && r.kind != ControlCarrierKind::RequiresScopedPacketObserver
        })
    }
    pub fn required_work(&self, event: &TraceEvent) -> Result<u64> {
        let bytes = match event {
            TraceEvent::Transport(_) => self.format.bytes(),
            TraceEvent::Section(s) => s.bytes().len(),
            _ => 1024,
        };
        let units = bytes
            .checked_mul(512)
            .and_then(|v| v.checked_add(self.carriers.len().checked_mul(64)?))
            .and_then(|v| v.checked_add(self.routes.len().checked_mul(256)?))
            .and_then(|v| v.checked_add(16384))
            .ok_or(Error::Overflow)?;
        u64::try_from(units).map_err(|_| Error::Overflow)
    }
    fn block_empty(&mut self, error: Error) {
        if self.no_media_blocker.is_none() {
            self.no_media_blocker = Some(error);
        }
    }
    fn check_context(&self, source: SourceId, context: SourceContext) -> Result<()> {
        if self.source.is_some_and(|old| old != source) {
            return Err(Error::SourceMismatch);
        }
        if context.byte_run_epoch.is_some()
            || self.last_context.is_some_and(|old| {
                old.source_event_ordinal >= context.source_event_ordinal
                    || old.epochs.clock > context.epochs.clock
                    || old.epochs.configuration > context.epochs.configuration
            })
        {
            return Err(Error::Invalid(
                "packet completion actual source context/order",
            ));
        }
        if self.source_finished.is_some() {
            return Err(Error::Invalid(
                "packet completion data after source Finished",
            ));
        }
        Ok(())
    }
    fn carrier(
        &mut self,
        packet: &TransportPacket,
        source: SourceId,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if self.physical_end.is_some() {
            return Err(Error::Invalid(
                "packet completion transport after physical End",
            ));
        }
        if self.physical_carrier_count() >= self.limits.source_spans {
            return Err(Error::Unsupported(
                "packet completion physical-carrier limit",
            ));
        }
        let mut inspector = TransportInspector::new(
            packet.source.clone(),
            self.format,
            SourceLocation {
                source,
                offset: packet.source_offset,
            },
            &self.budget,
        )
        .map_err(|e| e.error)?;
        let actual = match inspector.poll(work, cancel)? {
            AssemblyStep::Output(TransportInspection::Accepted(p)) => p,
            AssemblyStep::Output(TransportInspection::Rejected(_)) => {
                return Err(Error::Invalid(
                    "packet completion supplied accepted carrier is invalid",
                ));
            }
            _ => {
                return Err(Error::Invalid(
                    "packet completion reserved transport work exhausted",
                ));
            }
        };
        if packet.source_id != Some(source)
            || packet.prefix.as_ref().map(Bytes::as_ref)
                != actual.prefix.as_ref().map(Bytes::as_ref)
            || packet.source.as_ref() != actual.source.as_ref()
            || packet.pid != actual.pid
            || packet.payload_unit_start != actual.payload_unit_start
            || packet.continuity_counter != actual.continuity_counter
            || packet.scrambling != actual.scrambling
            || packet.transport_error != actual.transport_error
            || packet.payload_source_offset != actual.payload_source_offset
            || packet.payload.as_ref().map(Bytes::as_ref)
                != actual.payload.as_ref().map(Bytes::as_ref)
        {
            return Err(Error::Invalid(
                "packet completion actual raw/decoded carrier mismatch",
            ));
        }
        if actual.transport_error || actual.scrambling != 0 {
            return Err(Error::Unsupported(
                "packet completion unreadable physical payload",
            ));
        }
        if actual.source_offset != self.next_offset {
            self.block_empty(Error::Unsupported(
                "packet completion physical coverage gap/order",
            ));
        }
        if actual.source_offset < self.next_offset {
            return Err(Error::Invalid(
                "packet completion duplicate/reversed physical carrier",
            ));
        }
        self.next_offset = actual
            .source_offset
            .checked_add(self.format.bytes() as u64)
            .ok_or(Error::Overflow)?;
        let kind = if actual.pid == 0 {
            self.pat_pid_seen = true;
            // A PES start at the control payload start is not silently ignored.
            if actual.payload_unit_start
                && actual
                    .payload
                    .as_ref()
                    .is_some_and(|b| b.as_ref().starts_with(&[0, 0, 1]))
            {
                self.block_empty(Error::Unsupported(
                    "packet completion ambiguous PES start on PAT PID",
                ));
            }
            ControlCarrierKind::PatPid
        } else if actual.pid == 0x1fff && !actual.payload_unit_start {
            ControlCarrierKind::NullPid
        } else if actual.payload.is_none() {
            ControlCarrierKind::NoPayload
        } else if let Some(index) = self
            .routes
            .as_slice()
            .iter()
            .position(|r| r.pid == actual.pid)
        {
            let route = &mut self.routes.as_mut_slice()[index];
            route.touched = true;
            let body = actual
                .payload
                .as_ref()
                .ok_or(Error::Invalid("packet completion PMT payload missing"))?;
            if actual.payload_unit_start && body.as_ref().starts_with(&[0, 0, 1]) {
                return Err(Error::Unsupported(
                    "packet completion PES start on declared PMT PID",
                ));
            }
            if !route.anchored && !actual.payload_unit_start {
                return Err(Error::Unsupported(
                    "packet completion unanchored PMT continuation",
                ));
            }
            if actual.payload_unit_start {
                route.anchored = true;
            }
            route
                .assembler
                .push(
                    body.clone(),
                    actual.payload_unit_start,
                    source,
                    actual.payload_source_offset,
                )
                .map_err(|e| e.error)?;
            loop {
                match route.assembler.poll(work, cancel)? {
                    AssemblyStep::Output(section) => {
                        let raw = section.bytes().as_ref();
                        if section.crc_remainder() != Some(0)
                            || raw.len() < 12
                            || raw[0] != 2
                            || u16::from_be_bytes([raw[3], raw[4]]) != route.program
                        {
                            return Err(Error::Unsupported(
                                "packet completion control PMT identity/CRC",
                            ));
                        }
                        route.sections = route.sections.checked_add(1).ok_or(Error::Overflow)?;
                    }
                    AssemblyStep::NeedPayload => break,
                    _ => {
                        return Err(Error::Invalid(
                            "packet completion reserved PMT work exhausted",
                        ));
                    }
                }
            }
            ControlCarrierKind::PmtSection {
                association: route.association,
            }
        } else {
            self.block_empty(Error::Unsupported(
                "packet completion physical payload needs validated PES observer",
            ));
            ControlCarrierKind::RequiresScopedPacketObserver
        };
        self.carriers
            .try_push(PhysicalCarrier {
                packet: actual,
                context,
                kind,
            })
            .map_err(|e| e.error)?;
        Ok(())
    }
    fn literal_section(&self, section: &Section, context: SourceContext) -> Result<()> {
        if !section.bytes().belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        let mut at = 0usize;
        let mut carrier = 0usize;
        let mut end = 0u64;
        for range in section.source_ranges() {
            if range.bytes == 0 || range.offset < end || Some(range.entity) != self.source {
                return Err(Error::Invalid("packet completion source section ranges"));
            }
            end = range.end()?;
            while self.carriers.as_slice().get(carrier).is_some_and(|c| {
                c.packet.source_offset + c.packet.source.len() as u64 <= range.offset
            }) {
                carrier += 1;
            }
            let c = self.carriers.as_slice().get(carrier).ok_or(Error::Invalid(
                "packet completion source section has no physical carrier",
            ))?;
            let p = &c.packet;
            let body = p.payload.as_ref().ok_or(Error::Invalid(
                "packet completion source section on no-payload carrier",
            ))?;
            if p.pid != section.pid()
                || c.context.source_event_ordinal >= context.source_event_ordinal
                || range.offset < p.payload_source_offset
                || end
                    > p.payload_source_offset
                        .checked_add(body.len() as u64)
                        .ok_or(Error::Overflow)?
            {
                return Err(Error::Invalid(
                    "packet completion source section physical provenance",
                ));
            }
            let first = usize::try_from(range.offset - p.payload_source_offset)
                .map_err(|_| Error::Overflow)?;
            let next = at.checked_add(range.bytes).ok_or(Error::Overflow)?;
            if section.bytes().as_ref().get(at..next)
                != body.as_ref().get(first..first + range.bytes)
            {
                return Err(Error::Invalid(
                    "packet completion source section literal mismatch",
                ));
            }
            at = next;
        }
        if at != section.bytes().len() || at == 0 {
            return Err(Error::Invalid(
                "packet completion source section exact accounting",
            ));
        }
        Ok(())
    }
    fn pat(
        &mut self,
        section: &Section,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if section.pid() != 0
            || section.bytes().as_ref().first() != Some(&0)
            || section.crc_remainder() != Some(0)
        {
            return Ok(());
        }
        self.literal_section(section, context)?;
        if self.pats.len() >= self.limits.source_spans {
            return Err(Error::Unsupported("packet completion PAT-fact limit"));
        }
        let mut reader = PatReader::new(section, &self.budget, self.limits)?;
        let TableStep::Output(decoded) = reader.poll(work, cancel)? else {
            return Err(Error::Invalid(
                "packet completion reserved PAT work exhausted",
            ));
        };
        let association = self.pats.len();
        for entry in decoded.programs.as_slice() {
            check_cancel(cancel)?;
            if entry.is_network_pid() {
                continue;
            }
            if let Some(route) = self
                .routes
                .as_mut_slice()
                .iter_mut()
                .find(|r| r.pid == entry.pid)
            {
                if route.program != entry.program_number {
                    return Err(Error::Unsupported("packet completion PMT PID reassignment"));
                }
                route.association = association;
            } else {
                if self.routes.len() >= self.limits.programs {
                    return Err(Error::Unsupported("packet completion PMT-route limit"));
                }
                self.routes
                    .try_push(ControlRoute {
                        pid: entry.pid,
                        program: entry.program_number,
                        association,
                        assembler: SectionAssembler::new(
                            entry.pid,
                            self.budget.clone(),
                            self.limits,
                        )?,
                        touched: false,
                        anchored: false,
                        sections: 0,
                    })
                    .map_err(|e| e.error)?;
            }
        }
        // current_next/section numbers are retained literally; this is a raw
        // association fact, including inactive/incomplete tables, never a scope.
        self.pats
            .try_push(ControlPatFact {
                section: decoded,
                context,
            })
            .map_err(|e| e.error)?;
        Ok(())
    }
    fn observe_inner(
        &mut self,
        event: &TraceEvent,
        source: SourceId,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.check_context(source, context)?;
        self.source = Some(source);
        match event {
            TraceEvent::Transport(packet) => self.carrier(packet, source, context, work, cancel)?,
            TraceEvent::Section(section) => self.pat(section, context, work, cancel)?,
            TraceEvent::Pes(_) => self.block_empty(Error::Unsupported(
                "packet completion actual source PES cannot be empty",
            )),
            TraceEvent::Diagnostic(fault) => {
                if fault.location.source != source {
                    return Err(Error::SourceMismatch);
                }
                if let Some(rejected) = fault.rejected.as_ref() {
                    if !rejected.bytes().belongs_to(&self.budget) {
                        return Err(Error::ForeignPool);
                    }
                    if rejected.source_id() != Some(source)
                        || rejected.source_offset() != fault.location.offset
                    {
                        return Err(Error::Invalid(
                            "packet completion rejected carrier provenance",
                        ));
                    }
                }
                if self.diagnostics.len() >= self.limits.source_spans {
                    return Err(Error::Unsupported("packet completion diagnostic limit"));
                }
                if fault.kind != FaultKind::Psi && fault.kind != FaultKind::Continuity {
                    self.block_empty(Error::Unsupported(
                        "packet completion source failure/transport/media uncertainty",
                    ));
                }
                self.diagnostics
                    .try_push((fault.clone(), context))
                    .map_err(|e| e.error)?;
            }
            TraceEvent::Discontinuity { .. } => {
                return Err(Error::Unsupported(
                    "packet completion is one physical entity",
                ));
            }
            TraceEvent::Finished => {
                let end = self.physical_end.as_ref().ok_or(Error::Invalid(
                    "packet completion source Finished without physical EOF witness",
                ))?;
                if end.epochs != context.epochs {
                    return Err(Error::Invalid(
                        "packet completion finished/End source epochs",
                    ));
                }
                self.source_finished = Some(context);
            }
            _ => {}
        }
        self.last_context = Some(context);
        Ok(())
    }
    fn check_origin(&self, origin: &TraceOrigin, context: SourceContext) -> Result<()> {
        if origin.epochs != context.epochs
            || self.origin.as_ref().is_some_and(|first| {
                first.issuer != origin.issuer
                    || first.source != origin.source
                    || first.token != origin.token
            })
        {
            return Err(Error::Invalid("packet completion trace source mismatch"));
        }
        Ok(())
    }

    /// Admit a literal source event. False leaves it available to retry.
    pub fn observe_issued(
        &mut self,
        event: &ObservedTrace<TraceEvent>,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        self.observe_traced(&event.event, &event.origin, context, work, cancel)
    }

    fn observe_traced(
        &mut self,
        event: &TraceEvent,
        origin: &TraceOrigin,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        check_cancel(cancel)?;
        self.check_origin(origin, context)?;
        let consumed = self.observe(event, origin.source, context, work, cancel)?;
        if consumed && self.origin.is_none() {
            self.origin = Some(origin.clone());
        }
        Ok(consumed)
    }

    /// Admit one source-bound compact event. The budget still counts every carrier.
    pub fn observe_compact(
        &mut self,
        event: &ObservedTrace<CompactTraceEvent>,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        check_cancel(cancel)?;
        self.check_origin(&event.origin, context)?;
        let consumed = match &event.event {
            CompactTraceEvent::Event(trace) => {
                self.observe(trace, event.origin.source, context, work, cancel)?
            }
            CompactTraceEvent::NullRun(run) => self.observe_null_run(run, context, work, cancel)?,
        };
        if consumed && self.origin.is_none() {
            self.origin = Some(event.origin.clone());
        }
        Ok(consumed)
    }

    // Errors after admission are sticky. An exhausted grant leaves the event pending.
    fn observe(
        &mut self,
        event: &TraceEvent,
        source: SourceId,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        check_cancel(cancel)?;
        if let Some(e) = self.fault {
            return Err(e);
        }
        let cost = self.required_work(event)?;
        if !work.try_charge(cost) {
            return Ok(false);
        }
        match self.observe_inner(event, source, context, &mut WorkBudget::new(cost), cancel) {
            Ok(()) => Ok(true),
            Err(e) => {
                self.fault = Some(e);
                Err(e)
            }
        }
    }
    /// Admit one sealed run at its first literal source-event ordinal. The
    /// capacity limit still counts physical carriers, even in the compact view.
    fn observe_null_run(
        &mut self,
        run: &NullCarrierRun,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        check_cancel(cancel)?;
        if let Some(error) = self.fault {
            return Err(error);
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        let result = self.admit_null_run(run, context);
        if let Err(error) = result {
            self.fault = Some(error);
            return Err(error);
        }
        Ok(true)
    }
    fn admit_null_run(&mut self, run: &NullCarrierRun, context: SourceContext) -> Result<()> {
        let location = run.start();
        self.check_context(location.source, context)?;
        if !run.bytes().belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        if run.format() != self.format || self.physical_end.is_some() {
            return Err(Error::Invalid("packet completion null run format or End"));
        }
        if run.len() > self.limits.source_spans - self.physical_carrier_count() {
            return Err(Error::Unsupported(
                "packet completion physical-carrier limit",
            ));
        }
        if location.offset < self.next_offset {
            return Err(Error::Invalid(
                "packet completion duplicate/reversed physical carrier",
            ));
        }
        let end = location
            .offset
            .checked_add(run.bytes().len() as u64)
            .ok_or(Error::Overflow)?;
        let last_ordinal = context
            .source_event_ordinal
            .checked_add((run.len() - 1) as u64)
            .ok_or(Error::Overflow)?;
        self.null_runs
            .try_push(PhysicalNullRun {
                run: run.clone(),
                first_context: context,
            })
            .map_err(|failure| failure.error)?;
        if location.offset != self.next_offset {
            self.block_empty(Error::Unsupported(
                "packet completion physical coverage gap/order",
            ));
        }
        self.source = Some(location.source);
        self.next_offset = end;
        self.last_context = Some(SourceContext {
            source_event_ordinal: last_ordinal,
            ..context
        });
        self.compact_carrier_count += run.len();
        Ok(())
    }
    pub fn required_end_work(&self) -> Result<u64> {
        u64::try_from(
            self.carriers
                .len()
                .checked_add(self.routes.len())
                .and_then(|v| v.checked_mul(512))
                .and_then(|v| v.checked_add(16384))
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)
    }
    fn end_matches(&self, end: &MediaEntityEnd, input: &InputCursor) -> Result<()> {
        if !self.budget.same_pool(input.budget()) || !self.budget.same_pool(&end.budget) {
            return Err(Error::ForeignPool);
        }
        if self
            .origin
            .as_ref()
            .is_none_or(|origin| origin.issuer != end.issuer || origin.token != end.token)
            || end.program != self.program
            || end.format != self.format
            || Some(input.source()) != self.source
            || end.location.source != input.source()
            || end.location.offset != input.position()
            || end.token != input.token()
        {
            return Err(Error::Invalid(
                "packet completion actual core/framer End mismatch",
            ));
        }
        if self.last_context.is_some_and(|c| c.epochs != end.epochs) {
            return Err(Error::Invalid(
                "packet completion source epoch/End mismatch",
            ));
        }
        Ok(())
    }
    /// The composed source must supply its real drained InputCursor/TS End.
    pub fn seal_physical_end(
        &mut self,
        end: &MediaEntityEnd,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        check_cancel(cancel)?;
        if let Some(e) = self.fault {
            return Err(e);
        }
        if self.physical_end.is_some() || self.source_finished.is_some() {
            return Err(Error::Invalid("packet completion duplicate physical End"));
        }
        self.end_matches(end, input)?;
        let cost = self.required_end_work()?;
        if !work.try_charge(cost) {
            return Ok(false);
        }
        match input.read_into(&mut [0_u8; 1], &mut WorkBudget::new(1), cancel)? {
            Step::End(actual) if actual == end.location => {}
            _ => {
                return Err(Error::Invalid(
                    "packet completion core no longer reports physical End",
                ));
            }
        }
        if self.next_offset != end.location.offset {
            self.block_empty(Error::Unsupported(
                "packet completion incomplete physical carrier coverage",
            ));
        }
        self.physical_end = Some(end.clone());
        Ok(true)
    }
    /// Real EOF and source Finished are mandatory even on the ordinary scoped
    /// branch. This method does not reinterpret transport failure as EOF.
    pub fn validate_finished(
        &self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        check_cancel(cancel)?;
        if let Some(e) = self.fault {
            return Err(e);
        }
        let end = self.physical_end.as_ref().ok_or(Error::Invalid(
            "packet completion missing real physical End",
        ))?;
        self.end_matches(end, input)?;
        if self.source_finished.is_none() {
            return Err(Error::Invalid("packet completion source has not Finished"));
        }
        // A later core failure can retain its old token/position. Re-read the
        // actual terminal state; identity/advertised size alone are insufficient.
        match input.read_into(&mut [0_u8; 1], work, cancel)? {
            Step::End(actual) if actual == end.location => Ok(true),
            Step::Yield => Ok(false),
            _ => Err(Error::Invalid(
                "packet completion core terminal is not confirmed EOF",
            )),
        }
    }
    /// No metadata count, filename or empty observer selects this path. Every
    /// literal physical carrier must have a non-media classification and EOF.
    pub fn confirm_no_media(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<NoMediaConfirmation>> {
        check_cancel(cancel)?;
        if let Some(e) = self.fault {
            return Err(e);
        }
        if self.confirmation.is_some() {
            return Err(Error::Invalid(
                "packet completion duplicate no-media receipt",
            ));
        }
        let cost = self.required_end_work()?;
        if !work.try_charge(cost) {
            return Ok(None);
        }
        if !self.validate_finished(input, &mut WorkBudget::new(1), cancel)? {
            return Err(Error::Invalid("packet completion terminal grant exhausted"));
        }
        if let Some(e) = self.no_media_blocker {
            return Err(e);
        }
        if !self.pat_pid_seen || self.carriers.is_empty() {
            return Err(Error::Unsupported(
                "packet completion no observed PAT control carrier",
            ));
        }
        for route in self.routes.as_slice() {
            check_cancel(cancel)?;
            if route.touched
                && (!route.anchored
                    || route.sections == 0
                    || !route.assembler.partial_bytes().is_empty())
            {
                return Err(Error::Unsupported(
                    "packet completion unfinished PMT control syntax",
                ));
            }
        }
        let end = self
            .physical_end
            .as_ref()
            .ok_or(Error::Invalid("packet completion lost End"))?;
        let confirmation = NoMediaConfirmation {
            source: end.location.source,
            source_bytes: end.location.offset,
            physical_carriers: self.physical_carrier_count(),
            pat_facts: self.pats.len(),
            pmt_sections: self.routes.as_slice().iter().try_fold(0usize, |n, r| {
                n.checked_add(r.sections).ok_or(Error::Overflow)
            })?,
            source_finished: self
                .source_finished
                .ok_or(Error::Invalid("packet completion lost Finished"))?,
        };
        self.confirmation = Some(confirmation);
        Ok(Some(confirmation))
    }
}

#[cfg(test)]
mod guards;
