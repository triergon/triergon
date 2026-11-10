//! Source-confirmed opaque AVC recovery for one TS188 entity.
//!
//! A caller can replay a strict source refusal through a fresh sequential
//! cursor. The proof retains the original transport observations and admits
//! one unframed byte run only after the source reaches physical End and
//! Finished. The reference is FFmpeg revision `d32b387f` on the 9.0 branch.

use super::pes::{
    DemuxBoundary, DemuxObserver, DemuxPacket, DemuxPayloadKind, DemuxProfile, DemuxScope,
    DemuxSpan, PesFaultPolicy, RegisteredCarriage, SourceContext, TransportFaultRecovery,
    TransportObservation,
};
use crate::psi::{DescriptorNamespace, ProgramMap};
use crate::{
    AssemblyStep, FaultKind, MediaEntityEnd, ObservedTrace, PacketFormat, ParseLimits, TraceEvent,
    TraceFault, TransportStream,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputToken, Result, SourceLocation, Step, WorkBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proof {
    PacketFault,
    FinalCarrier,
    FiniteEof,
    FiniteOverrun,
}

/// The original source diagnostic to reproduce during recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryTrigger {
    kind: FaultKind,
    error: Error,
    location: SourceLocation,
    pid: Option<u16>,
    proof: Proof,
}

impl RecoveryTrigger {
    /// Select a diagnostic admitted by the bounded recovery policy. A
    /// successful proof must reproduce it from the replay's own source.
    pub fn from_fault(fault: &TraceFault) -> Option<Self> {
        let proof = match (fault.kind, fault.error) {
            (
                FaultKind::Transport,
                Error::Invalid(
                    "TS transport error indicator"
                    | "PCR reserved bits/extension"
                    | "OPCR without PCR",
                )
                | Error::Unsupported("scrambled TS payload"),
            )
            | (
                FaultKind::Pes,
                Error::Invalid("PES timestamp markers" | "forbidden PES time flags"),
            ) => Proof::PacketFault,
            (FaultKind::Transport, Error::Truncated | Error::Invalid("TS sync byte")) => {
                Proof::FinalCarrier
            }
            (FaultKind::Pes, Error::Truncated) => Proof::FiniteEof,
            (FaultKind::Pes, Error::Invalid("bytes beyond finite PES")) => Proof::FiniteOverrun,
            _ => return None,
        };
        Some(Self {
            kind: fault.kind,
            error: fault.error,
            location: fault.location,
            pid: fault.pid,
            proof,
        })
    }
}

/// A proved opaque byte run with its original source and PES observations.
/// The charged spans remain valid after the recovery and cursor are dropped.
#[derive(Debug)]
pub struct RecoveredOpaque {
    fault: TraceFault,
    diagnostic: SourceContext,
    end: MediaEntityEnd,
    finished: SourceContext,
    observations: BudgetVec<TransportObservation>,
    bytes: usize,
}

impl RecoveredOpaque {
    /// The source diagnostic reproduced by the recovery.
    pub fn source_fault(&self) -> &TraceFault {
        &self.fault
    }

    /// Context of the reproduced diagnostic.
    pub fn diagnostic_context(&self) -> SourceContext {
        self.diagnostic
    }

    /// The issuing source's drained physical End.
    pub fn physical_end(&self) -> &MediaEntityEnd {
        &self.end
    }

    /// Context of the issuing source's Finished event.
    pub fn source_finished(&self) -> SourceContext {
        self.finished
    }

    /// Original typed observations in demux order. The first supplies the
    /// compatibility position and clock for this unframed run.
    pub fn observations(&self) -> &[TransportObservation] {
        self.observations.as_slice()
    }

    /// Payload bytes across all retained spans.
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.observations()
            .iter()
            .flat_map(spans)
            .map(AsRef::as_ref)
    }

    /// Length of the opaque byte run.
    pub fn payload_len(&self) -> usize {
        self.bytes
    }
}

fn spans(packet: &TransportObservation) -> &[DemuxSpan] {
    match packet {
        TransportObservation::Accepted(packet) => packet.spans.as_slice(),
        TransportObservation::Faulted(packet) => packet.spans.as_slice(),
        TransportObservation::PesFault(packet) => packet.spans.as_slice(),
    }
}

#[derive(Debug)]
struct Scan {
    packet: TransportObservation,
    span: usize,
    offset: usize,
    retain_work: u64,
}

impl Scan {
    fn poll(
        &mut self,
        zeros: &mut u8,
        length: &mut usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while let Some(span) = spans(&self.packet).get(self.span) {
            check_cancel(cancel)?;
            let bytes = span.as_ref();
            if self.offset == bytes.len() {
                self.span += 1;
                self.offset = 0;
                continue;
            }
            let count = work.take((bytes.len() - self.offset) as u64) as usize;
            if count == 0 {
                return Ok(false);
            }
            let end = self.offset + count;
            for &byte in &bytes[self.offset..end] {
                if byte == 1 && *zeros == 2 {
                    return Err(Error::Unsupported("recovery contains Annex-B video"));
                }
                *zeros = if byte == 0 { (*zeros + 1).min(2) } else { 0 };
            }
            self.offset = end;
            *length = length.checked_add(count).ok_or(Error::Overflow)?;
        }
        Ok(true)
    }
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one original event or observation stays inline"
)]
enum Phase {
    Read,
    Route(TraceEvent, SourceContext),
    Drain,
    Scan(Scan),
    SealEnd,
    SealTail,
    Finish(SourceContext),
    Publish,
    Done(SourceLocation),
}

/// A cooperative proof bound to one fresh sequential TS188 cursor.
///
/// Supply requested windows and real EOF on that cursor. Polling advances the
/// cursor and preserves the current event or span across short work grants.
/// Source, policy and allocation errors end the proof. A poll rejected before
/// advancing for cancellation or a foreign cursor leaves the proof intact.
#[derive(Debug)]
pub struct OpaqueRecovery {
    source: TransportStream,
    observer: Option<TransportFaultRecovery>,
    budget: Budget,
    token: InputToken,
    position: u64,
    trigger: RecoveryTrigger,
    program: u16,
    limits: ParseLimits,
    phase: Phase,
    fault: Option<Error>,
    ordinal: u64,
    diagnostic: Option<(TraceFault, SourceContext)>,
    fault_count: usize,
    end: Option<MediaEntityEnd>,
    finished: Option<SourceContext>,
    observations: BudgetVec<TransportObservation>,
    bytes: usize,
    zeros: u8,
}

impl OpaqueRecovery {
    /// Bind the replay before it consumes the first byte. The original
    /// diagnostic's source ID must match this cursor.
    pub fn new(
        input: &InputCursor,
        limits: ParseLimits,
        program: u16,
        namespace: DescriptorNamespace,
        trigger: RecoveryTrigger,
    ) -> Result<Self> {
        if input.source() != trigger.location.source {
            return Err(Error::SourceMismatch);
        }
        if input.position() != 0 || input.capabilities().random_access {
            return Err(Error::Invalid(
                "opaque recovery requires a fresh sequential cursor",
            ));
        }
        let budget = input.budget().clone();
        let mut source = TransportStream::new_with_namespace(
            budget.clone(),
            PacketFormat::Ts188,
            limits,
            program,
            namespace,
        )?;
        source.share_input();
        Ok(Self {
            source,
            observer: None,
            observations: BudgetVec::new(&budget),
            budget,
            token: input.token(),
            position: input.position(),
            trigger,
            program,
            limits,
            phase: Phase::Read,
            fault: None,
            ordinal: 0,
            diagnostic: None,
            fault_count: 0,
            end: None,
            finished: None,
            bytes: 0,
            zeros: 0,
        })
    }

    /// Advance the issuing source and its recovery observer. `Ready` publishes
    /// the retained run once. Subsequent calls return its confirmed `End`.
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RecoveredOpaque>> {
        check_cancel(cancel)?;
        self.check_input(input)?;
        if let Some(error) = self.fault {
            return Err(error);
        }
        let result = self.advance(input, work, cancel);
        self.position = input.position();
        if let Err(error) = &result
            && *error != Error::Cancelled
        {
            self.fault = Some(*error);
        }
        result
    }

    fn check_input(&self, input: &InputCursor) -> Result<()> {
        if !self.budget.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        if input.source() != self.trigger.location.source {
            return Err(Error::SourceMismatch);
        }
        let received = input.token();
        if received.generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: received.generation(),
            });
        }
        if received.request() != self.token.request() {
            return Err(Error::StaleRequest {
                expected: self.token.request(),
                received: received.request(),
            });
        }
        if input.position() != self.position {
            return Err(Error::Invalid(
                "opaque recovery cursor advanced outside poll",
            ));
        }
        Ok(())
    }

    fn advance(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RecoveredOpaque>> {
        loop {
            check_cancel(cancel)?;
            match &mut self.phase {
                Phase::Read => match self.source.poll(input, work, cancel)? {
                    Step::Ready(event) => {
                        let context = SourceContext {
                            source_event_ordinal: self.ordinal,
                            epochs: self.source.source_epochs(),
                            byte_run_epoch: None,
                        };
                        self.ordinal = self.ordinal.checked_add(1).ok_or(Error::Overflow)?;
                        self.phase = Phase::Route(event, context);
                    }
                    Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                    Step::Yield => return Ok(Step::Yield),
                    Step::End(_) => self.phase = Phase::SealEnd,
                },
                Phase::Route(..) => {
                    // One fixed-size carrier or one source fact.
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let Phase::Route(event, context) =
                        std::mem::replace(&mut self.phase, Phase::Read)
                    else {
                        unreachable!()
                    };
                    self.phase = self.route(event, context, input)?;
                }
                Phase::Drain => {
                    let Some(observer) = self.observer.as_mut() else {
                        self.phase = Phase::Read;
                        continue;
                    };
                    match observer.poll(work, cancel)? {
                        AssemblyStep::Output(packet) => {
                            self.check_observation(&packet)?;
                            let len = self.observations.len();
                            // Pay for retained records that move if this append grows storage.
                            let copy_bytes = if len == self.observations.capacity() {
                                len.checked_mul(size_of::<TransportObservation>())
                                    .ok_or(Error::Overflow)?
                            } else {
                                0
                            };
                            let retain_work =
                                u64::try_from(copy_bytes.checked_add(1).ok_or(Error::Overflow)?)
                                    .map_err(|_| Error::Overflow)?;
                            self.phase = Phase::Scan(Scan {
                                packet,
                                span: 0,
                                offset: 0,
                                retain_work,
                            });
                        }
                        AssemblyStep::Yield => return Ok(Step::Yield),
                        AssemblyStep::NeedPayload if self.finished.is_none() => {
                            self.phase = Phase::Read;
                        }
                        AssemblyStep::Finished if self.finished.is_some() => {
                            self.phase = Phase::Publish;
                        }
                        _ => return Err(Error::Invalid("opaque observer and source end disagree")),
                    }
                }
                Phase::Scan(scan) => {
                    if !scan.poll(&mut self.zeros, &mut self.bytes, work, cancel)? {
                        return Ok(Step::Yield);
                    }
                    scan.retain_work -= work.take(scan.retain_work);
                    if scan.retain_work != 0 {
                        return Ok(Step::Yield);
                    }
                    check_cancel(cancel)?;
                    let Phase::Scan(scan) = std::mem::replace(&mut self.phase, Phase::Drain) else {
                        unreachable!()
                    };
                    self.observations
                        .try_push(scan.packet)
                        .map_err(|failure| failure.error)?;
                }
                Phase::SealEnd => {
                    let Some(end) = self.source.media_entity_end(input, work, cancel)? else {
                        return Ok(Step::Yield);
                    };
                    self.end = Some(end);
                    self.phase = Phase::SealTail;
                }
                Phase::SealTail => {
                    if let Some(observer) = self.observer.as_mut()
                        && observer.awaiting_terminal_completion()
                        && !observer.seal_terminal_end(
                            self.end
                                .as_ref()
                                .ok_or(Error::Invalid("opaque source End missing"))?,
                            input,
                            work,
                            cancel,
                        )?
                    {
                        return Ok(Step::Yield);
                    }
                    self.source.end_of_source()?;
                    self.phase = Phase::Read;
                }
                Phase::Finish(context) => {
                    let observer = self.observer.as_mut().ok_or(Error::Unsupported(
                        "opaque recovery requires a selected AVC stream",
                    ))?;
                    if observer.awaiting_terminal_completion() {
                        let finished = self.source.source_finished(input)?;
                        if !observer
                            .confirm_terminal_finished(&finished, *context, input, work, cancel)?
                        {
                            return Ok(Step::Yield);
                        }
                    }
                    if self.trigger.proof == Proof::FinalCarrier
                        && !observer.terminal_confirmation().is_some_and(|proof| {
                            RecoveryTrigger::from_fault(proof.source_fault()) == Some(self.trigger)
                        })
                    {
                        return Err(Error::Unsupported("opaque final carrier is unconfirmed"));
                    }
                    observer.end_of_source()?;
                    self.finished = Some(*context);
                    self.phase = Phase::Drain;
                }
                Phase::Publish => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    return self.publish(input, cancel).map(Step::Ready);
                }
                Phase::Done(end) => {
                    input.read_into(&mut [], &mut WorkBudget::new(0), cancel)?;
                    return Ok(Step::End(*end));
                }
            }
        }
    }

    fn route(
        &mut self,
        event: TraceEvent,
        context: SourceContext,
        input: &InputCursor,
    ) -> Result<Phase> {
        if self.source.clock_is_quarantined()
            || self.observer.as_ref().is_some_and(|observer| {
                observer.scope().epochs != context.epochs
                    || (observer.awaiting_terminal_completion()
                        && !matches!(event, TraceEvent::Finished))
            })
        {
            return Err(Error::Unsupported("opaque recovery source boundary"));
        }
        match event {
            TraceEvent::ProgramMap {
                map, active: true, ..
            } => {
                if self.observer.is_some() {
                    return Err(Error::Unsupported(
                        "opaque recovery requires one source map",
                    ));
                }
                self.observer = Some(self.bind_observer(&map, input, context)?);
            }
            TraceEvent::Transport(packet) => {
                if let Some(observer) = self.observer.as_mut()
                    && packet.pid == observer.scope().pid
                {
                    observer
                        .push(packet, context)
                        .map_err(|failure| failure.error)?;
                }
            }
            TraceEvent::Diagnostic(fault) => self.route_fault(fault, context, input)?,
            TraceEvent::Finished => {
                if self.end.is_none() || self.diagnostic.is_none() {
                    return Err(Error::Unsupported("opaque source Finished without proof"));
                }
                return Ok(Phase::Finish(context));
            }
            TraceEvent::Timeline { .. } | TraceEvent::Discontinuity { .. } => {
                return Err(Error::Unsupported("opaque recovery source boundary"));
            }
            TraceEvent::Continuity { .. } | TraceEvent::Pes(_) => return Ok(Phase::Read),
            _ => {}
        }
        Ok(Phase::Drain)
    }

    fn bind_observer(
        &self,
        map: &ProgramMap,
        input: &InputCursor,
        context: SourceContext,
    ) -> Result<TransportFaultRecovery> {
        let [stream] = map.streams.as_slice() else {
            return Err(Error::Unsupported(
                "opaque recovery requires one AVC stream",
            ));
        };
        if map.header.extension != self.program || stream.stream_type != 0x1b {
            return Err(Error::Unsupported("opaque recovery requires selected AVC"));
        }
        let scope = DemuxScope {
            program: self.program,
            pid: stream.pid,
            stream_type: stream.stream_type,
            stream_id: 0xe0,
            entity: input.source(),
            epochs: context.epochs,
        };
        let observer = DemuxObserver::new_registered(
            DemuxProfile::PesPackets,
            scope,
            RegisteredCarriage::Avc,
            self.budget.clone(),
            self.limits,
        )?;
        let mut observer = TransportFaultRecovery::with_policy(
            observer,
            PesFaultPolicy {
                timing_headers: true,
                rejected_clock_fields: true,
                source_pcr_pid: Some(map.pcr_pid),
                terminal_transport: true,
            },
        )
        .map_err(|failure| failure.error)?;
        observer.bind_source_input(input)?;
        Ok(observer)
    }

    fn route_fault(
        &mut self,
        fault: TraceFault,
        context: SourceContext,
        input: &InputCursor,
    ) -> Result<()> {
        let Some(observed) = RecoveryTrigger::from_fault(&fault) else {
            return Err(Error::Unsupported("opaque recovery source diagnostic"));
        };
        let matches = if self.diagnostic.is_none() {
            observed == self.trigger
        } else {
            self.trigger.proof == Proof::PacketFault
                && observed.kind == self.trigger.kind
                && observed.error == self.trigger.error
        };
        if !matches {
            return Err(Error::Unsupported("opaque recovery diagnostic mismatch"));
        }
        let observer = self.observer.as_mut().ok_or(Error::Unsupported(
            "opaque recovery requires a selected AVC stream",
        ))?;
        if observer.awaiting_source_fault() {
            observer.confirm_source_fault(&fault, context)?;
        } else if fault.rejected.is_some()
            && matches!(
                fault.error,
                Error::Invalid("PCR reserved bits/extension" | "OPCR without PCR")
            )
        {
            observer
                .push_rejected_fault(fault.clone(), context)
                .map_err(|failure| failure.error)?;
        } else if observed.proof == Proof::FinalCarrier {
            let observed = ObservedTrace {
                event: TraceEvent::Diagnostic(fault.clone()),
                origin: self.source.trace_origin(input)?,
            };
            observer.hold_terminal_transport(&observed, context)?;
        } else if observed.proof == Proof::PacketFault {
            return Err(Error::Unsupported(
                "opaque recovery has no matching packet fault",
            ));
        }
        if self.diagnostic.is_none() {
            self.diagnostic = Some((fault, context));
        }
        self.fault_count = self.fault_count.checked_add(1).ok_or(Error::Overflow)?;
        Ok(())
    }

    fn check_observation(&self, packet: &TransportObservation) -> Result<()> {
        let fault = match packet {
            TransportObservation::Faulted(packet) => Some(&packet.source_fault),
            TransportObservation::PesFault(packet) => Some(&packet.source_fault),
            TransportObservation::Accepted(_) => None,
        };
        let admitted = match (self.trigger.proof, fault) {
            (Proof::PacketFault, Some(fault)) if self.observations.is_empty() => {
                RecoveryTrigger::from_fault(fault) == Some(self.trigger)
            }
            (Proof::PacketFault, Some(fault)) => {
                fault.kind == self.trigger.kind && fault.error == self.trigger.error
            }
            (Proof::PacketFault, None) | (_, Some(_)) => false,
            (_, None) => true,
        };
        if admitted {
            Ok(())
        } else {
            Err(Error::Unsupported(
                "opaque recovery observation does not match the fault",
            ))
        }
    }

    fn proves_shape(&self) -> bool {
        let packets = self.observations.as_slice();
        if packets.is_empty() || self.bytes == 0 {
            return false;
        }
        match self.trigger.proof {
            Proof::PacketFault => packets.len() == self.fault_count,
            Proof::FinalCarrier => true,
            Proof::FiniteEof => matches!(
                packets,
                [TransportObservation::Accepted(DemuxPacket {
                    boundary: DemuxBoundary::LogicalEof,
                    payload_kind: DemuxPayloadKind::DeclaredIncomplete,
                    ..
                })]
            ),
            Proof::FiniteOverrun => matches!(
                packets,
                [TransportObservation::Accepted(DemuxPacket {
                    boundary: DemuxBoundary::FiniteLength,
                    payload_kind: DemuxPayloadKind::DeclaredComplete,
                    ..
                })] | [
                    TransportObservation::Accepted(DemuxPacket {
                        boundary: DemuxBoundary::FiniteOverrun,
                        payload_kind: DemuxPayloadKind::DeclaredIncomplete,
                        ..
                    }),
                    TransportObservation::Accepted(DemuxPacket {
                        payload_kind: DemuxPayloadKind::HeaderlessContinuation,
                        ..
                    })
                ]
            ),
        }
    }

    fn publish(&mut self, input: &mut InputCursor, cancel: &AtomicBool) -> Result<RecoveredOpaque> {
        if !self.proves_shape() {
            return Err(Error::Unsupported(
                "opaque recovery has unproved byte boundaries",
            ));
        }
        let end = self
            .end
            .take()
            .ok_or(Error::Invalid("opaque source End missing"))?;
        match input.read_discard(1, &mut WorkBudget::new(0), cancel) {
            Ok(Step::End(actual)) if actual == end.location() => {}
            Err(Error::Cancelled) => {
                self.end = Some(end);
                return Err(Error::Cancelled);
            }
            Err(error) => return Err(error),
            _ => return Err(Error::Invalid("opaque recovery requires actual core EOF")),
        }
        let (fault, diagnostic) = self
            .diagnostic
            .take()
            .ok_or(Error::Invalid("opaque recovery diagnostic missing"))?;
        let finished = self
            .finished
            .ok_or(Error::Invalid("opaque source Finished missing"))?;
        self.phase = Phase::Done(end.location());
        Ok(RecoveredOpaque {
            fault,
            diagnostic,
            end,
            finished,
            observations: std::mem::replace(&mut self.observations, BudgetVec::new(&self.budget)),
            bytes: self.bytes,
        })
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{OpaqueRecovery, RecoveryTrigger};
    use crate::{FaultKind, ParseLimits, TraceFault, psi::DescriptorNamespace};
    use std::sync::atomic::AtomicBool;
    use triergon_core::{
        Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceFailure,
        SourceId, SourceLocation, Step, WorkBudget,
    };

    #[test]
    fn sticky_io_failure_after_the_sealed_end_prevents_publication() {
        let pool = Budget::new(1 << 20).unwrap();
        let raw = Bytes::copy_from_slice(
            &pool,
            include_bytes!("../../tests/data/transport-sync-loss.ts"),
        )
        .unwrap();
        let source = SourceId(1);
        let mut input = InputCursor::new(
            &pool,
            source,
            SourceCapabilities {
                length: Some(raw.len() as u64),
                random_access: false,
            },
            4096,
        )
        .unwrap();
        let trigger = RecoveryTrigger::from_fault(&TraceFault {
            kind: FaultKind::Transport,
            pid: None,
            location: SourceLocation {
                source,
                offset: 564,
            },
            error: Error::Invalid("TS sync byte"),
            rejected: None,
        })
        .unwrap();
        let mut proof = OpaqueRecovery::new(
            &input,
            ParseLimits::default(),
            1,
            DescriptorNamespace::Unspecified,
            trigger,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        for _ in 0..20_000 {
            match proof
                .poll(&mut input, &mut WorkBudget::new(1), &cancel)
                .unwrap()
            {
                Step::NeedInput(next) if next.offset == 0 => input
                    .provide_owned(InputWindow::new(source, next.token, 0, raw.clone()).unwrap())
                    .unwrap(),
                Step::NeedInput(next) => {
                    assert_eq!(next.offset, raw.len() as u64);
                    input.finish(source, next.token).unwrap();
                }
                Step::Yield if proof.finished.is_some() => {
                    assert!(proof.end.is_some());
                    input
                        .fail(source, input.token(), SourceFailure::Transport)
                        .unwrap();
                    let error = proof
                        .poll(&mut input, &mut WorkBudget::new(1), &cancel)
                        .unwrap_err();
                    assert_eq!(error, Error::Transport);
                    assert_eq!(
                        proof
                            .poll(&mut input, &mut WorkBudget::new(1), &cancel)
                            .unwrap_err(),
                        error
                    );
                    return;
                }
                Step::Yield => {}
                other => panic!("expected a pending publication, got {other:?}"),
            }
        }
        panic!("proof did not reach publication");
    }
}
