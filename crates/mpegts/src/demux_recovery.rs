//! Opt-in real rejected-carrier observation. No validated source packet/header
//! is forged. OPCR and explicit selected-AVC diagnostic policies are separate.
use crate::demux_clock::DemuxProfile;
use crate::demux_pes::{
    DemuxObserver, DemuxPacket, DemuxScope, DemuxSpan, PrivateAudioCarriage,
    PrivateAudioDescriptors, RegisteredCarriage, SourceContext,
};
use crate::demux_transport::TransportFaultJob;
use crate::{
    AssemblyStep, PacketFormat, ParseLimits, RejectedTransport, SourceEpochs, SourceRange,
    TransportPacket,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, PushError, Result, SourceLocation, WorkBudget,
};

mod lifecycle;
mod selected;
pub use lifecycle::{PatRoute, PatSelection, PmtBinding, PmtBindingJob};
pub use selected::{DiagnosticCarrier, DiagnosticHeader, DiagnosticPacket, SelectedFault};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectedPolicy {
    /// This carriage profile has no observed rejected-carrier operation.
    NoRejectedCarrier,
    OpcrWithoutPcr,
}

/// Independently checked encoding in a rejected carrier, NOT a source PesHeader.
#[derive(Debug)]
pub struct EncodedHeader {
    bytes: Bytes,
    pts: u64,
}
impl EncodedHeader {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }
    pub fn encoded_pts_raw33(&self) -> u64 {
        self.pts
    }
    pub fn encoded_dts_raw33(&self) -> Option<u64> {
        None
    }
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }
}

#[derive(Debug)]
pub struct RecoveredPacket {
    pub profile: DemuxProfile,
    pub program: u16,
    pub pid: u16,
    pub source_origin: SourceLocation,
    pub source_epochs: SourceEpochs,
    pub position: i64,
    pub flags: u32,
    pub duration90_raw: i64,
    pub stream_id: u8,
    pub rejection: RejectedTransport,
    pub context: SourceContext,
    pub encoded_header: EncodedHeader,
    pub header_source_ranges: BudgetVec<SourceRange>,
    pub payload_len: usize,
    pub spans: BudgetVec<DemuxSpan>,
}
impl RecoveredPacket {
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
    }
}
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one owned inline observation; dynamic storage is charged, no Box"
)]
pub enum DemuxObservation {
    Accepted(DemuxPacket),
    FromRejectedCarrier(RecoveredPacket),
    FromSelectedDiagnostic(DiagnosticPacket),
}

#[derive(Debug)]
struct Pending {
    rejection: RejectedTransport,
    context: SourceContext,
    phase: u8,
    pts: Option<u64>,
    header_ranges: BudgetVec<SourceRange>,
    spans: BudgetVec<DemuxSpan>,
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one bounded original charged input mailbox, no heap Box"
)]
enum RecoveryPending {
    Opcr(Pending),
    Selected(selected::SelectedPending),
    TransportFault(TransportFaultJob),
}

/// Accepted inputs reuse the established assembly. A rejected complete finite
/// single-carrier PES is assembled directly from that real boundary, never from
/// a completed output. Exactly one pending mailbox, shared pool and clock scope.
#[derive(Debug)]
pub struct RecoveryObserver {
    inner: DemuxObserver,
    pmt_binding: Option<PmtBinding>,
    budget: Budget,
    policy: RejectedPolicy,
    pending: Option<RecoveryPending>,
    selected_faults: bool,
    pending_timeline: Option<(u64, SourceEpochs)>,
    last: Option<(u64, u64)>,
    entity_first: bool,
    fault: Option<Error>,
}
impl RecoveryObserver {
    pub fn new(
        profile: DemuxProfile,
        scope: DemuxScope,
        carriage: RegisteredCarriage,
        policy: RejectedPolicy,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: DemuxObserver::new_registered(profile, scope, carriage, budget.clone(), limits)?,
            budget,
            policy,
            pmt_binding: None,
            pending: None,
            selected_faults: false,
            pending_timeline: None,
            last: None,
            entity_first: false,
            fault: None,
        })
    }
    /// Explicit finite private-audio operation; rejected source carriers stay
    /// outside this profile. Accepted carriers use the same charged assembly.
    pub fn new_private_audio(
        profile: DemuxProfile,
        scope: DemuxScope,
        carriage: PrivateAudioCarriage,
        descriptors: PrivateAudioDescriptors<'_>,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: DemuxObserver::new_private_audio(
                profile,
                scope,
                carriage,
                descriptors,
                budget.clone(),
                limits,
            )?,
            budget,
            policy: RejectedPolicy::NoRejectedCarrier,
            pmt_binding: None,
            pending: None,
            selected_faults: false,
            pending_timeline: None,
            last: None,
            entity_first: false,
            fault: None,
        })
    }
    /// Explicit DVB-signalled finite PES.
    pub fn new_dvb_subtitles(
        profile: DemuxProfile,
        scope: DemuxScope,
        namespace: crate::psi::DescriptorNamespace,
        descriptors: super::demux_pes::DvbSubtitlesDescriptors<'_>,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: DemuxObserver::new_dvb_subtitles(
                profile,
                scope,
                namespace,
                descriptors,
                budget.clone(),
                limits,
            )?,
            budget,
            policy: RejectedPolicy::NoRejectedCarrier,
            pmt_binding: None,
            pending: None,
            selected_faults: false,
            pending_timeline: None,
            last: None,
            entity_first: false,
            fault: None,
        })
    }

    pub fn bind_input(
        &mut self,
        context: &crate::demux_input::InputContext,
        input: &triergon_core::InputCursor,
    ) -> Result<()> {
        if self.pending.is_some() || self.last.is_some() {
            return Err(Error::Invalid(
                "native recovery startup binding after input",
            ));
        }
        self.inner.bind_input(context, input)
    }
    pub fn validate_input(&self, input: &triergon_core::InputCursor) -> Result<()> {
        self.inner.validate_input(Some(input))
    }
    /// No active assembly/output/fault: required before dropping an unusable
    /// carrier in the separate complete-object view. Does not alter continuity.
    pub fn discard_idle(&self) -> bool {
        self.pending.is_none()
            && self.pending_timeline.is_none()
            && self.fault.is_none()
            && self.inner.recovery_idle()
    }
    pub fn scope(&self) -> DemuxScope {
        self.inner.scope()
    }
    /// Explicit contiguous media transaction; plain pushes stay single-entity.
    pub fn continue_media_entity(
        &mut self,
        end: &crate::MediaEntityEnd,
        input: &mut triergon_core::InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if work.remaining() == 0 {
            return Ok(false);
        }
        if self.pending.is_some() || self.pending_timeline.is_some() || self.fault.is_some() {
            return Err(Error::Invalid(
                "native recovery media entity operation not drained",
            ));
        }
        if !self.inner.continue_media_entity(end, input, work, cancel)? {
            return Ok(false);
        }
        self.entity_first = true;
        Ok(true)
    }
    pub fn policy(&self) -> RejectedPolicy {
        self.policy
    }
    pub fn pending_rejection(&self) -> Option<&RejectedTransport> {
        self.pending.as_ref().and_then(|p| match p {
            RecoveryPending::Opcr(p) => Some(&p.rejection),
            RecoveryPending::Selected(p) => p.rejection(),
            RecoveryPending::TransportFault(_) => None,
        })
    }
    #[allow(
        clippy::result_large_err,
        reason = "recover original charged carrier without allocating a Box"
    )]
    pub fn push_accepted(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        self.push_accepted_bound(packet, context, None)
    }
    #[allow(
        clippy::result_large_err,
        reason = "recover original charged inline carrier"
    )]
    pub fn push_accepted_with_input(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
        input: &triergon_core::InputCursor,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        self.push_accepted_bound(packet, context, Some(input))
    }
    #[allow(
        clippy::result_large_err,
        reason = "recover original charged inline carrier"
    )]
    fn push_accepted_bound(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
        input: Option<&triergon_core::InputCursor>,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        if let Err(error) = self.inner.validate_input(input) {
            return Err(PushError {
                error,
                value: packet,
            });
        }
        if self.pending.is_some()
            || !self.after_pmt_binding(context)
            || self.fault.is_some()
            || self.last.is_some_and(|(event, offset)| {
                context.source_event_ordinal <= event
                    || (!self.entity_first && packet.source_offset <= offset)
            })
        {
            return Err(PushError {
                error: Error::Invalid("native recovery input not drained/ordered"),
                value: packet,
            });
        }
        if self.selected_faults && selected::has_selected_hint(&packet) {
            return self.stage_selected_input(packet, context, input);
        }
        let offset = packet.source_offset;
        let boundary = self.selected_faults
            && packet.discontinuity_indicator()
            && packet.adaptation.as_ref().is_some_and(|a| a.pcr.is_some());
        if boundary {
            if !self.inner.recovery_idle() || packet.prefix.is_some() || !packet.payload_unit_start
            {
                return Err(PushError {
                    error: Error::Unsupported(
                        "selected PCR boundary must be drained finite TS188 PUSI",
                    ),
                    value: packet,
                });
            }
            if let Some(input) = input {
                self.inner
                    .push_input_boundary(packet, context, true, input)?;
            } else {
                self.inner.push_clock_boundary(packet, context)?;
            }
            self.pending_timeline = Some((context.source_event_ordinal, context.epochs));
        } else if let Some(input) = input {
            self.inner
                .push_input_boundary(packet, context, false, input)?;
        } else {
            self.inner.push(packet, context)?;
        }
        self.last = Some((context.source_event_ordinal, offset));
        self.entity_first = false;
        Ok(())
    }
    #[allow(
        clippy::result_large_err,
        reason = "recover original charged rejection, error and full storage inline"
    )]
    pub fn push_rejected(
        &mut self,
        rejection: RejectedTransport,
        context: SourceContext,
    ) -> std::result::Result<(), PushError<RejectedTransport>> {
        // This finite-input position view does not admit new rejected-carrier
        // contexts. Unbound recovery is unaffected.
        if let Err(error) = self.inner.validate_input(None) {
            return Err(PushError {
                error,
                value: rejection,
            });
        }
        let scope = self.scope();
        let f = rejection.envelope();
        let error = if let Some(error) = self.fault {
            Some(error)
        } else if self.pending.is_some() || !self.inner.recovery_idle() {
            Some(Error::Invalid("native recovery input not drained"))
        } else if !rejection.bytes().belongs_to(&self.budget) {
            Some(Error::ForeignPool)
        } else if self.policy == RejectedPolicy::NoRejectedCarrier {
            Some(Error::Unsupported(if self.inner.is_dvb_subtitles() {
                "native DVB rejected carrier unobserved"
            } else {
                "native private audio rejected carrier unobserved"
            }))
        } else if rejection.source_id() != Some(scope.entity)
            || context.epochs != scope.epochs
            || context.byte_run_epoch.is_some()
        {
            Some(Error::Unsupported(
                "native rejected carrier context unobserved",
            ))
        } else if !self.after_pmt_binding(context)
            || self.last.is_some_and(|(event, offset)| {
                context.source_event_ordinal <= event || rejection.source_offset() <= offset
            })
        {
            Some(Error::Invalid("native recovery source event order"))
        } else if rejection.error() != Error::Invalid("OPCR without PCR")
            || rejection.secondary_error().is_some()
            || !rejection.fully_checked()
        {
            Some(Error::Unsupported(
                "native rejected carrier policy unobserved",
            ))
        } else if rejection.format() != PacketFormat::Ts188
            || scope.stream_type != 0x1b
            || scope.stream_id != 0xe0
            || f.pid != scope.pid
            || !f.pusi
            || f.tei
            || f.scrambling != 0
            || f.afc != 3
            || f.flags != 8
        {
            Some(Error::Unsupported(
                "native rejected envelope context unobserved",
            ))
        } else {
            None
        };
        if let Some(error) = error {
            return Err(PushError {
                error,
                value: rejection,
            });
        }
        self.last = Some((context.source_event_ordinal, rejection.source_offset()));
        self.pending = Some(RecoveryPending::Opcr(Pending {
            rejection,
            context,
            phase: 0,
            pts: None,
            header_ranges: BudgetVec::new(&self.budget),
            spans: BudgetVec::new(&self.budget),
        }));
        Ok(())
    }
    pub fn end_of_source(&mut self) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::Invalid("native recovery input not drained"));
        }
        self.inner.end_of_source()
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<DemuxObservation>> {
        self.poll_bound(work, cancel, None)
    }
    pub fn poll_with_input(
        &mut self,
        input: &triergon_core::InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<DemuxObservation>> {
        self.poll_bound(work, cancel, Some(input))
    }
    fn poll_bound(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        input: Option<&triergon_core::InputCursor>,
    ) -> Result<AssemblyStep<DemuxObservation>> {
        self.inner.validate_input(input)?;
        if let Some(error) = self.fault {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel, input);
        if let Err(error) = &result
            && !matches!(
                error,
                Error::Cancelled | Error::Allocation | Error::Backpressure { .. }
            )
        {
            self.fault = Some(*error);
        }
        result
    }
    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        input: Option<&triergon_core::InputCursor>,
    ) -> Result<AssemblyStep<DemuxObservation>> {
        if self.pending.is_none() {
            return self
                .inner
                .poll_bound(work, cancel, input)
                .map(|step| match step {
                    AssemblyStep::Output(packet) => {
                        AssemblyStep::Output(DemuxObservation::Accepted(packet))
                    }
                    AssemblyStep::Yield => AssemblyStep::Yield,
                    AssemblyStep::NeedPayload => AssemblyStep::NeedPayload,
                    AssemblyStep::Finished => AssemblyStep::Finished,
                });
        }
        if matches!(self.pending, Some(RecoveryPending::Selected(_))) {
            return self.poll_selected(work, cancel);
        }
        if let Some(RecoveryPending::TransportFault(job)) = self.pending.as_mut() {
            return Ok(match job.poll(&self.inner, work, cancel)? {
                AssemblyStep::Output(()) => {
                    let Some(RecoveryPending::TransportFault(job)) = self.pending.take() else {
                        unreachable!()
                    };
                    let packet = job.finish(&mut self.inner)?;
                    AssemblyStep::Output(DemuxObservation::FromSelectedDiagnostic(
                        DiagnosticPacket::from(packet),
                    ))
                }
                AssemblyStep::Yield => AssemblyStep::Yield,
                AssemblyStep::NeedPayload => AssemblyStep::NeedPayload,
                AssemblyStep::Finished => {
                    return Err(Error::Invalid("transport fault ended before output"));
                }
            });
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(AssemblyStep::Yield);
            }
            let Some(RecoveryPending::Opcr(p)) = self.pending.as_mut() else {
                return Err(Error::Invalid("native rejected input missing"));
            };
            let start = p.rejection.envelope().payload_start;
            let body = &p.rejection.bytes().as_ref()[start..];
            match p.phase {
                0 => {
                    if body.len() < 14 || body[..4] != [0, 0, 1, 0xe0] {
                        return Err(Error::Unsupported(
                            "native rejected PES prefix/context unobserved",
                        ));
                    }
                }
                1 => {
                    let total = usize::from(u16::from_be_bytes([body[4], body[5]])) + 6;
                    if total != body.len() || total < 14 {
                        return Err(Error::Unsupported(
                            "native rejected finite PES boundary unobserved",
                        ));
                    }
                    if total > self.inner.limits().pes_bytes {
                        return Err(crate::limit(total, self.inner.limits().pes_bytes));
                    }
                }
                2 => {
                    if body[6..9] != [0x80, 0x80, 5] {
                        return Err(Error::Unsupported(
                            "native rejected header flags unobserved",
                        ));
                    }
                }
                3 => {
                    let t = &body[9..14];
                    if t[0] >> 4 != 2 || t[0] & 1 == 0 || t[2] & 1 == 0 || t[4] & 1 == 0 {
                        return Err(Error::Invalid("native rejected timestamp markers"));
                    }
                    p.pts = Some(
                        (u64::from((t[0] >> 1) & 7) << 30)
                            | (u64::from(t[1]) << 22)
                            | (u64::from(t[2] >> 1) << 15)
                            | (u64::from(t[3]) << 7)
                            | u64::from(t[4] >> 1),
                    );
                }
                4 => p.header_ranges.try_reserve_exact(1)?,
                5 => {
                    p.header_ranges
                        .try_push(SourceRange {
                            entity: self.inner.scope().entity,
                            offset: p.rejection.source_offset() + start as u64,
                            bytes: 14,
                        })
                        .map_err(|e| e.error)?;
                }
                6 => p.spans.try_reserve_exact(1)?,
                7 => {
                    if body.len() == 14 {
                        return Err(Error::Unsupported("native empty rejected PES unobserved"));
                    }
                    let scope = self.inner.scope();
                    p.spans
                        .try_push(DemuxSpan {
                            bytes: p
                                .rejection
                                .bytes()
                                .slice(start + 14..p.rejection.bytes().len())?,
                            source: SourceRange {
                                entity: scope.entity,
                                offset: p.rejection.source_offset() + start as u64 + 14,
                                bytes: body.len() - 14,
                            },
                            transport: p.rejection.source_range().ok_or(Error::SourceMismatch)?,
                            context: p.context,
                        })
                        .map_err(|e| e.error)?;
                }
                8 => {
                    let scope = self.inner.scope();
                    let offset = p.rejection.source_offset();
                    let header = EncodedHeader {
                        bytes: p.rejection.bytes().slice(start..start + 14)?,
                        pts: p.pts.ok_or(Error::Invalid("native encoded PTS missing"))?,
                    };
                    let position = self.inner.media_position(offset)?;
                    let Some(RecoveryPending::Opcr(p)) = self.pending.take() else {
                        return Err(Error::Invalid("native rejected input missing"));
                    };
                    self.inner.note_recovered(
                        p.rejection.envelope().cc,
                        offset,
                        p.context.source_event_ordinal,
                    );
                    return Ok(AssemblyStep::Output(DemuxObservation::FromRejectedCarrier(
                        RecoveredPacket {
                            profile: self.inner.profile(),
                            program: scope.program,
                            pid: scope.pid,
                            source_origin: SourceLocation {
                                source: scope.entity,
                                offset,
                            },
                            source_epochs: scope.epochs,
                            position,
                            flags: 0,
                            duration90_raw: 0,
                            stream_id: 0xe0,
                            payload_len: p.spans.as_slice()[0].as_ref().len(),
                            rejection: p.rejection,
                            context: p.context,
                            encoded_header: header,
                            header_source_ranges: p.header_ranges,
                            spans: p.spans,
                        },
                    )));
                }
                _ => return Err(Error::Invalid("native rejected input stage")),
            }
            p.phase += 1;
        }
    }
}
