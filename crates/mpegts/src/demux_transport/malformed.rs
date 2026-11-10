//! Explicit discards beside unchanged H.222.0 source refusals (2.4.3.3, 2.4.3.6).
//! The completion accounts for every carrier and the original cursor's EOF,
//! including empty physical input.
use super::{SourceContext, TransportFaultRecovery};
use crate::demux_pes::DemuxScope;
use crate::psi::DescriptorNamespace;
use crate::{
    FaultKind, MediaEntityEnd, ObservedTrace, PacketFormat, ParseLimits, TraceEvent, TraceFault,
    TransportPacket, TransportStream,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputToken, Result, SourceLocation, Step, WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MalformedInputKind {
    ReservedAdaptationControl,
    AdaptationLengthOverrun,
    PesStartCode,
    PesHeaderLength,
}
impl MalformedInputKind {
    fn error(self) -> Error {
        Error::Invalid(match self {
            Self::ReservedAdaptationControl => "reserved adaptation control",
            Self::AdaptationLengthOverrun => "adaptation length/control",
            Self::PesStartCode => "PES start code",
            Self::PesHeaderLength => "PES header exceeds length",
        })
    }
}

/// One reported discard. Rejected carriers stay in the unchanged source fault;
/// an accepted carrier with invalid PES syntax remains separately inspectable.
#[derive(Debug)]
pub struct MalformedInputFact {
    kind: MalformedInputKind,
    carrier: Option<TransportPacket>,
    input: SourceContext,
    fault: TraceFault,
    diagnostic: SourceContext,
}
impl MalformedInputFact {
    pub fn kind(&self) -> MalformedInputKind {
        self.kind
    }
    pub fn accepted_transport(&self) -> Option<&TransportPacket> {
        self.carrier.as_ref()
    }
    pub fn source_fault(&self) -> &TraceFault {
        &self.fault
    }
    pub fn input_context(&self) -> SourceContext {
        self.input
    }
    pub fn diagnostic_context(&self) -> SourceContext {
        self.diagnostic
    }
    pub fn source_bytes(&self) -> &Bytes {
        self.carrier.as_ref().map_or_else(
            || {
                self.fault
                    .rejected
                    .as_ref()
                    .expect("retained rejection")
                    .bytes()
            },
            |packet| &packet.source,
        )
    }
}

/// Apply to the original event: forward it unchanged, await its real diagnostic,
/// or deliver a reported discard. This never represents a codec/PES packet.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "original charged carriers remain inline"
)]
pub enum MalformedSourceDisposition {
    Forward,
    AwaitDiagnostic,
    Reported(MalformedInputFact),
}

/// An event issued by the completion's source advance. It borrows that
/// operation and its cursor until the caller has handled the event.
#[derive(Debug)]
pub struct MalformedSourceEvent<'a> {
    completion: &'a mut MalformedInputCompletion,
    input: &'a mut InputCursor,
    event: TraceEvent,
    context: SourceContext,
}
impl MalformedSourceEvent<'_> {
    pub fn event(&self) -> &TraceEvent {
        &self.event
    }
    pub fn context(&self) -> SourceContext {
        self.context
    }
    pub fn input(&self) -> &InputCursor {
        self.input
    }
    pub fn clock_is_quarantined(&self) -> bool {
        self.completion.stream.clock_is_quarantined()
    }

    /// Stage this source's terminal diagnostic in its transport observer.
    pub fn hold_terminal_transport(&self, observer: &mut TransportFaultRecovery) -> Result<()> {
        let origin = self.completion.stream.trace_origin(self.input)?;
        observer.hold_terminal_diagnostic(&self.event, &origin, self.context)
    }
    pub fn required_work(&self) -> Result<u64> {
        MalformedInputCompletion::required_work(&self.event)
    }
    /// Apply the policy to this issued event. A short grant leaves it available
    /// for retry. Forwarded events remain available through [`Self::event`].
    pub fn observe(
        &mut self,
        observer: Option<&mut TransportFaultRecovery>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<MalformedSourceDisposition>> {
        self.completion.observe_source(
            &self.event,
            self.context,
            self.input,
            observer,
            work,
            cancel,
        )
    }
    pub fn into_event(self) -> TraceEvent {
        self.event
    }
}

/// Completion after fully accounted controls/discards, not absence of media.
/// Malformed facts were delivered individually through `Reported`.
#[derive(Debug)]
pub struct NoPacketsConfirmation {
    end: MediaEntityEnd,
    finished: SourceContext,
    carriers: u64,
    discarded: u64,
}
impl NoPacketsConfirmation {
    pub fn physical_end(&self) -> SourceLocation {
        self.end.location()
    }
    pub fn source_finished(&self) -> SourceContext {
        self.finished
    }
    pub fn physical_carriers(&self) -> u64 {
        self.carriers
    }
    pub fn discarded_carriers(&self) -> u64 {
        self.discarded
    }
}

#[derive(Debug)]
struct Control {
    packet: TransportPacket,
    section: Option<Bytes>,
}
#[derive(Debug)]
struct Pending {
    packet: TransportPacket,
    context: SourceContext,
    kind: MalformedInputKind,
}

/// Opt into reported malformed input, keeping the strict source view unchanged.
///
/// One TS188 entity, a real selected single-AVC PMT, drained assembly, and
/// self-contained PAT/PMT control sections are admitted. Invalid transport and
/// PES headers are discarded only with their actual source diagnostics. Other
/// selected payload still goes to the observer; it prevents zero-packet proof.
/// Construction binds the original core cursor before its first byte. Advance
/// the source with [`Self::poll_source`] and observe each issued event before
/// advancing again. Seal physical End and signal logical EOF on this operation,
/// then observe its Finished event. EOF confirmation checks the original cursor.
#[derive(Debug)]
pub struct MalformedInputCompletion {
    stream: TransportStream,
    pool: Budget,
    token: InputToken,
    source: triergon_core::SourceId,
    program: u16,
    format: PacketFormat,
    limits: ParseLimits,
    next_ordinal: u64,
    next_offset: u64,
    last: Option<SourceContext>,
    carriers: u64,
    discarded: u64,
    control: Option<Control>,
    pmt_pid: Option<u16>,
    scope: Option<DemuxScope>,
    pending: Option<Pending>,
    after_discard: bool,
    blocked: bool,
    end: Option<MediaEntityEnd>,
    finished: Option<SourceContext>,
    confirmed: bool,
}
impl MalformedInputCompletion {
    pub fn new(
        input: &InputCursor,
        format: PacketFormat,
        limits: ParseLimits,
        program: u16,
    ) -> Result<Self> {
        if program == 0 {
            return Err(Error::Invalid(
                "malformed completion must bind before input",
            ));
        }
        Self::new_with_namespace(
            input,
            format,
            limits,
            program,
            DescriptorNamespace::Unspecified,
        )
    }
    pub fn new_with_namespace(
        input: &InputCursor,
        format: PacketFormat,
        limits: ParseLimits,
        program: u16,
        namespace: DescriptorNamespace,
    ) -> Result<Self> {
        if input.position() != 0 {
            return Err(Error::Invalid(
                "malformed completion must bind before input",
            ));
        }
        Ok(Self {
            stream: TransportStream::new_with_namespace(
                input.budget().clone(),
                format,
                limits,
                program,
                namespace,
            )?,
            pool: input.budget().clone(),
            token: input.token(),
            source: input.source(),
            program,
            format,
            limits,
            next_ordinal: 0,
            next_offset: 0,
            last: None,
            carriers: 0,
            discarded: 0,
            control: None,
            pmt_pid: None,
            scope: None,
            pending: None,
            after_discard: false,
            blocked: format != PacketFormat::Ts188,
            end: None,
            finished: None,
            confirmed: false,
        })
    }
    pub fn discarded_carriers(&self) -> u64 {
        self.discarded
    }
    pub fn source_epochs(&self) -> crate::SourceEpochs {
        self.stream.source_epochs()
    }
    /// Bind the observed logical EOF to the original source parser.
    pub fn source_finished(&self, input: &InputCursor) -> Result<ObservedTrace<()>> {
        self.identity(input)?;
        if self.finished.is_none() {
            return Err(Error::Invalid(
                "malformed completion missing source Finished",
            ));
        }
        self.stream.source_finished(input)
    }

    /// Advance the bound source and issue its original event with the context
    /// captured by that advance.
    pub fn poll_source<'a>(
        &'a mut self,
        input: &'a mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<MalformedSourceEvent<'a>>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.identity(input)?;
        Ok(match self.stream.poll(input, work, cancel)? {
            Step::Ready(event) => {
                let context = SourceContext {
                    source_event_ordinal: self.next_ordinal,
                    epochs: self.source_epochs(),
                    byte_run_epoch: None,
                };
                self.next_ordinal = self.next_ordinal.checked_add(1).ok_or(Error::Overflow)?;
                Step::Ready(MalformedSourceEvent {
                    completion: self,
                    input,
                    event,
                    context,
                })
            }
            Step::NeedInput(request) => Step::NeedInput(request),
            Step::Yield => Step::Yield,
            Step::End(location) => Step::End(location),
        })
    }
    fn required_work(event: &TraceEvent) -> Result<u64> {
        let entries = match event {
            TraceEvent::Pat(table) => table.programs.len(),
            _ => 0,
        };
        u64::try_from(entries)
            .map_err(|_| Error::Overflow)?
            .checked_add(4096)
            .ok_or(Error::Overflow)
    }
    fn identity(&self, input: &InputCursor) -> Result<()> {
        if !self.pool.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        if self.source != input.source() {
            return Err(Error::SourceMismatch);
        }
        let received = input.token();
        if self.token.generation() != received.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: received.generation(),
            });
        }
        if self.token.request() != received.request() {
            return Err(Error::StaleRequest {
                expected: self.token.request(),
                received: received.request(),
            });
        }
        Ok(())
    }
    fn coverage(&mut self, offset: u64) -> Result<()> {
        if offset < self.next_offset {
            return Err(Error::Invalid(
                "malformed completion physical coverage order",
            ));
        }
        if offset != self.next_offset {
            self.blocked = true;
        }
        self.next_offset = offset
            .checked_add(self.format.bytes() as u64)
            .ok_or(Error::Overflow)?;
        self.carriers = self.carriers.checked_add(1).ok_or(Error::Overflow)?;
        Ok(())
    }
    fn retire_control(&mut self) {
        if let Some(control) = self.control.take()
            && control.section.is_none()
        {
            self.blocked = true;
        }
    }
    fn observer<'a>(
        &self,
        observer: Option<&'a mut TransportFaultRecovery>,
        context: SourceContext,
    ) -> Result<&'a mut TransportFaultRecovery> {
        let observer = observer.ok_or(Error::Unsupported(
            "malformed completion needs source-selected AVC",
        ))?;
        if self.scope != Some(observer.scope())
            || context.epochs != observer.scope().epochs
            || context.byte_run_epoch.is_some()
            || observer.pending.is_some()
            || observer.boundaries.input_error().is_some()
            || !observer.inner.recovery_idle()
        {
            return Err(Error::Unsupported(
                "malformed completion requires drained source scope",
            ));
        }
        Ok(observer)
    }
    fn carrier(
        &mut self,
        packet: &TransportPacket,
        context: SourceContext,
        observer: Option<&mut TransportFaultRecovery>,
        cancel: &AtomicBool,
    ) -> Result<MalformedSourceDisposition> {
        if self.pending.is_some() {
            return Err(Error::Invalid("malformed PES diagnostic missing"));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        // poll_source issues this carrier from the bound stream's strict decode.
        let selected = self.scope.is_some_and(|scope| scope.pid == packet.pid);
        if selected && self.after_discard && packet.payload.is_some() && !packet.payload_unit_start
        {
            return Err(Error::Unsupported(
                "PES continuation after malformed discard",
            ));
        }
        let control = !packet.transport_error
            && packet.scrambling == 0
            && (packet.payload.is_none() || (packet.is_null() && !packet.payload_unit_start));
        let psi = packet.pid == 0 || self.pmt_pid == Some(packet.pid);
        let kind = if !control
            && !psi
            && selected
            && !packet.transport_error
            && packet.scrambling == 0
            && packet.payload_unit_start
            && packet.prefix.is_none()
            && !packet.discontinuity_indicator()
            && packet
                .adaptation
                .as_ref()
                .is_none_or(|a| a.pcr.is_none() && a.opcr.is_none())
        {
            packet.payload.as_ref().and_then(|payload| {
                let raw = payload.as_ref();
                if raw.len() < 6 || raw[3] != 0xe0 {
                    None
                } else if raw[..3] != [0, 0, 1] {
                    Some(MalformedInputKind::PesStartCode)
                } else if raw.len() >= 9 && raw[6] & 0xc0 == 0x80 {
                    let size = usize::from(u16::from_be_bytes([raw[4], raw[5]]));
                    (size != 0 && 9 + usize::from(raw[8]) > size + 6)
                        .then_some(MalformedInputKind::PesHeaderLength)
                } else {
                    None
                }
            })
        } else {
            None
        };
        if let Some(kind) = kind {
            let inner = self.observer(observer, context)?;
            inner.inner.transport_fault_input(packet, context, None)?;
            self.coverage(packet.source_offset)?;
            self.retire_control();
            self.pending = Some(Pending {
                packet: packet.clone(),
                context,
                kind,
            });
            return Ok(MalformedSourceDisposition::AwaitDiagnostic);
        }
        self.coverage(packet.source_offset)?;
        self.retire_control();
        if selected && packet.payload_unit_start && packet.payload.is_some() {
            self.after_discard = false;
        }
        if !control && psi {
            self.control = Some(Control {
                packet: packet.clone(),
                section: None,
            });
        } else if !control {
            self.blocked = true;
        }
        Ok(MalformedSourceDisposition::Forward)
    }
    fn section(&mut self, section: &crate::psi::Section) {
        let Some(control) = self.control.as_mut() else {
            self.blocked = true;
            return;
        };
        let Some(payload) = control.packet.payload.as_ref() else {
            self.blocked = true;
            return;
        };
        let raw = payload.as_ref();
        let expected = if control.packet.pid == 0 { 0 } else { 2 };
        let bytes = section.bytes().as_ref();
        let ranges = section.source_ranges();
        let matches = control.packet.payload_unit_start
            && section.pid() == control.packet.pid
            && raw.first() == Some(&0)
            && bytes.first() == Some(&expected)
            && bytes.len() < raw.len()
            && bytes == &raw[1..1 + bytes.len()]
            && raw[1 + bytes.len()..].iter().all(|b| *b == 0xff)
            && section.crc_remainder() == Some(0)
            && ranges.len() == 1
            && ranges[0].entity == self.source
            && ranges[0].offset == control.packet.payload_source_offset + 1
            && ranges[0].bytes == bytes.len();
        if matches {
            control.section = Some(section.bytes().clone());
        } else {
            self.blocked = true;
        }
    }
    fn diagnostic(
        &mut self,
        fault: &TraceFault,
        context: SourceContext,
        observer: Option<&mut TransportFaultRecovery>,
    ) -> Result<MalformedSourceDisposition> {
        if let Some(pending) = self.pending.as_ref() {
            if fault.kind != FaultKind::Pes
                || fault.error != pending.kind.error()
                || fault.rejected.is_some()
                || fault.pid != Some(pending.packet.pid)
                || fault.location.source != self.source
                || fault.location.offset != pending.packet.source_offset
                || context.epochs != pending.context.epochs
                || context.source_event_ordinal <= pending.context.source_event_ordinal
                || context.byte_run_epoch.is_some()
            {
                return Err(Error::Invalid(
                    "malformed completion source diagnostic mismatch",
                ));
            }
            let observer = self.observer(observer, context)?;
            let pending = self.pending.take().expect("matching malformed diagnostic");
            observer.inner.note_recovered(
                pending.packet.continuity_counter,
                pending.packet.source_offset,
                context.source_event_ordinal,
            );
            self.discarded = self.discarded.checked_add(1).ok_or(Error::Overflow)?;
            self.after_discard = true;
            return Ok(MalformedSourceDisposition::Reported(MalformedInputFact {
                kind: pending.kind,
                carrier: Some(pending.packet),
                input: pending.context,
                fault: fault.clone(),
                diagnostic: context,
            }));
        }
        let Some(rejected) = fault.rejected.as_ref() else {
            self.blocked = true;
            return Ok(MalformedSourceDisposition::Forward);
        };
        let raw = rejected.bytes().as_ref();
        let kind = if raw.len() == 188 && raw[0] == 0x47 && raw[1] & 0x80 == 0 && raw[3] & 0xc0 == 0
        {
            match (raw[3] >> 4) & 3 {
                0 => Some(MalformedInputKind::ReservedAdaptationControl),
                3 if raw[4] > 183 => Some(MalformedInputKind::AdaptationLengthOverrun),
                _ => None,
            }
        } else {
            None
        };
        let Some(kind) = kind else {
            self.blocked = true;
            if rejected.source_id() == Some(self.source)
                && rejected.format() == self.format
                && rejected.bytes().len() == self.format.bytes()
            {
                self.coverage(rejected.source_offset())?;
            }
            return Ok(MalformedSourceDisposition::Forward);
        };
        if fault.kind != FaultKind::Transport
            || fault.error != kind.error()
            || rejected.error() != fault.error
            || rejected.secondary_error().is_some()
            || rejected.format() != PacketFormat::Ts188
            || fault.location.source != self.source
            || rejected.source_id() != Some(self.source)
            || rejected.source_offset() != fault.location.offset
            || self
                .scope
                .is_none_or(|scope| scope.pid != (u16::from(raw[1] & 31) << 8 | u16::from(raw[2])))
            || fault.location.offset != self.next_offset
        {
            return Err(Error::Invalid(
                "malformed completion rejected source mismatch",
            ));
        }
        let observer = self.observer(observer, context)?;
        observer.inner.rejected_fault_input(rejected, context)?;
        self.coverage(fault.location.offset)?;
        self.retire_control();
        observer.inner.note_recovered(
            raw[3] & 15,
            fault.location.offset,
            context.source_event_ordinal,
        );
        self.discarded = self.discarded.checked_add(1).ok_or(Error::Overflow)?;
        self.after_discard = true;
        Ok(MalformedSourceDisposition::Reported(MalformedInputFact {
            kind,
            carrier: None,
            input: context,
            fault: fault.clone(),
            diagnostic: context,
        }))
    }
    fn observe_source(
        &mut self,
        event: &TraceEvent,
        context: SourceContext,
        input: &mut InputCursor,
        observer: Option<&mut TransportFaultRecovery>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<MalformedSourceDisposition>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let quarantined = self.stream.clock_is_quarantined();
        if !quarantined && let Some(observer) = observer.as_ref() {
            if !observer.inner.recovery_budget().same_pool(input.budget()) {
                return Err(Error::ForeignPool);
            }
            observer.check_source_input(input)?;
        }
        // Check sticky I/O failure even when no new bytes are requested.
        input.read_into(&mut [], &mut WorkBudget::new(0), cancel)?;
        if self.confirmed
            || self.finished.is_some()
            || context.byte_run_epoch.is_some()
            || self.last.is_some_and(|last| {
                context.source_event_ordinal <= last.source_event_ordinal
                    || context.epochs.clock < last.epochs.clock
                    || context.epochs.configuration < last.epochs.configuration
            })
        {
            return Err(Error::Invalid("malformed completion source event order"));
        }
        if !work.try_charge(Self::required_work(event)?) {
            return Ok(None);
        }
        if quarantined {
            self.blocked = true;
            self.last = Some(context);
            return Ok(Some(MalformedSourceDisposition::Forward));
        }
        let expected = self.last.map_or(0, |last| last.source_event_ordinal + 1);
        if context.source_event_ordinal != expected {
            self.blocked = true;
        }
        let physical = match event {
            TraceEvent::Transport(packet) => Some((packet.source_offset, packet.source.len())),
            TraceEvent::Diagnostic(fault) => fault
                .rejected
                .as_ref()
                .map(|rejected| (rejected.source_offset(), rejected.bytes().len())),
            _ => None,
        };
        if let Some((offset, size)) = physical
            && (self.end.is_some()
                || offset.checked_add(size as u64).ok_or(Error::Overflow)? > input.position())
        {
            return Err(Error::Invalid(
                "malformed completion carrier outside original input progress",
            ));
        }
        let result = match event {
            TraceEvent::Transport(packet) => self.carrier(packet, context, observer, cancel)?,
            TraceEvent::Diagnostic(fault) => self.diagnostic(fault, context, observer)?,
            TraceEvent::Section(section) => {
                self.section(section);
                MalformedSourceDisposition::Forward
            }
            TraceEvent::Pat(table) => {
                let literal = self
                    .control
                    .as_ref()
                    .and_then(|c| c.section.as_ref())
                    .is_some_and(|s| {
                        table.source_sections.len() == 1
                            && s.as_ref() == table.source_sections.as_slice()[0].as_ref()
                    });
                if literal
                    && table.header.current_next
                    && table.header.last_section_number == 0
                    && table.programs.len() <= self.limits.programs
                {
                    self.pmt_pid = table
                        .programs
                        .as_slice()
                        .iter()
                        .find(|p| p.program_number == self.program)
                        .map(|p| p.pid);
                } else {
                    self.blocked = true;
                }
                MalformedSourceDisposition::Forward
            }
            TraceEvent::ProgramMap {
                map, active: true, ..
            } => {
                let literal = self
                    .control
                    .as_ref()
                    .and_then(|c| c.section.as_ref())
                    .is_some_and(|s| s.as_ref() == map.source.as_ref());
                if literal
                    && map.header.extension == self.program
                    && map.streams.len() == 1
                    && map.streams.as_slice()[0].stream_type == 0x1b
                {
                    self.scope = Some(DemuxScope {
                        program: self.program,
                        pid: map.streams.as_slice()[0].pid,
                        stream_type: 0x1b,
                        stream_id: 0xe0,
                        entity: self.source,
                        epochs: context.epochs,
                    });
                } else {
                    self.blocked = true;
                }
                MalformedSourceDisposition::Forward
            }
            TraceEvent::Pes(_) | TraceEvent::Discontinuity { .. } | TraceEvent::Timeline { .. } => {
                self.blocked = true;
                MalformedSourceDisposition::Forward
            }
            TraceEvent::Finished => {
                let end = self.end.as_ref().ok_or(Error::Invalid(
                    "malformed completion Finished without physical End",
                ))?;
                if end.epochs != context.epochs {
                    return Err(Error::Invalid("malformed completion Finished epochs"));
                }
                self.finished = Some(context);
                MalformedSourceDisposition::Forward
            }
            _ => MalformedSourceDisposition::Forward,
        };
        self.last = Some(context);
        Ok(Some(result))
    }
    pub fn seal_physical_end(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<MediaEntityEnd>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.identity(input)?;
        if self.end.is_some() || self.pending.is_some() {
            return Err(Error::Invalid(
                "malformed completion original physical End mismatch",
            ));
        }
        let Some(end) = self.stream.media_entity_end(input, work, cancel)? else {
            return Ok(None);
        };
        if (!self.blocked && end.location.offset != self.next_offset)
            || self.last.is_some_and(|c| c.epochs != end.epochs)
        {
            return Err(Error::Invalid(
                "malformed completion original physical End mismatch",
            ));
        }
        self.retire_control();
        self.end = Some(end.clone());
        Ok(Some(end))
    }
    /// Start the logical drain after sealing this operation's physical End.
    pub fn end_of_source(&mut self) -> Result<()> {
        if self.end.is_none() {
            return Err(Error::Invalid("malformed completion missing physical End"));
        }
        self.stream.end_of_source()
    }
    /// Complete only the explicitly classified zero-packet branch. A real empty
    /// physical source is allowed, without inventing a PAT, PMT or PES scope.
    pub fn confirm_no_packets(
        &mut self,
        observer: Option<&mut TransportFaultRecovery>,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<NoPacketsConfirmation>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.identity(input)?;
        let end = self
            .end
            .as_ref()
            .ok_or(Error::Invalid("malformed completion missing physical End"))?;
        let finished = self.finished.ok_or(Error::Invalid(
            "malformed completion missing source Finished",
        ))?;
        if self.confirmed
            || self.blocked
            || self.pending.is_some()
            || (self.next_offset != 0 && (self.discarded == 0 || self.scope.is_none()))
        {
            return Err(Error::Unsupported(
                "malformed completion has unproved packet work",
            ));
        }
        if self.next_offset != 0 {
            let observer = self.observer(observer, finished)?;
            if !observer.inner.recovery_budget().same_pool(input.budget()) {
                return Err(Error::ForeignPool);
            }
            observer.check_source_input(input)?;
        }
        if input.position() != end.location.offset
            || end.token != self.token
            || self.next_offset != end.location.offset
        {
            return Err(Error::Invalid("malformed completion EOF moved"));
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        match input.read_into(&mut [0; 1], &mut WorkBudget::new(1), cancel)? {
            Step::End(actual) if actual == end.location => (),
            _ => {
                return Err(Error::Invalid(
                    "malformed completion requires real core EOF",
                ));
            }
        }
        let confirmation = NoPacketsConfirmation {
            end: end.clone(),
            finished,
            carriers: self.carriers,
            discarded: self.discarded,
        };
        self.confirmed = true;
        Ok(Some(confirmation))
    }
}
