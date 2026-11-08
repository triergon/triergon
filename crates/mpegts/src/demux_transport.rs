//! Opt-in finite PES extraction from explicitly damaged or opaque transport.
//!
//! H.222.0 (08/2023) 2.4.3.3: TEI denotes an uncorrectable error; scrambling
//! does not establish clear payload. Neither flag is cleared or repaired here.
use crate::demux_pes::{DemuxObserver, DemuxPacket, DemuxScope, DemuxSpan, SourceContext};
use crate::{AssemblyStep, FaultKind, SourceEpochs, SourceRange, TraceFault, TransportPacket};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{BudgetVec, Bytes, Error, PushError, Result, SourceLocation, WorkBudget};

/// The sole transport fault admitted by this compatibility policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportFaultKind {
    TransportError,
    Scrambling,
}
impl TransportFaultKind {
    fn source_error(self) -> Error {
        match self {
            Self::TransportError => Error::Invalid("TS transport error indicator"),
            Self::Scrambling => Error::Unsupported("scrambled TS payload"),
        }
    }
}

/// Checked encoding in damaged or opaque bytes, never a validated source header.
#[derive(Debug)]
pub struct TransportFaultHeader {
    bytes: Bytes,
    pts: u64,
}
impl TransportFaultHeader {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn encoded_pts_raw33(&self) -> u64 {
        self.pts
    }
    /// This policy admits only PTS-only encoding. It never creates source DTS.
    pub fn encoded_dts_raw33(&self) -> Option<u64> {
        None
    }
}

/// A compatibility packet retaining its real source fault and unchanged carrier.
/// Payload bytes may be damaged or scrambled; this is neither source PES nor an AU.
#[derive(Debug)]
pub struct TransportFaultPacket {
    pub program: u16,
    pub pid: u16,
    pub source_origin: SourceLocation,
    pub source_epochs: SourceEpochs,
    pub position: i64,
    pub flags: u32,
    pub duration90_raw: i64,
    pub stream_id: u8,
    pub kind: TransportFaultKind,
    pub source_fault: TraceFault,
    pub carrier: TransportPacket,
    pub input_context: SourceContext,
    pub diagnostic_context: SourceContext,
    pub encoded_header: TransportFaultHeader,
    pub header_source_ranges: BudgetVec<SourceRange>,
    pub payload_len: usize,
    pub spans: BudgetVec<DemuxSpan>,
}
impl TransportFaultPacket {
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
    }
}

/// Accepted assembly and faulted extraction remain distinct results.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "charged inputs and outputs stay inline"
)]
pub enum TransportObservation {
    Accepted(DemuxPacket),
    Faulted(TransportFaultPacket),
}

/// Add explicit TEI/scrambling handling to an ordinary PES observer.
///
/// Ordinary input uses the existing assembler unchanged. Faulted input requires
/// drained AVC carriage, TS188 PUSI, a single complete nonempty finite PES with
/// PTS-only encoding, no other transport/header fault, and the actual matching
/// source diagnostic. Other contexts remain unsupported. Drain `poll` after each
/// input; `NeedPayload` while awaiting a diagnostic does not release the carrier.
/// There is no implicit entity, clock or configuration transition.
#[derive(Debug)]
pub struct TransportFaultRecovery {
    inner: DemuxObserver,
    pending: Option<TransportFaultJob>,
    fault: Option<Error>,
}
impl TransportFaultRecovery {
    /// Wrap a drained, unbound observer. No storage is allocated.
    /// Rejection returns the original observer with its pending state intact.
    #[allow(
        clippy::result_large_err,
        reason = "return the original observer inline"
    )]
    pub fn new(inner: DemuxObserver) -> std::result::Result<Self, PushError<DemuxObserver>> {
        let error = inner.validate_input(None).err().or_else(|| {
            (!inner.recovery_idle()).then_some(Error::Invalid(
                "transport recovery requires drained observer",
            ))
        });
        if let Some(error) = error {
            return Err(PushError {
                error,
                value: inner,
            });
        }
        Ok(Self {
            inner,
            pending: None,
            fault: None,
        })
    }
    pub fn scope(&self) -> DemuxScope {
        self.inner.scope()
    }
    /// Whether the pending original carrier still needs its source diagnostic.
    pub fn awaiting_source_fault(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|job| job.diagnostic.is_none())
    }
    #[allow(
        clippy::result_large_err,
        reason = "return the original charged carrier inline"
    )]
    pub fn push(
        &mut self,
        packet: TransportPacket,
        context: SourceContext,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        let error = self.fault.or_else(|| {
            self.pending
                .as_ref()
                .map(|_| Error::Invalid("transport recovery input not drained"))
        });
        if let Some(error) = error {
            return Err(PushError {
                error,
                value: packet,
            });
        }
        if packet.transport_error || packet.scrambling != 0 {
            self.pending = Some(TransportFaultJob::new(packet, context, &self.inner, None)?);
            Ok(())
        } else {
            self.inner.push(packet, context)
        }
    }
    /// A stale or unrelated diagnostic leaves the pending original input intact.
    pub fn confirm_source_fault(
        &mut self,
        fault: &TraceFault,
        context: SourceContext,
    ) -> Result<()> {
        if let Some(error) = self.fault {
            return Err(error);
        }
        self.pending
            .as_mut()
            .ok_or(Error::Invalid("no transport fault awaiting diagnostic"))?
            .confirm(fault, context)
    }
    pub fn end_of_source(&mut self) -> Result<()> {
        if let Some(error) = self.fault {
            return Err(error);
        }
        if self.pending.is_some() {
            return Err(Error::Invalid("transport recovery input not drained"));
        }
        self.inner.end_of_source()
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<TransportObservation>> {
        if let Some(error) = self.fault {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result
            && !matches!(
                error,
                Error::Cancelled | Error::Allocation | Error::Backpressure { .. }
            )
        {
            self.fault = Some(error);
        }
        result
    }
    fn poll_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<TransportObservation>> {
        let Some(job) = self.pending.as_mut() else {
            return self.inner.poll(work, cancel).map(|step| match step {
                AssemblyStep::Output(packet) => {
                    AssemblyStep::Output(TransportObservation::Accepted(packet))
                }
                AssemblyStep::Yield => AssemblyStep::Yield,
                AssemblyStep::NeedPayload => AssemblyStep::NeedPayload,
                AssemblyStep::Finished => AssemblyStep::Finished,
            });
        };
        Ok(match job.poll(&self.inner, work, cancel)? {
            AssemblyStep::Output(()) => {
                let job = self
                    .pending
                    .take()
                    .ok_or(Error::Invalid("transport fault input missing"))?;
                AssemblyStep::Output(TransportObservation::Faulted(job.finish(&mut self.inner)?))
            }
            AssemblyStep::Yield => AssemblyStep::Yield,
            AssemblyStep::NeedPayload => AssemblyStep::NeedPayload,
            AssemblyStep::Finished => {
                return Err(Error::Invalid("transport fault ended before output"));
            }
        })
    }
}

/// Shared with the comparison-only adapter; all parsing and storage phases live here.
#[derive(Debug)]
pub(crate) struct TransportFaultJob {
    packet: TransportPacket,
    kind: TransportFaultKind,
    input: SourceContext,
    diagnostic: Option<(TraceFault, SourceContext)>,
    start: usize,
    phase: u8,
    pts: u64,
    header: Option<TransportFaultHeader>,
    position: i64,
    header_ranges: BudgetVec<SourceRange>,
    spans: BudgetVec<DemuxSpan>,
}
impl TransportFaultJob {
    #[allow(
        clippy::result_large_err,
        reason = "recover original charged input without allocating"
    )]
    pub(crate) fn new(
        packet: TransportPacket,
        input: SourceContext,
        inner: &DemuxObserver,
        cursor: Option<&triergon_core::InputCursor>,
    ) -> std::result::Result<Self, PushError<TransportPacket>> {
        let admission = (|| {
            let budget = inner.transport_fault_input(&packet, input, cursor)?;
            if packet.source.len() != 188 || packet.prefix.is_some() || !packet.payload_unit_start {
                return Err(Error::Unsupported(
                    "selected diagnostic envelope unobserved",
                ));
            }
            let body = packet
                .payload
                .as_ref()
                .ok_or(Error::Unsupported("selected diagnostic no payload"))?;
            let start = usize::try_from(
                packet
                    .payload_source_offset
                    .checked_sub(packet.source_offset)
                    .ok_or(Error::Overflow)?,
            )
            .map_err(|_| Error::Overflow)?;
            if start.checked_add(body.len()) != Some(188) {
                return Err(Error::Invalid("selected diagnostic payload extent"));
            }
            let kind = match (packet.transport_error, packet.scrambling) {
                (true, 0) => TransportFaultKind::TransportError,
                (false, 1..=3) => TransportFaultKind::Scrambling,
                _ => {
                    return Err(Error::Unsupported(
                        "selected diagnostic additional transport fault/context",
                    ));
                }
            };
            Ok((
                kind,
                start,
                inner.media_position(packet.source_offset)?,
                BudgetVec::new(budget),
                BudgetVec::new(budget),
            ))
        })();
        let (kind, start, position, header_ranges, spans) = match admission {
            Ok(value) => value,
            Err(error) => {
                return Err(PushError {
                    error,
                    value: packet,
                });
            }
        };
        Ok(Self {
            packet,
            kind,
            input,
            diagnostic: None,
            start,
            phase: 0,
            pts: 0,
            header: None,
            position,
            header_ranges,
            spans,
        })
    }
    pub(crate) fn confirm(&mut self, fault: &TraceFault, context: SourceContext) -> Result<()> {
        if self.diagnostic.is_some()
            || fault.rejected.is_some()
            || fault.kind != FaultKind::Transport
            || fault.error != self.kind.source_error()
            || fault.pid != Some(self.packet.pid)
            || fault.location.source
                != self
                    .packet
                    .source_id
                    .ok_or(Error::Invalid("transport fault source missing"))?
            || fault.location.offset != self.packet.source_offset
            || context.epochs != self.input.epochs
            || context.byte_run_epoch.is_some()
            || context.source_event_ordinal <= self.input.source_event_ordinal
        {
            return Err(Error::Invalid(
                "selected source diagnostic context mismatch",
            ));
        }
        self.diagnostic = Some((fault.clone(), context));
        Ok(())
    }
    /// Only turn pressure into a limit when storage retained by this operation
    /// cannot fit even after every external reservation/output is released.
    fn check_reservation(&self, inner: &DemuxObserver, result: Result<()>) -> Result<()> {
        if matches!(result, Err(Error::Backpressure { .. })) {
            let budget = self.header_ranges.budget();
            let adaptation = self.packet.adaptation.as_ref();
            let storage = [
                Some(&self.packet.source),
                self.packet.prefix.as_ref(),
                self.packet.payload.as_ref(),
                adaptation.map(|field| &field.source),
                adaptation.and_then(|field| field.private_data.as_ref()),
                adaptation.and_then(|field| field.extension.as_ref()),
            ];
            let mut minimum = triergon_core::Budget::control_storage_bytes()
                .checked_add(inner.recovery_storage_bytes()?)
                .ok_or(Error::Overflow)?;
            for (index, bytes) in storage.iter().enumerate() {
                if let Some(bytes) = bytes
                    && bytes.belongs_to(budget)
                    && !storage[..index]
                        .iter()
                        .flatten()
                        .any(|prior| prior.shares_storage_with(bytes))
                {
                    minimum = minimum
                        .checked_add(bytes.retained_storage_bytes())
                        .ok_or(Error::Overflow)?;
                }
            }
            minimum = self
                .header_ranges
                .capacity()
                .max(1)
                .checked_mul(size_of::<SourceRange>())
                .and_then(|bytes| minimum.checked_add(bytes))
                .and_then(|bytes| {
                    self.spans
                        .capacity()
                        .max(1)
                        .checked_mul(size_of::<DemuxSpan>())
                        .and_then(|spans| bytes.checked_add(spans))
                })
                .ok_or(Error::Overflow)?;
            if minimum > budget.limit() {
                return Err(Error::Limit {
                    requested: minimum,
                    maximum: budget.limit(),
                });
            }
        }
        result
    }
    /// Each phase checks at most a fixed six-byte field or commits one span action.
    pub(crate) fn poll(
        &mut self,
        inner: &DemuxObserver,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<()>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(AssemblyStep::Yield);
            }
            if self.diagnostic.is_none() {
                return Ok(AssemblyStep::NeedPayload);
            }
            if !work.try_charge(1) {
                return Ok(AssemblyStep::Yield);
            }
            let raw = self.packet.source.as_ref();
            let body = raw
                .get(self.start..)
                .ok_or(Error::Invalid("selected diagnostic payload offset"))?;
            match self.phase {
                0 => {
                    if raw[0] != 0x47
                        || raw[1] & 0x40 == 0
                        || (u16::from(raw[1] & 31) << 8) | u16::from(raw[2]) != inner.scope().pid
                        || raw[3] & 0x30 != 0x30
                        || self.start != 5 + usize::from(raw[4])
                        || !(6..=188).contains(&self.start)
                    {
                        return Err(Error::Unsupported(
                            "selected diagnostic raw envelope unobserved",
                        ));
                    }
                    if inner
                        .recovery_counter()
                        .is_some_and(|cc| ((cc + 1) & 15) != (raw[3] & 15))
                    {
                        return Err(Error::Unsupported(
                            "selected diagnostic counter damage unobserved",
                        ));
                    }
                    if (raw[1] & 0x80 != 0) != self.packet.transport_error
                        || raw[3] >> 6 != self.packet.scrambling
                        || raw[5] != 0
                        || raw[3] & 15 != self.packet.continuity_counter
                    {
                        return Err(Error::Unsupported(
                            "selected diagnostic additional transport fault/context",
                        ));
                    }
                }
                1 => {
                    if body.len() < 9 || body[..4] != [0, 0, 1, 0xe0] {
                        return Err(Error::Unsupported(
                            "selected diagnostic PES prefix unobserved",
                        ));
                    }
                }
                2 => {
                    let declared = usize::from(u16::from_be_bytes([body[4], body[5]]));
                    if declared == 0 || declared + 6 != body.len() {
                        return Err(Error::Unsupported(
                            "selected incomplete/unbounded finite PES unobserved",
                        ));
                    }
                    if body.len() > inner.limits().pes_bytes {
                        return Err(crate::limit(body.len(), inner.limits().pes_bytes));
                    }
                }
                3 => {
                    if body[6..9] != [0x80, 0x80, 5] {
                        return Err(Error::Unsupported(
                            "selected diagnostic additional header fault/context",
                        ));
                    }
                    if body.len() <= 14 {
                        return Err(Error::Unsupported(
                            "selected diagnostic empty/truncated PES unobserved",
                        ));
                    }
                }
                4 => {
                    self.pts = crate::pes::timestamp(&body[9..14], 2).map_err(|_| {
                        Error::Unsupported("selected diagnostic additional timestamp fault/prefix")
                    })?;
                }
                5 => {
                    let result = self.header_ranges.try_reserve_exact(1);
                    self.check_reservation(inner, result)?;
                }
                6 => self
                    .header_ranges
                    .try_push(SourceRange {
                        entity: inner.scope().entity,
                        offset: self.packet.payload_source_offset,
                        bytes: 14,
                    })
                    .map_err(|failure| failure.error)?,
                7 => {
                    let result = self.spans.try_reserve_exact(1);
                    self.check_reservation(inner, result)?;
                }
                8 => self
                    .spans
                    .try_push(DemuxSpan {
                        bytes: self.packet.source.slice(self.start + 14..188)?,
                        source: SourceRange {
                            entity: inner.scope().entity,
                            offset: self.packet.payload_source_offset + 14,
                            bytes: body.len() - 14,
                        },
                        transport: SourceRange {
                            entity: inner.scope().entity,
                            offset: self.packet.source_offset,
                            bytes: 188,
                        },
                        context: self.input,
                    })
                    .map_err(|failure| failure.error)?,
                9 => {
                    self.header = Some(TransportFaultHeader {
                        bytes: self.packet.source.slice(self.start..self.start + 14)?,
                        pts: self.pts,
                    });
                    self.phase = 10;
                    return Ok(AssemblyStep::Output(()));
                }
                _ => return Err(Error::Invalid("transport fault phase")),
            }
            self.phase += 1;
        }
    }
    pub(crate) fn finish(self, inner: &mut DemuxObserver) -> Result<TransportFaultPacket> {
        let header = self
            .header
            .ok_or(Error::Invalid("transport fault header unfinished"))?;
        let (source_fault, diagnostic_context) = self
            .diagnostic
            .ok_or(Error::Invalid("selected source diagnostic missing"))?;
        let scope = inner.scope();
        inner.note_recovered(
            self.packet.continuity_counter,
            self.packet.source_offset,
            diagnostic_context.source_event_ordinal,
        );
        Ok(TransportFaultPacket {
            program: scope.program,
            pid: scope.pid,
            source_origin: SourceLocation {
                source: scope.entity,
                offset: self.packet.source_offset,
            },
            source_epochs: self.input.epochs,
            position: self.position,
            flags: 0,
            duration90_raw: 0,
            stream_id: scope.stream_id,
            kind: self.kind,
            source_fault,
            carrier: self.packet,
            input_context: self.input,
            diagnostic_context,
            encoded_header: header,
            payload_len: self.spans.as_slice()[0].as_ref().len(),
            header_source_ranges: self.header_ranges,
            spans: self.spans,
        })
    }
}
