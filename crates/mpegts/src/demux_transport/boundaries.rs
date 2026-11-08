//! Explicit source boundaries beside finite compatibility packets.
// H.222.0 (08/2023) 2.4.3.5: DI signals a source clock discontinuity;
// the strict source Timeline, rather than compatibility timestamps, binds it.
use super::{SourceContext, TransportFaultRecovery};
use crate::demux_pes::{DemuxBoundary, DemuxPacket};
use crate::{FaultKind, MediaEntityEnd, PacketFormat, TraceEvent, TraceFault, TransportPacket};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Error, InputCursor, InputToken, PushError, Result, SourceLocation, Step, WorkBudget,
};

/// A final unusable carrier was excluded after nonempty finite packet output.
/// The unchanged strict diagnostic and real physical/source EOF remain available.
#[derive(Debug)]
pub struct TransportTailConfirmation {
    fault: TraceFault,
    diagnostic: SourceContext,
    end: SourceLocation,
    finished: SourceContext,
}
impl TransportTailConfirmation {
    pub fn source_fault(&self) -> &TraceFault {
        &self.fault
    }
    pub fn diagnostic_context(&self) -> SourceContext {
        self.diagnostic
    }
    pub fn physical_end(&self) -> SourceLocation {
        self.end
    }
    pub fn source_finished(&self) -> SourceContext {
        self.finished
    }
}

#[derive(Debug)]
struct TailFault {
    fault: TraceFault,
    diagnostic: SourceContext,
    end: Option<(SourceLocation, InputToken)>,
}
#[derive(Debug)]
enum Tail {
    Pending(TailFault),
    Confirmed(TransportTailConfirmation),
}
#[derive(Debug, Default)]
pub(super) struct Boundaries {
    source_input: Option<InputToken>,
    last_carrier: Option<(u64, SourceContext, bool)>,
    finite_output: bool,
    timeline: Option<SourceContext>,
    tail: Option<Tail>,
}
impl Boundaries {
    pub(super) fn input_error(&self) -> Option<Error> {
        (self.timeline.is_some() || self.tail.is_some())
            .then_some(Error::Unsupported("source boundary confirmation required"))
    }
    pub(super) fn check_end(&self) -> Result<()> {
        if self.timeline.is_some() || matches!(self.tail, Some(Tail::Pending(_))) {
            return Err(Error::Unsupported("source boundary confirmation required"));
        }
        Ok(())
    }
    pub(super) fn note_packet(&mut self, packet: &DemuxPacket) {
        self.finite_output = packet.boundary == DemuxBoundary::FiniteLength
            && packet.payload_len != 0
            && packet.flags == 0;
    }
}
impl TransportFaultRecovery {
    /// Bind the original sequential core input before admitting any carrier.
    /// Terminal recovery requires this identity; reset, seek, entity continuation
    /// and a replacement cursor cannot supply this operation's EOF proof.
    /// Binding retains a fixed-size token, with no byte or `Budget` handle clone.
    pub fn bind_source_input(&mut self, input: &InputCursor) -> Result<()> {
        if let Some(error) = self.fault {
            return Err(error);
        }
        if !self.inner.recovery_budget().same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        if input.source() != self.scope().entity {
            return Err(Error::SourceMismatch);
        }
        if self.boundaries.source_input.is_some()
            || self.pending.is_some()
            || self.boundaries.input_error().is_some()
            || self.boundaries.last_carrier.is_some()
            || !self.inner.recovery_pristine()
        {
            return Err(Error::Invalid(
                "source input binding must precede admitted carriers",
            ));
        }
        self.boundaries.source_input = Some(input.token());
        Ok(())
    }

    // Both callers check the pool first, preserving foreign-pool precedence.
    pub(super) fn check_source_input(&self, input: &InputCursor) -> Result<()> {
        let expected = self
            .boundaries
            .source_input
            .as_ref()
            .ok_or(Error::Unsupported(
                "terminal transport source input not bound",
            ))?;
        let received = input.token();
        if expected.generation() != received.generation() {
            return Err(Error::StaleGeneration {
                expected: expected.generation(),
                received: received.generation(),
            });
        }
        if expected.request() != received.request() {
            return Err(Error::StaleRequest {
                expected: expected.request(),
                received: received.request(),
            });
        }
        if input.source() != self.scope().entity {
            return Err(Error::SourceMismatch);
        }
        Ok(())
    }

    #[allow(
        clippy::result_large_err,
        reason = "return the original charged carrier inline"
    )]
    pub(super) fn push_ordinary(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        let boundary = self.policy.source_pcr_pid == Some(packet.pid)
            && packet.discontinuity_indicator()
            && packet.adaptation.as_ref().is_some_and(|a| a.pcr.is_some());
        if boundary {
            let error = self.inner.validate_input(None).err().or_else(|| {
                let scope = self.scope();
                (!self.inner.recovery_idle()
                    || packet.prefix.is_some()
                    || packet.source.len() != 188
                    || !packet.payload_unit_start
                    || context.byte_run_epoch.is_some()
                    || scope.pid != packet.pid
                    || scope.stream_type != 0x1b
                    || scope.stream_id != 0xe0)
                    .then_some(Error::Unsupported(
                        "source PCR boundary requires drained AVC TS188 PUSI",
                    ))
            });
            if let Some(error) = error {
                return Err(PushError {
                    error,
                    value: packet,
                });
            }
        }
        let end = packet.source_offset.checked_add(packet.source.len() as u64);
        let ts188 = packet.prefix.is_none() && packet.source.len() == 188;
        if boundary {
            self.inner.push_clock_boundary(packet, context)?;
            self.boundaries.timeline = Some(context);
        } else {
            self.inner.push(packet, context)?;
        }
        // The ordinary assembler checked the full-width physical end already.
        self.boundaries.last_carrier = end.map(|end| (end, context, ts188));
        self.boundaries.finite_output = false;
        Ok(())
    }

    /// Confirm only the actual next source Timeline after an admitted PCR/DI
    /// carrier has produced a complete finite packet. The caller keeps its
    /// existing DemuxClock; this changes only the source epoch of future packets.
    pub fn confirm_source_timeline(
        &mut self,
        event: &TraceEvent,
        context: SourceContext,
    ) -> Result<()> {
        if let Some(error) = self.fault {
            return Err(error);
        }
        let old = self.boundaries.timeline.ok_or(Error::Unsupported(
            "no source PCR boundary awaiting Timeline",
        ))?;
        let next = old.epochs.clock.checked_add(1).ok_or(Error::Overflow)?;
        if !matches!(event, TraceEvent::Timeline { program, clock_epoch }
            if *program == self.scope().program && *clock_epoch == next)
            || self.pending.is_some()
            || self.boundaries.tail.is_some()
            || !self.inner.recovery_idle()
            || !self.boundaries.finite_output
            || context.source_event_ordinal <= old.source_event_ordinal
            || context.epochs.clock != next
            || context.epochs.configuration != old.epochs.configuration
            || context.byte_run_epoch.is_some()
        {
            return Err(Error::Unsupported("source Timeline context unobserved"));
        }
        self.inner.commit_clock_epoch(context);
        self.boundaries.timeline = None;
        Ok(())
    }

    /// Hold a possible final partial carrier or sync-invalid carrier. No EOF
    /// or success is implied. Further transport is refused until physical EOF
    /// and source Finished confirm this was the sole final unusable carrier.
    pub fn hold_terminal_transport(
        &mut self,
        fault: &TraceFault,
        context: SourceContext,
    ) -> Result<()> {
        if let Some(error) = self.fault {
            return Err(error);
        }
        if self.boundaries.source_input.is_none() {
            return Err(Error::Unsupported(
                "terminal transport source input not bound",
            ));
        }
        let scope = self.scope();
        let last = self.boundaries.last_carrier;
        let carrier = match fault.error {
            Error::Truncated => fault.rejected.is_none(),
            Error::Invalid("TS sync byte") => fault.rejected.as_ref().is_some_and(|rejected| {
                rejected.format() == PacketFormat::Ts188
                    && rejected.source_id() == Some(scope.entity)
                    && rejected.source_offset() == fault.location.offset
                    && rejected.error() == fault.error
                    && rejected.secondary_error().is_none()
                    && rejected.bytes().len() == 188
                    && rejected.bytes().as_ref()[0] != 0x47
                    && rejected.bytes().belongs_to(self.inner.recovery_budget())
            }),
            _ => false,
        };
        if !self.policy.terminal_transport
            || self.pending.is_some()
            || self.boundaries.input_error().is_some()
            || !self.inner.recovery_idle()
            || !self.boundaries.finite_output
            || scope.stream_type != 0x1b
            || scope.stream_id != 0xe0
            || fault.kind != FaultKind::Transport
            || fault.pid.is_some()
            || !carrier
            || fault.location.source != scope.entity
            || context.epochs != scope.epochs
            || context.byte_run_epoch.is_some()
            || !last.is_some_and(|(end, previous, ts188)| {
                ts188
                    && end == fault.location.offset
                    && previous.epochs == context.epochs
                    && previous.source_event_ordinal < context.source_event_ordinal
            })
        {
            return Err(Error::Unsupported("terminal transport context unobserved"));
        }
        self.boundaries.tail = Some(Tail::Pending(TailFault {
            fault: fault.clone(),
            diagnostic: context,
            end: None,
        }));
        Ok(())
    }
    /// A held fault still needs physical EOF or source Finished confirmation.
    pub fn awaiting_terminal_completion(&self) -> bool {
        matches!(self.boundaries.tail, Some(Tail::Pending(_)))
    }
    pub fn terminal_confirmation(&self) -> Option<&TransportTailConfirmation> {
        match self.boundaries.tail.as_ref() {
            Some(Tail::Confirmed(confirmation)) => Some(confirmation),
            _ => None,
        }
    }
    /// Seal a real drained TS/core End. A short grant leaves the pending fault
    /// unchanged. No input-dependent allocation or payload copy is performed.
    pub fn seal_terminal_end(
        &mut self,
        end: &MediaEntityEnd,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(error) = self.fault {
            return Err(error);
        }
        let scope = self.scope();
        if !self.inner.recovery_budget().same_pool(input.budget())
            || !end.budget.same_pool(input.budget())
        {
            return Err(Error::ForeignPool);
        }
        self.check_source_input(input)?;
        let Some(Tail::Pending(tail)) = self.boundaries.tail.as_mut() else {
            return Err(Error::Invalid("no terminal transport awaiting End"));
        };
        let tail_bytes = end.location.offset.checked_sub(tail.fault.location.offset);
        let extent = match tail.fault.error {
            Error::Truncated => tail_bytes.is_some_and(|bytes| (1..188).contains(&bytes)),
            Error::Invalid("TS sync byte") => tail_bytes == Some(188),
            _ => false,
        };
        if tail.end.is_some()
            || end.program != scope.program
            || end.format != PacketFormat::Ts188
            || end.epochs != scope.epochs
            || end.location.source != scope.entity
            || input.source() != scope.entity
            || end.location.offset != input.position()
            || end.token != input.token()
            || !extent
        {
            return Err(Error::Unsupported(
                "terminal transport physical End mismatch",
            ));
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        match input.read_into(&mut [0_u8; 1], &mut WorkBudget::new(1), cancel)? {
            Step::End(actual) if actual == end.location => {}
            _ => {
                return Err(Error::Invalid(
                    "terminal transport requires actual core EOF",
                ));
            }
        }
        tail.end = Some((end.location, end.token.clone()));
        Ok(true)
    }

    /// Source Finished and a fresh read of the same core EOF complete the
    /// confirmation. An advertised length, starvation or an I/O error cannot.
    pub fn confirm_terminal_finished(
        &mut self,
        event: &TraceEvent,
        context: SourceContext,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(error) = self.fault {
            return Err(error);
        }
        if !self.inner.recovery_budget().same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        self.check_source_input(input)?;
        let Some(Tail::Pending(tail)) = self.boundaries.tail.as_ref() else {
            return Err(Error::Invalid("no terminal transport awaiting Finished"));
        };
        let (location, token) = tail
            .end
            .as_ref()
            .ok_or(Error::Invalid("terminal transport requires physical End"))?;
        if !matches!(event, TraceEvent::Finished)
            || context.epochs != tail.diagnostic.epochs
            || context.byte_run_epoch.is_some()
            || context.source_event_ordinal <= tail.diagnostic.source_event_ordinal
            || location.source != input.source()
            || location.offset != input.position()
            || *token != input.token()
        {
            return Err(Error::Invalid(
                "terminal transport source Finished mismatch",
            ));
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        match input.read_into(&mut [0_u8; 1], &mut WorkBudget::new(1), cancel)? {
            Step::End(actual) if actual == *location => {}
            _ => {
                return Err(Error::Invalid(
                    "terminal transport requires actual core EOF",
                ));
            }
        }
        let end = *location;
        let Some(Tail::Pending(tail)) = self.boundaries.tail.take() else {
            return Err(Error::Invalid("terminal transport state missing"));
        };
        self.boundaries.tail = Some(Tail::Confirmed(TransportTailConfirmation {
            fault: tail.fault,
            diagnostic: tail.diagnostic,
            end,
            finished: context,
        }));
        Ok(true)
    }
}
