//! The confirmation that an input carried only control traffic (H.222.0 (08/2023) §§2.4.3.3
//! and 2.4.4 tell control PIDs and sections from PES payload). Any other payload needs a
//! scoped [`DemuxObserver`](super::DemuxObserver) and rules the confirmation out.
use crate::compat::pes::observer::SourceContext;
use crate::pes::START_CODE_PREFIX;
use crate::psi::{PatReader, Section, SectionAssembler, TableStep};
use crate::stream::entity::{ObservedTrace, TraceOrigin};
use crate::stream::{FaultKind, MediaEntityEnd, TraceEvent};
use crate::transport::{TransportInspection, TransportInspector, TransportPacket, pid};
use crate::{AssemblyStep, PacketFormat, ParseLimits};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, Result, SourceId, SourceLocation, Step,
    WorkBudget,
};

/// One physical carrier admitted by [`PacketCompletion`]: the PSI sections it carries are
/// checked against its bytes.
#[derive(Debug)]
struct PhysicalCarrier {
    packet: TransportPacket,
    context: SourceContext,
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
/// Confirmation that one completely covered physical input contained only control traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoMediaConfirmation {
    source: SourceId,
    source_bytes: u64,
    physical_carriers: usize,
    source_finished: SourceContext,
}
impl NoMediaConfirmation {
    /// The source entity the confirmation covers.
    pub fn source(self) -> SourceId {
        self.source
    }
    /// Length in bytes of the covered input: the offset of its physical end.
    pub fn source_bytes(self) -> u64 {
        self.source_bytes
    }
    /// Number of physical carriers, null runs counted per carrier.
    pub fn physical_carriers(self) -> usize {
        self.physical_carriers
    }
    /// Event ordinal and epochs at the parser's confirmed logical EOF.
    pub fn source_finished(self) -> SourceContext {
        self.source_finished
    }
}
/// Confirms that one physical input carried only control traffic: PAT, PMT, null and no-payload
/// carriers.
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
    /// PAT sections admitted; a route's association is the count when its PAT arrived.
    pat_facts: usize,
    routes: BudgetVec<ControlRoute>,
    diagnostics: usize,
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
    /// A TS188 completion for `program`, charging its evidence to `budget`; see
    /// [`new_for_format`](Self::new_for_format) for M2TS.
    ///
    /// Fails with `Invalid` for program 0 or invalid `limits`.
    pub fn new(budget: Budget, limits: ParseLimits, program: u16) -> Result<Self> {
        if program == 0 {
            return Err(Error::Invalid("packet completion selected program"));
        }
        limits.validate()?;
        Ok(Self {
            carriers: BudgetVec::new(&budget),
            pat_facts: 0,
            routes: BudgetVec::new(&budget),
            diagnostics: 0,
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
    #[cfg(test)]
    fn confirmation(&self) -> Option<NoMediaConfirmation> {
        self.confirmation
    }
    /// Track control-only completion for either TS188 or M2TS192. The selected program must
    /// be nonzero and the parser limits must be valid, as for [`Self::new`].
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
    #[cfg(test)]
    fn empty_refusal(&self) -> Option<Error> {
        self.no_media_blocker
    }
    /// Work units [`observe_issued`](Self::observe_issued) charges to admit `event`; a smaller
    /// grant
    /// leaves the event unconsumed.
    ///
    /// Fails with `Overflow` when the count does not fit in a `u64`.
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
        if self.carriers.len() >= self.limits.source_spans {
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
        // PAT, null and no-payload carriers are control traffic; a PMT PID's carriers must
        // assemble into that program's PMT sections; any other payload needs a scoped media
        // observer and prevents a no-media confirmation.
        if actual.pid == pid::PAT {
            self.pat_pid_seen = true;
            // A PES start at the control payload start is not silently ignored.
            if actual.payload_unit_start
                && actual
                    .payload
                    .as_ref()
                    .is_some_and(|b| b.as_ref().starts_with(&START_CODE_PREFIX))
            {
                self.block_empty(Error::Unsupported(
                    "packet completion ambiguous PES start on PAT PID",
                ));
            }
        } else if actual.payload.is_none()
            || (actual.pid == pid::NULL && !actual.payload_unit_start)
        {
            // Null and no-payload carriers.
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
            if actual.payload_unit_start && body.as_ref().starts_with(&START_CODE_PREFIX) {
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
        } else {
            self.block_empty(Error::Unsupported(
                "packet completion physical payload needs validated PES observer",
            ));
        }
        self.carriers
            .try_push(PhysicalCarrier {
                packet: actual,
                context,
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
        if self.pat_facts >= self.limits.source_spans {
            return Err(Error::Unsupported("packet completion PAT-fact limit"));
        }
        let mut reader = PatReader::new(section, &self.budget, self.limits)?;
        let TableStep::Output(decoded) = reader.poll(work, cancel)? else {
            return Err(Error::Invalid(
                "packet completion reserved PAT work exhausted",
            ));
        };
        let association = self.pat_facts;
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
        // An inactive or incomplete PAT associates its PMT PIDs too: a raw association
        // fact, never a scope.
        self.pat_facts += 1;
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
                if self.diagnostics >= self.limits.source_spans {
                    return Err(Error::Unsupported("packet completion diagnostic limit"));
                }
                if fault.kind != FaultKind::Psi && fault.kind != FaultKind::Continuity {
                    self.block_empty(Error::Unsupported(
                        "packet completion source failure/transport/media uncertainty",
                    ));
                }
                self.diagnostics += 1;
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

    /// Admit one event from the bound parser, cursor and source epoch. `Ok(false)`
    /// leaves it available for retry with a larger work grant. A mismatched event
    /// is rejected, and errors after admission stop the completion proof.
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
    /// Work units that [`seal_physical_end`](Self::seal_physical_end) and
    /// [`confirm_no_media`](Self::confirm_no_media) each charge.
    ///
    /// Fails with `Overflow` when the count does not fit in a `u64`.
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
    // Recheck physical EOF after the parser has issued Finished.
    fn validate_finished(
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
            physical_carriers: self.carriers.len(),
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
