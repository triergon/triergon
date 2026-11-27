//! The demuxer's packets from rejected carriers.
//!
//! [`RecoveryObserver`] extends [`DemuxObserver`] with packets from rejected OPCR
//! carriers ([`RecoveredPacket`]) or selected AVC diagnostics ([`PesFaultPacket`]).
//! It keeps the original carrier and follows one program through PAT and PMT
//! changes ([`PatSelection`], [`PmtBindingJob`]).
use crate::compat::pes::fault::{PesFaultPacket, TransportFaultJob};
use crate::compat::pes::observer::{
    DemuxObserver, DemuxPacket, DemuxScope, DemuxSpan, PrivateAudioCarriage,
    PrivateAudioDescriptors, RegisteredCarriage, SourceContext,
};
use crate::pes::stream_id;
use crate::psi::stream_type;
use crate::transport::OPCR_WITHOUT_PCR;
use crate::transport::{RejectedTransport, TransportPacket};
use crate::{AssemblyStep, PacketFormat, ParseLimits, SourceEpochs, SourceRange};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Refused, Result, SourceLocation, WorkBudget};

mod lifecycle;
mod selected;
pub use lifecycle::{PatRoute, PatSelection, PmtBinding, PmtBindingJob};

/// Which rejected carriers a [`RecoveryObserver`] assembles packets from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectedPolicy {
    /// This carriage profile has no observed rejected-carrier operation.
    NoRejectedCarrier,
    /// A fully checked TS188 AVC unit start rejected only for an OPCR without a PCR.
    OpcrWithoutPcr,
}

/// Checked header bytes and raw PTS retained from a carrier rejected by the source reader.
#[derive(Debug)]
pub struct EncodedHeader {
    bytes: Bytes,
    pts: u64,
}
impl EncodedHeader {
    /// The PES header bytes as encoded in the rejected carrier.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }
    /// The encoded PTS field, a 33-bit value in 90 kHz ticks.
    pub fn encoded_pts_raw33(&self) -> u64 {
        self.pts
    }
    /// Always `None`: an encoded DTS is never used.
    pub fn encoded_dts_raw33(&self) -> Option<u64> {
        None
    }
    /// Charged backing for the exact original header range, shared with the source carrier.
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }
}

/// A packet the demuxer emits from a carrier the source view rejected.
#[derive(Debug)]
pub struct RecoveredPacket {
    /// The program number of the observed scope.
    pub program: u16,
    /// The 13-bit PID of the observed scope.
    pub pid: u16,
    /// Where the rejected carrier begins in the source.
    pub source_origin: SourceLocation,
    /// The clock and configuration epochs of the observed scope.
    pub source_epochs: SourceEpochs,
    /// The demuxer's byte position of the packet.
    pub position: i64,
    /// The demuxer's packet flags, as in [`DemuxPacket::flags`].
    pub flags: u32,
    /// The demuxer's duration in 90 kHz ticks; 0 is its unknown sentinel.
    pub duration90_raw: i64,
    /// The PES stream ID of the observed scope.
    pub stream_id: u8,
    /// The unchanged rejected carrier and its source fault.
    pub rejection: RejectedTransport,
    /// The source context the rejection was pushed with.
    pub context: SourceContext,
    /// Original PES header bytes retained from the rejected carrier.
    pub encoded_header: EncodedHeader,
    /// Where the header bytes lie in the source.
    pub header_source_ranges: BudgetVec<SourceRange>,
    /// Payload bytes after the header.
    pub payload_len: usize,
    /// Original carrier-backed byte spans in elementary order, covering `payload_len`.
    pub spans: BudgetVec<DemuxSpan>,
}
impl RecoveredPacket {
    /// The payload bytes, one slice per span.
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
    }
}
/// One packet of a [`RecoveryObserver`].
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one owned inline observation; dynamic storage is charged, no Box"
)]
pub enum DemuxObservation {
    /// A packet of accepted input, from the wrapped observer.
    Accepted(DemuxPacket),
    /// A packet from a carrier the source view rejected.
    FromRejectedCarrier(RecoveredPacket),
    /// A packet from a selected carrier with a source diagnostic.
    FromSelectedDiagnostic(PesFaultPacket),
}

#[derive(Debug)]
enum OpcrStage {
    Prefix,
    Length,
    Flags,
    Timestamp,
    ReserveHeader { pts: u64 },
    PushHeader { pts: u64 },
    ReserveSpan { pts: u64 },
    PushSpan { pts: u64 },
    Emit { pts: u64 },
}

#[derive(Debug)]
struct Pending {
    rejection: RejectedTransport,
    context: SourceContext,
    stage: OpcrStage,
    header_ranges: BudgetVec<SourceRange>,
    spans: BudgetVec<DemuxSpan>,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one pending carrier inline; dynamic storage is charged, no Box"
)]
enum RecoveryPending {
    Opcr(Pending),
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
    /// An observer of a registered carriage on the scope's PID that also assembles packets from
    /// rejected carriers under `policy`.
    ///
    /// Fails as [`DemuxObserver::new_registered`] does.
    pub fn new(
        scope: DemuxScope,
        carriage: RegisteredCarriage,
        policy: RejectedPolicy,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        let inner = DemuxObserver::new_registered(scope, carriage, budget.clone(), limits)?;
        Ok(Self::with_observer(inner, budget, policy))
    }
    /// Observe finite private-audio PES from accepted carriers.
    ///
    /// Fails as [`DemuxObserver::new_private_audio`] does.
    pub fn new_private_audio(
        scope: DemuxScope,
        carriage: PrivateAudioCarriage,
        descriptors: PrivateAudioDescriptors<'_>,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        let inner =
            DemuxObserver::new_private_audio(scope, carriage, descriptors, budget.clone(), limits)?;
        Ok(Self::with_observer(
            inner,
            budget,
            RejectedPolicy::NoRejectedCarrier,
        ))
    }
    /// Observe finite DVB subtitle PES in the selected program. The namespace,
    /// descriptors and stream identifiers must satisfy [`DemuxObserver::new_dvb_subtitles`].
    pub fn new_dvb_subtitles(
        scope: DemuxScope,
        namespace: crate::psi::DescriptorNamespace,
        descriptors: crate::compat::pes::observer::DvbSubtitlesDescriptors<'_>,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        let inner = DemuxObserver::new_dvb_subtitles(
            scope,
            namespace,
            descriptors,
            budget.clone(),
            limits,
        )?;
        Ok(Self::with_observer(
            inner,
            budget,
            RejectedPolicy::NoRejectedCarrier,
        ))
    }

    fn with_observer(inner: DemuxObserver, budget: Budget, policy: RejectedPolicy) -> Self {
        Self {
            inner,
            budget,
            policy,
            pmt_binding: None,
            pending: None,
            selected_faults: false,
            pending_timeline: None,
            last: None,
            entity_first: false,
            fault: None,
        }
    }

    /// Binds the observer to the input positions `context` found; see
    /// [`DemuxObserver::bind_input`].
    ///
    /// Fails with `Invalid` after input was pushed, and as [`DemuxObserver::bind_input`] does.
    pub fn bind_input(
        &mut self,
        context: &crate::compat::input::InputContext,
        input: &triergon_core::InputCursor,
    ) -> Result<()> {
        if self.pending.is_some() || self.last.is_some() {
            return Err(Error::Invalid(
                "PES recovery startup context must be bound before input",
            ));
        }
        self.inner.bind_input(context, input)
    }
    /// The selected program and PID of the wrapped observer.
    pub fn scope(&self) -> DemuxScope {
        self.inner.scope()
    }
    /// The configured rule for whether to admit payload from a rejected transport carrier.
    pub fn policy(&self) -> RejectedPolicy {
        self.policy
    }
    /// The rejected carrier held for assembly, if any.
    pub fn pending_rejection(&self) -> Option<&RejectedTransport> {
        self.pending.as_ref().and_then(|p| match p {
            RecoveryPending::Opcr(p) => Some(&p.rejection),
            RecoveryPending::TransportFault(job) => job.rejection(),
        })
    }
    /// Pushes the next accepted carrier of the selected PID in source order.
    ///
    /// Refuses the carrier intact while a pushed input is not drained, out of source order, before
    /// a pending PMT binding resolves, once the observer has failed, or when it is bound to an
    /// input.
    #[expect(
        clippy::result_large_err,
        reason = "recover original charged carrier without allocating a Box"
    )]
    pub fn push_accepted(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
    ) -> std::result::Result<(), Refused<TransportPacket>> {
        self.push_accepted_bound(packet, context, None)
    }
    /// Admit an accepted carrier against the bound source cursor. A mismatch in
    /// entity, request or byte pool returns the original carrier intact. Other
    /// admission rules follow [`Self::push_accepted`].
    #[expect(
        clippy::result_large_err,
        reason = "recover original charged inline carrier"
    )]
    pub fn push_accepted_with_input(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
        input: &triergon_core::InputCursor,
    ) -> std::result::Result<(), Refused<TransportPacket>> {
        self.push_accepted_bound(packet, context, Some(input))
    }
    #[expect(
        clippy::result_large_err,
        reason = "recover original charged inline carrier"
    )]
    fn push_accepted_bound(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
        input: Option<&triergon_core::InputCursor>,
    ) -> std::result::Result<(), Refused<TransportPacket>> {
        if let Err(error) = self.inner.validate_input(input) {
            return Err(Refused {
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
            return Err(Refused {
                error: Error::Invalid("PES recovery input is pending or out of order"),
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
                return Err(Refused {
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
            self.inner.push_unconfirmed(packet, context, Some(input))?;
        } else {
            self.inner.push_unconfirmed(packet, context, None)?;
        }
        self.last = Some((context.source_event_ordinal, offset));
        self.entity_first = false;
        Ok(())
    }
    /// Pushes a carrier the source view rejected, to assemble its packet under the policy.
    ///
    /// Refuses the rejection intact with `Unsupported` outside the policy's carrier shape, with
    /// `Invalid` while input is not drained or out of source order, and with `ForeignPool` for
    /// another byte pool.
    #[expect(
        clippy::result_large_err,
        reason = "recover original charged rejection, error and full storage inline"
    )]
    pub fn push_rejected(
        &mut self,
        rejection: RejectedTransport,
        context: SourceContext,
    ) -> std::result::Result<(), Refused<RejectedTransport>> {
        // This finite-input position view does not admit new rejected-carrier
        // contexts. Unbound recovery is unaffected.
        if let Err(error) = self.inner.validate_input(None) {
            return Err(Refused {
                error,
                value: rejection,
            });
        }
        let scope = self.scope();
        let f = rejection.envelope();
        let error = if let Some(error) = self.fault {
            Some(error)
        } else if self.pending.is_some() || !self.inner.recovery_idle() {
            Some(Error::Invalid(
                "rejected-carrier recovery must drain pending input",
            ))
        } else if !rejection.bytes().belongs_to(&self.budget) {
            Some(Error::ForeignPool)
        } else if self.policy == RejectedPolicy::NoRejectedCarrier {
            Some(Error::Unsupported(if self.inner.is_dvb_subtitles() {
                "DVB subtitle PES does not recover rejected carriers"
            } else {
                "private audio PES does not recover rejected carriers"
            }))
        } else if rejection.source_id() != Some(scope.entity)
            || context.epochs != scope.epochs
            || context.byte_run_epoch.is_some()
        {
            Some(Error::Unsupported(
                "rejected carrier belongs to a different entity or source epoch",
            ))
        } else if !self.after_pmt_binding(context)
            || self.last.is_some_and(|(event, offset)| {
                context.source_event_ordinal <= event || rejection.source_offset() <= offset
            })
        {
            Some(Error::Invalid(
                "PES recovery source events are out of order",
            ))
        } else if rejection.error() != Error::Invalid(OPCR_WITHOUT_PCR)
            || rejection.secondary_error().is_some()
            || !rejection.fully_checked()
        {
            Some(Error::Unsupported(
                "PES recovery requires a fully checked carrier rejected only for OPCR without PCR",
            ))
        } else if rejection.format() != PacketFormat::Ts188
            || scope.stream_type != stream_type::H264
            || scope.stream_id != stream_id::VIDEO_FIRST
            || f.pid != scope.pid
            || !f.pusi
            || f.tei
            || f.scrambling != 0
            || f.afc != 3
            || f.flags != 8
        {
            Some(Error::Unsupported(
                "rejected carrier lacks the TS188 start and adaptation fields required for AVC PES recovery",
            ))
        } else {
            None
        };
        if let Some(error) = error {
            return Err(Refused {
                error,
                value: rejection,
            });
        }
        self.last = Some((context.source_event_ordinal, rejection.source_offset()));
        self.pending = Some(RecoveryPending::Opcr(Pending {
            rejection,
            context,
            stage: OpcrStage::Prefix,
            header_ranges: BudgetVec::new(&self.budget),
            spans: BudgetVec::new(&self.budget),
        }));
        Ok(())
    }
    /// Declares logical end of the source.
    ///
    /// Fails with `Invalid` while a pushed input is not drained, and as
    /// [`DemuxObserver::end_of_source`] does.
    pub fn end_of_source(&mut self) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::Invalid(
                "rejected-carrier recovery must drain pending input",
            ));
        }
        self.inner.end_of_source()
    }
    /// Advances within `work` and returns the next packet; see [`DemuxObserver::poll`] for the
    /// steps and errors.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<DemuxObservation>> {
        self.poll_bound(work, cancel, None)
    }
    /// Poll recovery for the bound input, with the work and outcomes of [`Self::poll`].
    /// A cursor identity or pool mismatch is refused before assembly advances.
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
            && !crate::retryable(*error)
        {
            self.fault = Some(*error);
        }
        result
    }
    /// Polls the pending transport fault `job`, which stays pending until it has output.
    fn poll_transport_fault(
        &mut self,
        mut job: TransportFaultJob,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<DemuxObservation>> {
        let step = match job.poll(&self.inner, work, cancel) {
            Ok(AssemblyStep::Output(())) => {
                let packet = job.finish_staged(&mut self.inner)?;
                return Ok(AssemblyStep::Output(
                    DemuxObservation::FromSelectedDiagnostic(packet),
                ));
            }
            Ok(AssemblyStep::Yield) => Ok(AssemblyStep::Yield),
            Ok(AssemblyStep::NeedPayload) => Ok(AssemblyStep::NeedPayload),
            Ok(AssemblyStep::Finished) => {
                Err(Error::Invalid("transport fault ended before output"))
            }
            Err(error) => Err(error),
        };
        self.pending = Some(RecoveryPending::TransportFault(job));
        step
    }

    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        input: Option<&triergon_core::InputCursor>,
    ) -> Result<AssemblyStep<DemuxObservation>> {
        let mut pending = match self.pending.take() {
            None => {
                return self
                    .inner
                    .poll_bound(work, cancel, input)
                    .map(|step| step.map(DemuxObservation::Accepted));
            }
            Some(RecoveryPending::TransportFault(job)) => {
                return self.poll_transport_fault(job, work, cancel);
            }
            Some(RecoveryPending::Opcr(pending)) => pending,
        };
        let (encoded_header, position) = match self.poll_opcr(&mut pending, work, cancel) {
            Ok(Some(output)) => output,
            step => {
                self.pending = Some(RecoveryPending::Opcr(pending));
                return step.map(|_| AssemblyStep::Yield);
            }
        };
        let scope = self.inner.scope();
        let offset = pending.rejection.source_offset();
        self.inner.note_recovered(
            pending.rejection.envelope().cc,
            offset,
            pending.context.source_event_ordinal,
        );
        Ok(AssemblyStep::Output(DemuxObservation::FromRejectedCarrier(
            RecoveredPacket {
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
                stream_id: stream_id::VIDEO_FIRST,
                payload_len: pending.spans.as_slice()[0].as_ref().len(),
                rejection: pending.rejection,
                context: pending.context,
                encoded_header,
                header_source_ranges: pending.header_ranges,
                spans: pending.spans,
            },
        )))
    }

    fn poll_opcr(
        &self,
        pending: &mut Pending,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<(EncodedHeader, i64)>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(None);
            }
            let start = pending.rejection.envelope().payload_start;
            let body = &pending.rejection.bytes().as_ref()[start..];
            match pending.stage {
                OpcrStage::Prefix => {
                    if body.len() < 14 || body[..4] != [0, 0, 1, stream_id::VIDEO_FIRST] {
                        return Err(Error::Unsupported(
                            "rejected carrier lacks a complete AVC PES header",
                        ));
                    }
                    pending.stage = OpcrStage::Length;
                }
                OpcrStage::Length => {
                    let total = usize::from(u16::from_be_bytes([body[4], body[5]])) + 6;
                    if total != body.len() || total < 14 {
                        return Err(Error::Unsupported(
                            "rejected carrier does not contain one complete finite PES",
                        ));
                    }
                    if total > self.inner.limits().pes_bytes {
                        return Err(crate::limit(total, self.inner.limits().pes_bytes));
                    }
                    pending.stage = OpcrStage::Flags;
                }
                OpcrStage::Flags => {
                    if body[6..9] != [0x80, 0x80, 5] {
                        return Err(Error::Unsupported(
                            "rejected PES requires an MPEG-2 PTS-only header with no extra flags",
                        ));
                    }
                    pending.stage = OpcrStage::Timestamp;
                }
                OpcrStage::Timestamp => {
                    let pts = crate::pes::timestamp(&body[9..14], 2).map_err(|_| {
                        Error::Invalid("invalid PTS prefix or marker bits in rejected PES")
                    })?;
                    pending.stage = OpcrStage::ReserveHeader { pts };
                }
                OpcrStage::ReserveHeader { pts } => {
                    pending.header_ranges.try_reserve_exact(1)?;
                    pending.stage = OpcrStage::PushHeader { pts };
                }
                OpcrStage::PushHeader { pts } => {
                    pending
                        .header_ranges
                        .try_push(SourceRange {
                            entity: self.inner.scope().entity,
                            offset: pending.rejection.source_offset() + start as u64,
                            bytes: 14,
                        })
                        .map_err(|e| e.error)?;
                    pending.stage = OpcrStage::ReserveSpan { pts };
                }
                OpcrStage::ReserveSpan { pts } => {
                    pending.spans.try_reserve_exact(1)?;
                    pending.stage = OpcrStage::PushSpan { pts };
                }
                OpcrStage::PushSpan { pts } => {
                    if body.len() == 14 {
                        return Err(Error::Unsupported(
                            "rejected carrier has an empty PES payload",
                        ));
                    }
                    let scope = self.inner.scope();
                    pending
                        .spans
                        .try_push(DemuxSpan {
                            bytes: pending
                                .rejection
                                .bytes()
                                .slice(start + 14..pending.rejection.bytes().len())?,
                            source: SourceRange {
                                entity: scope.entity,
                                offset: pending.rejection.source_offset() + start as u64 + 14,
                                bytes: body.len() - 14,
                            },
                            transport: pending
                                .rejection
                                .source_range()
                                .ok_or(Error::SourceMismatch)?,
                            context: pending.context,
                        })
                        .map_err(|e| e.error)?;
                    pending.stage = OpcrStage::Emit { pts };
                }
                OpcrStage::Emit { pts } => {
                    let header = EncodedHeader {
                        bytes: pending.rejection.bytes().slice(start..start + 14)?,
                        pts,
                    };
                    let position = self
                        .inner
                        .media_position(pending.rejection.source_offset())?;
                    return Ok(Some((header, position)));
                }
            }
        }
    }
}
