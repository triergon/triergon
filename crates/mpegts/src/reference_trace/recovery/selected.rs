//! Explicit finite AVC diagnostic view: the selected carriers are staged as the fault jobs of
//! [`TransportFaultRecovery`](crate::compat::pes::TransportFaultRecovery), under this
//! observer's own admission.
use super::*;
use crate::compat::pes::fault::video_timing_hint;
use crate::stream::{FaultKind, TraceFault};
use crate::transport::PCR_ENCODING;

/// Constant bounded hint only; no source header is validated or replaced here.
/// The staged carrier cannot emit until its actual strict diagnostic arrives.
pub(super) fn has_selected_hint(packet: &TransportPacket) -> bool {
    packet.transport_error || packet.scrambling != 0 || video_timing_hint(packet)
}

impl RecoveryObserver {
    /// Observe finite AVC packets under the selected diagnostic policies. A scope
    /// other than stream type `0x1b` and PES stream ID `0xe0` returns `Unsupported`.
    /// Other constructor failures follow [`Self::new`].
    pub fn new_selected_avc_faults(
        scope: DemuxScope,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        if scope.stream_type != stream_type::H264 || scope.stream_id != stream_id::VIDEO_FIRST {
            return Err(Error::Unsupported(
                "selected diagnostics require explicit AVC carriage",
            ));
        }
        let mut observer = Self::new(
            scope,
            RegisteredCarriage::Avc,
            RejectedPolicy::OpcrWithoutPcr,
            budget,
            limits,
        )?;
        observer.selected_faults = true;
        Ok(observer)
    }
    fn selected_context(
        &self,
        source: Option<triergon_core::SourceId>,
        offset: u64,
        c: SourceContext,
        bytes: &Bytes,
    ) -> Result<()> {
        let s = self.scope();
        if !self.selected_faults {
            return Err(Error::Unsupported("selected diagnostics not enabled"));
        }
        if self.pending.is_some() || !self.inner.recovery_idle() || self.fault.is_some() {
            return Err(Error::Invalid("selected diagnostic input not drained"));
        }
        if !bytes.belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        if source != Some(s.entity) || c.epochs != s.epochs || c.byte_run_epoch.is_some() {
            return Err(Error::Unsupported(
                "selected diagnostic source context unobserved",
            ));
        }
        if !self.after_pmt_binding(c)
            || self.last.is_some_and(|(event, old)| {
                c.source_event_ordinal <= event || (!self.entity_first && offset <= old)
            })
        {
            return Err(Error::Invalid("selected diagnostic source order"));
        }
        offset.checked_add(188).ok_or(Error::Overflow)?;
        if bytes.len() != 188 {
            return Err(Error::Unsupported("selected diagnostic TS188 only"));
        }
        Ok(())
    }
    #[expect(
        clippy::result_large_err,
        reason = "original full charged carrier returned inline"
    )]
    pub(super) fn stage_selected_input(
        &mut self,
        packet: TransportPacket,
        c: SourceContext,
        input: Option<&triergon_core::InputCursor>,
    ) -> std::result::Result<(), Refused<TransportPacket>> {
        let admission = (|| {
            self.selected_context(packet.source_id, packet.source_offset, c, &packet.source)?;
            DemuxObserver::transport_bounds(&packet)?;
            if packet.pid != self.scope().pid
                || packet.prefix.is_some()
                || !packet.payload_unit_start
            {
                return Err(Error::Unsupported(
                    "selected diagnostic envelope unobserved",
                ));
            }
            Ok(())
        })();
        if let Err(error) = admission {
            return Err(Refused {
                error,
                value: packet,
            });
        }
        let offset = packet.source_offset;
        let job = if packet.transport_error || packet.scrambling != 0 {
            TransportFaultJob::new(packet, c, &self.inner, input)?
        } else {
            TransportFaultJob::staged_header(packet, c, &self.inner, input)?
        };
        self.last = Some((c.source_event_ordinal, offset));
        self.entity_first = false;
        self.pending = Some(RecoveryPending::TransportFault(job));
        Ok(())
    }
    /// Stage the source diagnostic and its complete original carrier after
    /// checking that the PCR encoding is its sole transport failure. The source
    /// scope and ordinal must match, and pending input must be drained. A refusal
    /// returns the diagnostic and charged carrier intact.
    #[expect(
        clippy::result_large_err,
        reason = "recover the original charged source diagnostic inline"
    )]
    pub fn push_diagnostic_rejection(
        &mut self,
        fault: TraceFault,
        c: SourceContext,
    ) -> std::result::Result<(), Refused<TraceFault>> {
        if let Err(error) = self.inner.validate_input(None) {
            return Err(Refused {
                error,
                value: fault,
            });
        }
        let admission = (|| {
            let r = fault
                .rejected
                .as_ref()
                .ok_or(Error::Invalid("selected rejection owner missing"))?;
            self.selected_context(r.source_id(), r.source_offset(), c, r.bytes())?;
            if fault.kind != FaultKind::Transport
                || fault.pid.is_some()
                || fault.error != Error::Invalid(PCR_ENCODING)
                || r.error() != fault.error
                || fault.location.source != self.scope().entity
                || fault.location.offset != r.source_offset()
                || r.secondary_error().is_some()
                || !r.fully_checked()
                || r.format() != PacketFormat::Ts188
            {
                return Err(Error::Unsupported(
                    "selected PCR rejection is not the sole checked source fault",
                ));
            }
            let f = r.envelope();
            if f.pid != self.scope().pid
                || !f.pusi
                || f.tei
                || f.scrambling != 0
                || f.afc != 3
                || f.flags != 16
            {
                return Err(Error::Unsupported(
                    "selected invalid PCR envelope unobserved",
                ));
            }
            Ok(r.source_offset())
        })();
        let offset = match admission {
            Ok(offset) => offset,
            Err(error) => {
                return Err(Refused {
                    error,
                    value: fault,
                });
            }
        };
        let job = TransportFaultJob::staged_rejected(fault, c, &self.inner)?;
        self.last = Some((c.source_event_ordinal, offset));
        self.entity_first = false;
        self.pending = Some(RecoveryPending::TransportFault(job));
        Ok(())
    }
    /// A wrong/stale diagnostic leaves the one original pending storage intact.
    pub fn confirm_source_fault(&mut self, fault: &TraceFault, c: SourceContext) -> Result<()> {
        let Some(RecoveryPending::TransportFault(job)) = self.pending.as_mut() else {
            return Err(Error::Invalid(
                "no selected source fault awaiting diagnostic",
            ));
        };
        job.confirm(fault, c)?;
        self.last = Some((c.source_event_ordinal, fault.location.offset));
        self.entity_first = false;
        Ok(())
    }
    /// Advance the source clock epoch after draining a validated PCR discontinuity.
    /// Missing or out-of-order timeline context and undrained input are refused.
    /// The compatibility clock interval remains in effect.
    pub fn commit_source_timeline(&mut self, c: SourceContext) -> Result<()> {
        let Some((ordinal, old)) = self.pending_timeline else {
            return Err(Error::Unsupported(
                "no selected PCR boundary awaiting Timeline",
            ));
        };
        if !self.selected_faults
            || self.pending.is_some()
            || !self.inner.recovery_idle()
            || c.source_event_ordinal <= ordinal
            || c.epochs.configuration != old.configuration
            || c.epochs.clock != old.clock.checked_add(1).ok_or(Error::Overflow)?
            || c.byte_run_epoch.is_some()
        {
            return Err(Error::Unsupported(
                "selected source Timeline context unobserved",
            ));
        }
        self.inner.commit_clock_epoch(c);
        if let Some((ordinal, _)) = self.last.as_mut() {
            *ordinal = (*ordinal).max(c.source_event_ordinal);
        }
        self.pending_timeline = None;
        Ok(())
    }
}
