//! Explicit flags48 control-only packet completion from physical source facts.
//!
//! H.222.0 (08/2023) 2.4.3.3 and 2.4.4 distinguish control PIDs/sections
//! from PES payload. No validated PES scope, source table, timestamp or clock
//! can be constructed from this operation's receipt. Non-control payload needs
//! the existing real-scope observer; it can never select an empty fallback.
use crate::native_pes::NativeSourceContext;
use crate::psi::{PatReader, PatSection, Section, SectionAssembler, TableStep};
use crate::{
    AssemblyStep, FaultKind, MediaEntityEndV1, NullCarrierRun, PacketFormat, ParseLimits,
    TraceEvent, TraceFault, TransportInspection, TransportInspector, TransportPacket,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, Result, SourceId, SourceLocation, Step,
    WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeControlCarrierKindV1 {
    PatPid,
    PmtSection { association: usize },
    NullPid,
    NoPayload,
    RequiresScopedPacketObserver,
}
#[derive(Debug)]
pub struct NativePhysicalCarrierV1 {
    packet: TransportPacket,
    context: NativeSourceContext,
    kind: NativeControlCarrierKindV1,
}
impl NativePhysicalCarrierV1 {
    pub fn packet(&self) -> &TransportPacket {
        &self.packet
    }
    pub fn context(&self) -> NativeSourceContext {
        self.context
    }
    pub fn kind(&self) -> NativeControlCarrierKindV1 {
        self.kind
    }
}
/// Physical carrier evidence compacted by the opt-in source poll.
#[derive(Debug)]
pub struct NativePhysicalNullRun {
    run: NullCarrierRun,
    first_context: NativeSourceContext,
}
impl NativePhysicalNullRun {
    pub fn run(&self) -> &NullCarrierRun {
        &self.run
    }
    pub fn first_context(&self) -> NativeSourceContext {
        self.first_context
    }
}
#[derive(Debug)]
pub struct NativeControlPatFactV1 {
    section: PatSection,
    context: NativeSourceContext,
}
impl NativeControlPatFactV1 {
    pub fn section(&self) -> &PatSection {
        &self.section
    }
    pub fn context(&self) -> NativeSourceContext {
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
pub struct NativeNoMediaReceiptV1 {
    source: SourceId,
    source_bytes: u64,
    physical_carriers: usize,
    pat_facts: usize,
    pmt_sections: usize,
    source_finished: NativeSourceContext,
}
impl NativeNoMediaReceiptV1 {
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
    pub fn source_finished(self) -> NativeSourceContext {
        self.source_finished
    }
}
#[derive(Debug)]
pub struct NativePacketCompletionV1 {
    budget: Budget,
    limits: ParseLimits,
    program: u16,
    format: PacketFormat,
    source: Option<SourceId>,
    next_offset: u64,
    last_context: Option<NativeSourceContext>,
    carriers: BudgetVec<NativePhysicalCarrierV1>,
    null_runs: BudgetVec<NativePhysicalNullRun>,
    compact_carrier_count: usize,
    pats: BudgetVec<NativeControlPatFactV1>,
    routes: BudgetVec<ControlRoute>,
    diagnostics: BudgetVec<(TraceFault, NativeSourceContext)>,
    no_media_blocker: Option<Error>,
    fault: Option<Error>,
    physical_end: Option<MediaEntityEndV1>,
    source_finished: Option<NativeSourceContext>,
    receipt: Option<NativeNoMediaReceiptV1>,
    pat_pid_seen: bool,
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
impl NativePacketCompletionV1 {
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
            next_offset: 0,
            last_context: None,
            no_media_blocker: None,
            fault: None,
            physical_end: None,
            source_finished: None,
            receipt: None,
            pat_pid_seen: false,
        })
    }
    /// Individually admitted carriers. With compact polling, merge these
    /// with physical_null_runs by source offset for the complete evidence.
    pub fn physical_carriers(&self) -> &[NativePhysicalCarrierV1] {
        self.carriers.as_slice()
    }
    pub fn physical_null_runs(&self) -> &[NativePhysicalNullRun] {
        self.null_runs.as_slice()
    }
    pub fn physical_carrier_count(&self) -> usize {
        self.carriers.len() + self.compact_carrier_count
    }
    pub fn raw_pat_facts(&self) -> &[NativeControlPatFactV1] {
        self.pats.as_slice()
    }
    pub fn source_diagnostics(&self) -> &[(TraceFault, NativeSourceContext)] {
        self.diagnostics.as_slice()
    }
    pub fn receipt(&self) -> Option<NativeNoMediaReceiptV1> {
        self.receipt
    }
    /// Additive explicit physical format. The original constructor stays TS188.
    pub fn new_for_format_v2(
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
    pub fn is_control_transport_v1(&self, pid: u16, context: NativeSourceContext) -> bool {
        self.carriers.as_slice().last().is_some_and(|r| {
            r.packet.pid == pid
                && r.context == context
                && r.kind != NativeControlCarrierKindV1::RequiresScopedPacketObserver
        })
    }
    pub fn required_work_v1(&self, event: &TraceEvent) -> Result<u64> {
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
    fn check_context(&self, source: SourceId, context: NativeSourceContext) -> Result<()> {
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
        context: NativeSourceContext,
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
            NativeControlCarrierKindV1::PatPid
        } else if actual.pid == 0x1fff && !actual.payload_unit_start {
            NativeControlCarrierKindV1::NullPid
        } else if actual.payload.is_none() {
            NativeControlCarrierKindV1::NoPayload
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
            NativeControlCarrierKindV1::PmtSection {
                association: route.association,
            }
        } else {
            self.block_empty(Error::Unsupported(
                "packet completion physical payload needs validated PES observer",
            ));
            NativeControlCarrierKindV1::RequiresScopedPacketObserver
        };
        self.carriers
            .try_push(NativePhysicalCarrierV1 {
                packet: actual,
                context,
                kind,
            })
            .map_err(|e| e.error)?;
        Ok(())
    }
    fn literal_section(&self, section: &Section, context: NativeSourceContext) -> Result<()> {
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
        context: NativeSourceContext,
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
        let decoded = match reader.poll(work, cancel)? {
            TableStep::Output(p) => p,
            _ => {
                return Err(Error::Invalid(
                    "packet completion reserved PAT work exhausted",
                ));
            }
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
            .try_push(NativeControlPatFactV1 {
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
        context: NativeSourceContext,
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
    /// false means the source event has not been consumed. Errors are sticky
    /// after admission; cancellation before the reservation leaves it retryable.
    pub fn observe_v1(
        &mut self,
        event: &TraceEvent,
        source: SourceId,
        context: NativeSourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        check_cancel(cancel)?;
        if let Some(e) = self.fault {
            return Err(e);
        }
        let cost = self.required_work_v1(event)?;
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
    pub fn observe_null_run(
        &mut self,
        run: &NullCarrierRun,
        context: NativeSourceContext,
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
    fn admit_null_run(&mut self, run: &NullCarrierRun, context: NativeSourceContext) -> Result<()> {
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
            .try_push(NativePhysicalNullRun {
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
        self.last_context = Some(NativeSourceContext {
            source_event_ordinal: last_ordinal,
            ..context
        });
        self.compact_carrier_count += run.len();
        Ok(())
    }
    pub fn required_end_work_v1(&self) -> Result<u64> {
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
    fn end_matches(&self, end: &MediaEntityEndV1, input: &InputCursor) -> Result<()> {
        if !self.budget.same_pool(input.budget()) || !self.budget.same_pool(&end.budget) {
            return Err(Error::ForeignPool);
        }
        if end.program != self.program
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
    pub fn seal_physical_end_v1(
        &mut self,
        end: &MediaEntityEndV1,
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
        let cost = self.required_end_work_v1()?;
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
    pub fn validate_finished_v1(
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
    pub fn confirm_no_media_v1(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<NativeNoMediaReceiptV1>> {
        check_cancel(cancel)?;
        if let Some(e) = self.fault {
            return Err(e);
        }
        if self.receipt.is_some() {
            return Err(Error::Invalid(
                "packet completion duplicate no-media receipt",
            ));
        }
        let cost = self.required_end_work_v1()?;
        if !work.try_charge(cost) {
            return Ok(None);
        }
        if !self.validate_finished_v1(input, &mut WorkBudget::new(1), cancel)? {
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
        let receipt = NativeNoMediaReceiptV1 {
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
        self.receipt = Some(receipt);
        Ok(Some(receipt))
    }
}

#[cfg(test)]
mod guards {
    use super::*;
    use crate::{SourceEpochs, TransportStream};
    use triergon_core::{InputWindow, SourceCapabilities, SourceFailure};
    const BAD_CRC: &[u8] = include_bytes!("../tests/data/psi-crc-invalid.ts");
    const BASIC: &[u8] = include_bytes!("../tests/data/ts188-basic.ts");
    const INACTIVE: &[u8] = include_bytes!("../tests/data/psi-pending-not-current.ts");
    #[allow(
        clippy::large_enum_variant,
        reason = "test recorder retains the original inline owned source event; no extra boxed allocation"
    )]
    enum Item {
        Event(TraceEvent, NativeSourceContext),
        End(MediaEntityEndV1),
    }
    struct Run {
        items: Vec<Item>,
        input: InputCursor,
    }
    fn source(data: &[u8], budget: &Budget) -> Run {
        let mut input = InputCursor::new(
            budget,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            4096,
        )
        .unwrap();
        let mut stream = TransportStream::new(
            budget.clone(),
            PacketFormat::Ts188,
            ParseLimits::default(),
            1,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut ordinal = 0;
        let mut items = Vec::new();
        for _ in 0..500_000 {
            match stream
                .poll(&mut input, &mut WorkBudget::new(17), &cancel)
                .unwrap()
            {
                Step::Ready(event) => {
                    let finished = matches!(event, TraceEvent::Finished);
                    let context = NativeSourceContext {
                        source_event_ordinal: ordinal,
                        epochs: stream.source_epochs(),
                        byte_run_epoch: None,
                    };
                    ordinal += 1;
                    items.push(Item::Event(event, context));
                    if finished {
                        return Run { items, input };
                    }
                }
                Step::Yield => {}
                Step::NeedInput(r) => {
                    let at = usize::try_from(r.offset).unwrap();
                    if at == data.len() {
                        input.finish(r.source, r.token).unwrap();
                    } else {
                        let end = data.len().min(at + r.maximum.min(7));
                        input
                            .provide(
                                &InputWindow::new(
                                    r.source,
                                    r.token,
                                    r.offset,
                                    Bytes::copy_from_slice(budget, &data[at..end]).unwrap(),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Step::End(_) => {
                    let end = stream
                        .media_entity_end_v1(&mut input, &mut WorkBudget::new(1), &cancel)
                        .unwrap()
                        .unwrap();
                    items.push(Item::End(end));
                    stream.end_of_source().unwrap();
                }
            }
        }
        panic!("bounded source guard did not finish")
    }
    fn view(b: &Budget) -> NativePacketCompletionV1 {
        NativePacketCompletionV1::new(b.clone(), ParseLimits::default(), 1).unwrap()
    }
    fn feed(
        v: &mut NativePacketCompletionV1,
        item: &Item,
        input: &mut InputCursor,
    ) -> Result<bool> {
        let cancel = AtomicBool::new(false);
        match item {
            Item::Event(e, c) => {
                let work = v.required_work_v1(e)?;
                v.observe_v1(e, SourceId(1), *c, &mut WorkBudget::new(work), &cancel)
            }
            Item::End(end) => {
                let work = v.required_end_work_v1()?;
                v.seal_physical_end_v1(end, input, &mut WorkBudget::new(work), &cancel)
            }
        }
    }
    fn all(v: &mut NativePacketCompletionV1, run: &mut Run) -> Result<()> {
        for item in &run.items {
            assert!(feed(v, item, &mut run.input)?);
        }
        Ok(())
    }
    fn finish(
        v: &mut NativePacketCompletionV1,
        input: &mut InputCursor,
    ) -> Result<Option<NativeNoMediaReceiptV1>> {
        let work = v.required_end_work_v1()?;
        v.confirm_no_media_v1(input, &mut WorkBudget::new(work), &AtomicBool::new(false))
    }
    fn first(run: &Run) -> (&TransportPacket, NativeSourceContext) {
        match &run.items[0] {
            Item::Event(TraceEvent::Transport(p), c) => (p, *c),
            _ => panic!("first carrier fixture"),
        }
    }
    #[test]
    fn compact_null_runs_keep_physical_bytes_order_and_completion() {
        use crate::CompactTraceEvent;

        let mut data = BAD_CRC.to_vec();
        for cc in 0..19 {
            let mut carrier = [cc; 188];
            carrier[..6].copy_from_slice(&[0x47, 0x1f, 0xff, 0x30 | (cc & 15), 1, 0]);
            data.extend(carrier);
        }

        for window in [97, 1013, 4096] {
            let budget = Budget::new(8 * 1024 * 1024).unwrap();
            let literal = source(&data, &budget);
            let mut expected = literal.items.iter().filter_map(|item| match item {
                Item::Event(TraceEvent::Transport(packet), context) => Some((packet, context)),
                _ => None,
            });
            let mut input = InputCursor::new(
                &budget,
                SourceId(1),
                SourceCapabilities {
                    length: None,
                    random_access: false,
                },
                window,
            )
            .unwrap();
            let mut stream = TransportStream::new(
                budget.clone(),
                PacketFormat::Ts188,
                ParseLimits::default(),
                1,
            )
            .unwrap();
            let mut completion = view(&budget);
            let cancel = AtomicBool::new(false);
            let mut ordinal = 0;
            for _ in 0..10_000 {
                match stream
                    .poll_compact(&mut input, &mut WorkBudget::new(4096), &cancel)
                    .unwrap()
                {
                    Step::Ready(CompactTraceEvent::Event(event)) => {
                        let context = NativeSourceContext {
                            source_event_ordinal: ordinal,
                            epochs: stream.source_epochs(),
                            byte_run_epoch: None,
                        };
                        ordinal += 1;
                        if let TraceEvent::Transport(packet) = &event {
                            let (want, want_context) = expected.next().unwrap();
                            assert_eq!(
                                (packet.source_offset, packet.source.as_ref(), &context),
                                (want.source_offset, want.source.as_ref(), want_context)
                            );
                        }
                        let done = matches!(event, TraceEvent::Finished);
                        assert!(
                            feed(&mut completion, &Item::Event(event, context), &mut input)
                                .unwrap()
                        );
                        if done {
                            break;
                        }
                    }
                    Step::Ready(CompactTraceEvent::NullRun(run)) => {
                        let context = NativeSourceContext {
                            source_event_ordinal: ordinal,
                            epochs: stream.source_epochs(),
                            byte_run_epoch: None,
                        };
                        assert!(
                            !completion
                                .observe_null_run(&run, context, &mut WorkBudget::new(0), &cancel)
                                .unwrap()
                        );
                        assert!(
                            completion
                                .observe_null_run(&run, context, &mut WorkBudget::new(1), &cancel)
                                .unwrap()
                        );
                        for index in 0..run.len() {
                            let packet = run.packet(index).unwrap();
                            let (want, want_context) = expected.next().unwrap();
                            assert_eq!(
                                (
                                    packet.source_offset,
                                    packet.source.as_ref(),
                                    packet.source_id
                                ),
                                (want.source_offset, want.source.as_ref(), want.source_id)
                            );
                            assert_eq!(want_context.source_event_ordinal, ordinal);
                            ordinal += 1;
                        }
                    }
                    Step::NeedInput(r) => {
                        let at = r.offset as usize;
                        if at == data.len() {
                            input.finish(r.source, r.token).unwrap();
                        } else {
                            let end = data.len().min(at + r.maximum);
                            input
                                .provide_owned(
                                    InputWindow::new(
                                        r.source,
                                        r.token,
                                        r.offset,
                                        Bytes::copy_from_slice(&budget, &data[at..end]).unwrap(),
                                    )
                                    .unwrap(),
                                )
                                .unwrap();
                        }
                    }
                    Step::End(_) => {
                        let end = stream
                            .media_entity_end_v1(&mut input, &mut WorkBudget::new(1), &cancel)
                            .unwrap()
                            .unwrap();
                        assert!(feed(&mut completion, &Item::End(end), &mut input).unwrap());
                        stream.end_of_source().unwrap();
                    }
                    Step::Yield => {}
                }
            }
            let receipt = finish(&mut completion, &mut input).unwrap().unwrap();
            assert!(expected.next().is_none());
            assert_eq!(receipt.physical_carriers(), data.len() / 188);
            assert_eq!(completion.physical_null_runs().is_empty(), window < 188);
        }
    }

    #[test]
    fn original_control_only_bytes_complete_without_a_pes_scope() {
        for data in [
            BAD_CRC,
            INACTIVE,
            include_bytes!("../tests/data/psi-pointer-overrun.ts").as_slice(),
            include_bytes!("../tests/data/psi-length-over-policy.ts").as_slice(),
            include_bytes!("../tests/data/pat-incomplete-version.ts").as_slice(),
            include_bytes!("../tests/data/pat-network-pid-not-track.ts").as_slice(),
            include_bytes!("../tests/data/psi-pointer-finishes-old-starts-new.ts").as_slice(),
            include_bytes!("../tests/data/psi-two-sections-one-payload.ts").as_slice(),
            include_bytes!("../tests/data/pmt-descriptor-overrun.ts").as_slice(),
            include_bytes!("../tests/data/pmt-multi-section-forbidden.ts").as_slice(),
            include_bytes!("../tests/data/psi-stuffing-tail-invalid.ts").as_slice(),
            include_bytes!("../tests/data/psi-truncated-eof.ts").as_slice(),
        ] {
            let b = Budget::new(8 * 1024 * 1024).unwrap();
            let mut run = source(data, &b);
            let mut v = view(&b);
            all(&mut v, &mut run).unwrap();
            let receipt = finish(&mut v, &mut run.input).unwrap().unwrap();
            assert_eq!(receipt.source_bytes(), data.len() as u64);
            assert_eq!(receipt.physical_carriers(), data.len() / 188);
            for evidence in v.physical_carriers() {
                let p = evidence.packet();
                assert_eq!(
                    p.source.as_ref(),
                    &data[p.source_offset as usize..p.source_offset as usize + 188]
                );
                assert_ne!(
                    evidence.kind(),
                    NativeControlCarrierKindV1::RequiresScopedPacketObserver
                );
            }
        }
    }
    #[test]
    fn inactive_pat_association_stays_inactive_and_raw() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(INACTIVE, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        finish(&mut v, &mut run.input).unwrap().unwrap();
        assert!(!v.raw_pat_facts()[0].section().header.current_next);
        assert_eq!(
            v.physical_carriers()[1].kind(),
            NativeControlCarrierKindV1::PmtSection { association: 0 }
        );
        assert_eq!(
            v.raw_pat_facts()[0].section().source.as_ref(),
            &INACTIVE[172..188]
        );
    }
    #[test]
    fn zero_event_work_is_unconsumed_and_retryable() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_CRC, &b);
        let mut v = view(&b);
        let Item::Event(e, c) = &run.items[0] else {
            panic!("carrier")
        };
        assert!(
            !v.observe_v1(
                e,
                SourceId(1),
                *c,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
        assert!(v.physical_carriers().is_empty());
        let cost = v.required_work_v1(e).unwrap();
        assert!(
            v.observe_v1(
                e,
                SourceId(1),
                *c,
                &mut WorkBudget::new(cost),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
    }
    #[test]
    fn cancellation_before_event_cannot_allocate_or_consume() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_CRC, &b);
        let mut v = view(&b);
        let Item::Event(e, c) = &run.items[0] else {
            panic!("carrier")
        };
        let before = b.used();
        assert_eq!(
            v.observe_v1(
                e,
                SourceId(1),
                *c,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(true)
            ),
            Err(Error::Cancelled)
        );
        assert_eq!(b.used(), before);
        assert!(v.physical_carriers().is_empty());
    }
    #[test]
    fn foreign_pool_carrier_cannot_be_a_source_witness() {
        let a = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_CRC, &a);
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut v = view(&b);
        let Item::Event(e, c) = &run.items[0] else {
            panic!("carrier")
        };
        assert_eq!(
            v.observe_v1(
                e,
                SourceId(1),
                *c,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            ),
            Err(Error::ForeignPool)
        );
    }
    #[test]
    fn decoded_pid_cannot_replace_actual_wire_pid() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_CRC, &b);
        let (p, c) = first(&run);
        let mut p = p.clone();
        p.pid = 4096;
        let mut v = view(&b);
        assert!(
            v.observe_v1(
                &TraceEvent::Transport(p),
                SourceId(1),
                c,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn decoded_payload_cannot_replace_physical_bytes() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_CRC, &b);
        let (p, c) = first(&run);
        let mut p = p.clone();
        p.payload = Some(Bytes::copy_from_slice(&b, b"different").unwrap());
        let mut v = view(&b);
        assert!(
            v.observe_v1(
                &TraceEvent::Transport(p),
                SourceId(1),
                c,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn repeated_source_ordinal_is_sticky() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let mut v = view(&b);
        feed(&mut v, &run.items[0], &mut run.input).unwrap();
        assert!(feed(&mut v, &run.items[0], &mut run.input).is_err());
        assert!(v.fault().is_some());
    }
    #[test]
    fn foreign_entity_is_not_the_original_source() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_CRC, &b);
        let mut v = view(&b);
        let Item::Event(e, c) = &run.items[0] else {
            panic!("carrier")
        };
        assert!(
            v.observe_v1(
                e,
                SourceId(2),
                *c,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn missing_physical_carrier_cannot_be_empty_completion() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let mut v = view(&b);
        let (p, c) = first(&run);
        let mut shifted = p.clone();
        shifted.source_offset += 188;
        shifted.payload_source_offset += 188;
        v.observe_v1(
            &TraceEvent::Transport(shifted),
            SourceId(1),
            c,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false),
        )
        .unwrap();
        for item in &run.items {
            if matches!(item, Item::End(_) | Item::Event(TraceEvent::Finished, _)) {
                feed(&mut v, item, &mut run.input).unwrap();
            }
        }
        assert!(finish(&mut v, &mut run.input).is_err());
        assert!(v.receipt().is_none());
    }
    #[test]
    fn actual_nonempty_source_pes_requires_real_scoped_observer() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BASIC, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        assert!(v.empty_refusal().is_some());
        assert!(finish(&mut v, &mut run.input).is_err());
    }
    #[test]
    fn unknown_pes_pid_does_not_select_empty_fallback() {
        let data = include_bytes!(
            "../tests/data/packet-completion-v1/unknown-pid-pes-after-pat-control.ts"
        );
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(data, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        assert!(finish(&mut v, &mut run.input).is_err());
    }
    #[test]
    fn unknown_continuation_without_pusi_is_still_uncertainty() {
        let data = include_bytes!(
            "../tests/data/packet-completion-v1/unknown-pid-continuation-after-control.ts"
        );
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(data, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        assert!(finish(&mut v, &mut run.input).is_err());
    }
    #[test]
    fn later_media_after_inactive_pat_is_not_dropped_as_control() {
        let data =
            include_bytes!("../tests/data/packet-completion-v1/inactive-pat-then-unrouted-pes.ts");
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(data, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        assert!(finish(&mut v, &mut run.input).is_err());
    }
    #[test]
    fn pes_signature_on_control_pid_is_an_explicit_refusal() {
        let data = include_bytes!("../tests/data/packet-completion-v1/pes-on-declared-pmt-pid.ts");
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(data, &b);
        let mut v = view(&b);
        assert!(all(&mut v, &mut run).is_err());
        assert!(v.receipt().is_none());
    }
    #[test]
    fn pmt_continuation_requires_its_actual_start() {
        let data = include_bytes!("../tests/data/packet-completion-v1/unanchored-pmt-control.ts");
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(data, &b);
        let mut v = view(&b);
        assert!(all(&mut v, &mut run).is_err());
    }
    #[test]
    fn incomplete_pmt_control_section_is_not_complete_empty() {
        let data =
            include_bytes!("../tests/data/packet-completion-v1/pmt-control-truncated-section.ts");
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(data, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        assert!(finish(&mut v, &mut run.input).is_err());
    }
    #[test]
    fn physical_truncated_tail_is_not_eof_coverage() {
        let data = include_bytes!("../tests/data/packet-completion-v1/physical-truncated-tail.ts");
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(data, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        assert!(finish(&mut v, &mut run.input).is_err());
    }
    #[test]
    fn rejected_unreadable_transport_is_not_control_only() {
        let data = include_bytes!("../tests/data/packet-completion-v1/tei-rejected-control.ts");
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(data, &b);
        let mut v = view(&b);
        let admission = all(&mut v, &mut run);
        assert!(admission.is_err() || finish(&mut v, &mut run.input).is_err());
        assert!(v.receipt().is_none());
    }
    #[test]
    fn later_transport_failure_with_same_eof_token_remains_failure() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        let token = run.input.token();
        let position = run.input.position();
        run.input
            .fail(SourceId(1), token.clone(), SourceFailure::Transport)
            .unwrap();
        assert_eq!(run.input.token(), token);
        assert_eq!(run.input.position(), position);
        assert_eq!(finish(&mut v, &mut run.input), Err(Error::Transport));
        assert!(v.receipt().is_none());
    }
    #[test]
    fn cancellation_after_end_does_not_publish_a_receipt() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        assert_eq!(
            v.confirm_no_media_v1(
                &mut run.input,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(true)
            ),
            Err(Error::Cancelled)
        );
        assert!(v.receipt().is_none());
        assert!(finish(&mut v, &mut run.input).unwrap().is_some());
    }
    #[test]
    fn actual_other_core_cursor_cannot_supply_end() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        let mut other = InputCursor::new(
            &b,
            SourceId(2),
            SourceCapabilities {
                length: Some(188),
                random_access: false,
            },
            4096,
        )
        .unwrap();
        assert!(finish(&mut v, &mut other).is_err());
    }
    #[test]
    fn source_finished_without_actual_physical_end_is_refused() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let mut v = view(&b);
        let item = run
            .items
            .iter()
            .find(|i| matches!(i, Item::Event(TraceEvent::Finished, _)))
            .unwrap();
        assert!(feed(&mut v, item, &mut run.input).is_err());
    }
    #[test]
    fn advertised_length_and_drained_bytes_are_not_eof() {
        for known in [false, true] {
            let b = Budget::new(8 * 1024 * 1024).unwrap();
            let mut input = InputCursor::new(
                &b,
                SourceId(1),
                SourceCapabilities {
                    length: known.then_some(188),
                    random_access: false,
                },
                4096,
            )
            .unwrap();
            let mut stream =
                TransportStream::new(b.clone(), PacketFormat::Ts188, ParseLimits::default(), 1)
                    .unwrap();
            let mut v = view(&b);
            let mut ordinal = 0;
            let mut supplied = false;
            let mut waiting = false;
            for _ in 0..500_000 {
                match stream
                    .poll(
                        &mut input,
                        &mut WorkBudget::new(17),
                        &AtomicBool::new(false),
                    )
                    .unwrap()
                {
                    Step::Ready(event) => {
                        let c = NativeSourceContext {
                            source_event_ordinal: ordinal,
                            epochs: stream.source_epochs(),
                            byte_run_epoch: None,
                        };
                        ordinal += 1;
                        let cost = v.required_work_v1(&event).unwrap();
                        v.observe_v1(
                            &event,
                            SourceId(1),
                            c,
                            &mut WorkBudget::new(cost),
                            &AtomicBool::new(false),
                        )
                        .unwrap();
                    }
                    Step::Yield => {}
                    Step::NeedInput(r) => {
                        if supplied {
                            waiting = true;
                            break;
                        }
                        input
                            .provide(
                                &InputWindow::new(
                                    r.source,
                                    r.token,
                                    r.offset,
                                    Bytes::copy_from_slice(&b, BAD_CRC).unwrap(),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                        supplied = true;
                    }
                    Step::End(_) => {
                        panic!("advertised length cannot establish actual terminal state")
                    }
                }
            }
            assert!(waiting);
            assert!(finish(&mut v, &mut input).is_err());
            assert!(v.receipt().is_none());
        }
    }
    #[test]
    fn source_finished_is_required_after_sealed_physical_end() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let mut v = view(&b);
        for item in &run.items {
            if matches!(item, Item::Event(TraceEvent::Finished, _)) {
                break;
            }
            feed(&mut v, item, &mut run.input).unwrap();
        }
        assert!(finish(&mut v, &mut run.input).is_err());
    }
    #[test]
    fn zero_end_and_terminal_work_is_retryable() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let mut v = view(&b);
        for item in &run.items {
            if let Item::End(end) = item {
                assert!(
                    !v.seal_physical_end_v1(
                        end,
                        &mut run.input,
                        &mut WorkBudget::new(0),
                        &AtomicBool::new(false)
                    )
                    .unwrap()
                );
                assert!(v.physical_end.is_none());
            }
            feed(&mut v, item, &mut run.input).unwrap();
        }
        assert!(
            v.confirm_no_media_v1(
                &mut run.input,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap()
            .is_none()
        );
        assert!(v.receipt().is_none());
        assert!(finish(&mut v, &mut run.input).unwrap().is_some());
    }
    #[test]
    fn duplicate_end_and_receipt_cannot_replay_completion() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let mut v = view(&b);
        all(&mut v, &mut run).unwrap();
        let end = run
            .items
            .iter()
            .find_map(|i| if let Item::End(e) = i { Some(e) } else { None })
            .unwrap();
        assert!(
            v.seal_physical_end_v1(
                end,
                &mut run.input,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        finish(&mut v, &mut run.input).unwrap().unwrap();
        assert!(finish(&mut v, &mut run.input).is_err());
    }
    #[test]
    fn bounded_retained_history_releases_all_new_owners() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(INACTIVE, &b);
        let before = b.used();
        let mut v = NativePacketCompletionV1::new(
            b.clone(),
            ParseLimits {
                source_spans: 1,
                ..ParseLimits::default()
            },
            1,
        )
        .unwrap();
        assert!(all(&mut v, &mut run).is_err());
        drop(v);
        assert_eq!(b.used(), before);
    }
    #[test]
    fn foreign_epoch_context_cannot_relabel_real_end() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut run = source(BAD_CRC, &b);
        let (p, mut c) = first(&run);
        c.epochs = SourceEpochs {
            clock: 9,
            configuration: 9,
        };
        let mut v = view(&b);
        v.observe_v1(
            &TraceEvent::Transport(p.clone()),
            SourceId(1),
            c,
            &mut WorkBudget::new(1_000_000),
            &AtomicBool::new(false),
        )
        .unwrap();
        let end = run
            .items
            .iter()
            .find_map(|i| if let Item::End(e) = i { Some(e) } else { None })
            .unwrap();
        assert!(
            v.seal_physical_end_v1(
                end,
                &mut run.input,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn real_transport_failure_before_core_end_is_not_eof() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut input = InputCursor::new(
            &b,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            4096,
        )
        .unwrap();
        let mut stream =
            TransportStream::new(b.clone(), PacketFormat::Ts188, ParseLimits::default(), 1)
                .unwrap();
        let mut v = view(&b);
        let mut ordinal = 0;
        let mut supplied = false;
        let mut failed = false;
        for _ in 0..500_000 {
            match stream
                .poll(
                    &mut input,
                    &mut WorkBudget::new(17),
                    &AtomicBool::new(false),
                )
                .unwrap()
            {
                Step::Ready(event) => {
                    let c = NativeSourceContext {
                        source_event_ordinal: ordinal,
                        epochs: stream.source_epochs(),
                        byte_run_epoch: None,
                    };
                    ordinal += 1;
                    let cost = v.required_work_v1(&event).unwrap();
                    v.observe_v1(
                        &event,
                        SourceId(1),
                        c,
                        &mut WorkBudget::new(cost),
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                }
                Step::Yield => {}
                Step::NeedInput(r) => {
                    if supplied {
                        input
                            .fail(r.source, r.token, SourceFailure::Transport)
                            .unwrap();
                        failed = true;
                        break;
                    }
                    input
                        .provide(
                            &InputWindow::new(
                                r.source,
                                r.token,
                                r.offset,
                                Bytes::copy_from_slice(&b, BAD_CRC).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    supplied = true;
                }
                Step::End(_) => panic!("transport failure never supplied an End"),
            }
        }
        assert!(failed);
        assert!(matches!(
            stream.poll(
                &mut input,
                &mut WorkBudget::new(4096),
                &AtomicBool::new(false)
            ),
            Err(Error::Transport)
        ));
        assert!(finish(&mut v, &mut input).is_err());
        assert!(v.receipt().is_none());
    }
    #[test]
    fn rejected_carrier_from_foreign_pool_cannot_escape_ownership() {
        let a = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(
            include_bytes!("../tests/data/transport-afc-reserved.ts"),
            &a,
        );
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut v = view(&b);
        let item = run
            .items
            .iter()
            .find(|i| matches!(i,Item::Event(TraceEvent::Diagnostic(f),_) if f.rejected.is_some()))
            .unwrap();
        let Item::Event(event, context) = item else {
            panic!("actual rejection")
        };
        assert_eq!(
            v.observe_v1(
                event,
                SourceId(1),
                *context,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            ),
            Err(Error::ForeignPool)
        );
    }
}
