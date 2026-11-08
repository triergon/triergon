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

/// Additional finite-PES recovery choices. Source validation stays strict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PesFaultPolicy {
    pub timing_headers: bool,
    pub rejected_clock_fields: bool,
}

/// A source fault retained by finite-PES extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PesFaultKind {
    TransportError,
    Scrambling,
    TimestampMarkers,
    ForbiddenTimeFlags,
    PcrEncoding,
    OpcrWithoutPcr,
}
impl PesFaultKind {
    fn source_error(self) -> Error {
        match self {
            Self::TransportError => Error::Invalid("TS transport error indicator"),
            Self::Scrambling => Error::Unsupported("scrambled TS payload"),
            Self::TimestampMarkers => Error::Invalid("PES timestamp markers"),
            Self::ForbiddenTimeFlags => Error::Invalid("forbidden PES time flags"),
            Self::PcrEncoding => Error::Invalid("PCR reserved bits/extension"),
            Self::OpcrWithoutPcr => Error::Invalid("OPCR without PCR"),
        }
    }
    fn source_layer(self) -> FaultKind {
        if matches!(self, Self::TimestampMarkers | Self::ForbiddenTimeFlags) {
            FaultKind::Pes
        } else {
            FaultKind::Transport
        }
    }
}

/// The unchanged accepted or rejected carrier. No accepted packet is forged.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "charged source storage stays inline"
)]
pub enum PesFaultCarrier {
    Transport(TransportPacket),
    Rejected(crate::RejectedTransport),
}
impl PesFaultCarrier {
    pub fn backing_bytes(&self) -> &Bytes {
        match self {
            Self::Transport(packet) => &packet.source,
            Self::Rejected(packet) => packet.bytes(),
        }
    }
    pub fn source_offset(&self) -> u64 {
        match self {
            Self::Transport(packet) => packet.source_offset,
            Self::Rejected(packet) => packet.source_offset(),
        }
    }
    fn source_id(&self) -> Option<triergon_core::SourceId> {
        match self {
            Self::Transport(packet) => packet.source_id,
            Self::Rejected(packet) => packet.source_id(),
        }
    }
    fn retained_storage(&self) -> [Option<&Bytes>; 6] {
        match self {
            Self::Transport(packet) => {
                let adaptation = packet.adaptation.as_ref();
                [
                    Some(&packet.source),
                    packet.prefix.as_ref(),
                    packet.payload.as_ref(),
                    adaptation.map(|field| &field.source),
                    adaptation.and_then(|field| field.private_data.as_ref()),
                    adaptation.and_then(|field| field.extension.as_ref()),
                ]
            }
            Self::Rejected(packet) => [Some(packet.bytes()), None, None, None, None, None],
        }
    }
}

/// Raw encoding and its applicability in the separate compatibility view.
#[derive(Debug)]
pub struct PesFaultHeader {
    bytes: Bytes,
    pts: Option<u64>,
    markers_valid: Option<bool>,
    time_flags: u8,
}
impl PesFaultHeader {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn encoded_pts_raw33(&self) -> Option<u64> {
        self.pts
    }
    pub fn encoded_dts_raw33(&self) -> Option<u64> {
        None
    }
    /// Marker validity for an applicable PTS field; forbidden flags leave it unknown.
    pub fn markers_valid(&self) -> Option<bool> {
        self.markers_valid
    }
    pub fn time_flags(&self) -> u8 {
        self.time_flags
    }
}

/// Nonempty finite PES bytes beside the actual source refusal. This is not
/// validated source PES, a codec AU, or a source PCR/PTS/DTS anchor.
#[derive(Debug)]
pub struct PesFaultPacket {
    pub program: u16,
    pub pid: u16,
    pub source_origin: SourceLocation,
    pub source_epochs: SourceEpochs,
    pub position: i64,
    pub flags: u32,
    pub duration90_raw: i64,
    pub stream_id: u8,
    pub kind: PesFaultKind,
    pub source_fault: TraceFault,
    pub carrier: PesFaultCarrier,
    pub input_context: SourceContext,
    pub diagnostic_context: SourceContext,
    pub encoded_header: PesFaultHeader,
    pub header_source_ranges: BudgetVec<SourceRange>,
    pub payload_len: usize,
    pub spans: BudgetVec<DemuxSpan>,
}
impl PesFaultPacket {
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
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
    PesFault(PesFaultPacket),
}

/// Add explicit TEI/scrambling handling to an ordinary PES observer.
///
/// Ordinary input uses the existing assembler unchanged. Default faulted input requires
/// drained AVC carriage, TS188 PUSI, a single complete nonempty finite PES with
/// PTS-only encoding, no other transport/header fault, and the actual matching
/// source diagnostic. Other contexts remain unsupported. Drain `poll` after each
/// input; `NeedPayload` while awaiting a diagnostic does not release the carrier.
/// `with_policy` adds the explicit finite source-fault choices in `PesFaultPolicy`.
/// There is no implicit entity, clock or configuration transition.
#[derive(Debug)]
pub struct TransportFaultRecovery {
    inner: DemuxObserver,
    pending: Option<TransportFaultJob>,
    fault: Option<Error>,
    policy: PesFaultPolicy,
}
impl TransportFaultRecovery {
    /// Wrap a drained, unbound observer. No storage is allocated.
    /// Rejection returns the original observer with its pending state intact.
    #[allow(
        clippy::result_large_err,
        reason = "return the original observer inline"
    )]
    pub fn new(inner: DemuxObserver) -> std::result::Result<Self, PushError<DemuxObserver>> {
        Self::with_policy(inner, PesFaultPolicy::default())
    }
    /// Explicit additional source-fault policies for drained finite AVC TS188.
    #[allow(
        clippy::result_large_err,
        reason = "return the original observer inline"
    )]
    pub fn with_policy(
        inner: DemuxObserver,
        policy: PesFaultPolicy,
    ) -> std::result::Result<Self, PushError<DemuxObserver>> {
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
            policy,
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
        } else if self.policy.timing_headers && TransportFaultJob::header_hint(&packet) {
            self.pending = Some(TransportFaultJob::new_header(packet, context, &self.inner)?);
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
    /// Admit only a sole, fully checked PCR/OPCR rejection and keep it opaque.
    #[allow(
        clippy::result_large_err,
        reason = "return the original charged diagnostic inline"
    )]
    pub fn push_rejected_fault(
        &mut self,
        fault: TraceFault,
        context: SourceContext,
    ) -> std::result::Result<(), PushError<TraceFault>> {
        let error = self
            .fault
            .or_else(|| {
                self.pending
                    .as_ref()
                    .map(|_| Error::Invalid("transport recovery input not drained"))
            })
            .or_else(|| {
                (!self.policy.rejected_clock_fields)
                    .then_some(Error::Unsupported("rejected clock recovery disabled"))
            });
        if let Some(error) = error {
            return Err(PushError {
                error,
                value: fault,
            });
        }
        self.pending = Some(TransportFaultJob::new_rejected(
            fault,
            context,
            &self.inner,
        )?);
        Ok(())
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
                AssemblyStep::Output(
                    if matches!(
                        job.kind,
                        PesFaultKind::TransportError | PesFaultKind::Scrambling
                    ) {
                        TransportObservation::Faulted(job.finish(&mut self.inner)?)
                    } else {
                        TransportObservation::PesFault(job.finish_source(&mut self.inner)?)
                    },
                )
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
    carrier: PesFaultCarrier,
    kind: PesFaultKind,
    input: SourceContext,
    diagnostic: Option<(TraceFault, SourceContext)>,
    start: usize,
    phase: u8,
    pts: Option<u64>,
    header_len: usize,
    header: Option<PesFaultHeader>,
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
        Self::from_transport(packet, input, inner, cursor, false)
    }
    fn header_hint(packet: &TransportPacket) -> bool {
        let Some(body) = packet.payload.as_ref().map(AsRef::as_ref) else {
            return false;
        };
        packet.payload_unit_start
            && body.len() >= 9
            && body[..4] == [0, 0, 1, 0xe0]
            && (body[7] >> 6 == 1
                || (body[7] >> 6 == 2
                    && body.len() >= 14
                    && (body[9] >> 4 != 2
                        || body[9] & 1 == 0
                        || body[11] & 1 == 0
                        || body[13] & 1 == 0)))
    }
    #[allow(
        clippy::result_large_err,
        reason = "return the original charged transport inline"
    )]
    fn new_header(
        packet: TransportPacket,
        input: SourceContext,
        inner: &DemuxObserver,
    ) -> std::result::Result<Self, PushError<TransportPacket>> {
        Self::from_transport(packet, input, inner, None, true)
    }
    #[allow(
        clippy::result_large_err,
        reason = "return the original charged transport inline"
    )]
    fn from_transport(
        packet: TransportPacket,
        input: SourceContext,
        inner: &DemuxObserver,
        cursor: Option<&triergon_core::InputCursor>,
        header_fault: bool,
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
            let kind = match (packet.transport_error, packet.scrambling, header_fault) {
                (false, 0, true) => {
                    if body.as_ref()[7] >> 6 == 1 {
                        PesFaultKind::ForbiddenTimeFlags
                    } else {
                        PesFaultKind::TimestampMarkers
                    }
                }
                (true, 0, false) => PesFaultKind::TransportError,
                (false, 1..=3, false) => PesFaultKind::Scrambling,
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
            carrier: PesFaultCarrier::Transport(packet),
            kind,
            input,
            diagnostic: None,
            start,
            phase: 0,
            pts: None,
            header_len: 0,
            header: None,
            position,
            header_ranges,
            spans,
        })
    }
    #[allow(
        clippy::result_large_err,
        reason = "return the original charged diagnostic inline"
    )]
    fn new_rejected(
        fault: TraceFault,
        input: SourceContext,
        inner: &DemuxObserver,
    ) -> std::result::Result<Self, PushError<TraceFault>> {
        let admission = (|| {
            let rejected = fault
                .rejected
                .as_ref()
                .ok_or(Error::Invalid("rejected carrier missing"))?;
            let budget = inner.rejected_fault_input(rejected, input)?;
            let kind = match rejected.error() {
                Error::Invalid("PCR reserved bits/extension") => PesFaultKind::PcrEncoding,
                Error::Invalid("OPCR without PCR") => PesFaultKind::OpcrWithoutPcr,
                _ => return Err(Error::Unsupported("rejected clock source fault unobserved")),
            };
            let envelope = rejected.envelope();
            if fault.kind != FaultKind::Transport
                || fault.pid.is_some()
                || fault.error != kind.source_error()
                || fault.location.source != inner.scope().entity
                || fault.location.offset != rejected.source_offset()
                || !rejected.fully_checked()
                || rejected.secondary_error().is_some()
                || rejected.format() != crate::PacketFormat::Ts188
                || rejected.bytes().len() != 188
                || envelope.pid != inner.scope().pid
                || !envelope.pusi
                || envelope.tei
                || envelope.scrambling != 0
                || envelope.afc != 3
                || envelope.flags
                    != (if kind == PesFaultKind::PcrEncoding {
                        16
                    } else {
                        8
                    })
            {
                return Err(Error::Unsupported(
                    "rejected clock is not the sole checked source fault",
                ));
            }
            Ok((
                rejected.clone(),
                kind,
                envelope.payload_start,
                inner.media_position(rejected.source_offset())?,
                BudgetVec::new(budget),
                BudgetVec::new(budget),
            ))
        })();
        let (rejected, kind, start, position, header_ranges, spans) = match admission {
            Ok(value) => value,
            Err(error) => {
                return Err(PushError {
                    error,
                    value: fault,
                });
            }
        };
        Ok(Self {
            carrier: PesFaultCarrier::Rejected(rejected),
            kind,
            input,
            diagnostic: Some((fault, input)),
            start,
            phase: 0,
            pts: None,
            header_len: 0,
            header: None,
            position,
            header_ranges,
            spans,
        })
    }
    pub(crate) fn confirm(&mut self, fault: &TraceFault, context: SourceContext) -> Result<()> {
        if self.diagnostic.is_some()
            || fault.rejected.is_some()
            || fault.kind != self.kind.source_layer()
            || fault.error != self.kind.source_error()
            || fault.pid
                != Some(
                    u16::from(self.carrier.backing_bytes().as_ref()[1] & 31) << 8
                        | u16::from(self.carrier.backing_bytes().as_ref()[2]),
                )
            || fault.location.source
                != self
                    .carrier
                    .source_id()
                    .ok_or(Error::Invalid("transport fault source missing"))?
            || fault.location.offset != self.carrier.source_offset()
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
            let storage = self.carrier.retained_storage();
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
            let raw = self.carrier.backing_bytes().as_ref();
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
                    if matches!(self.carrier, PesFaultCarrier::Transport(_))
                        && inner
                            .recovery_counter()
                            .is_some_and(|cc| ((cc + 1) & 15) != (raw[3] & 15))
                    {
                        return Err(Error::Unsupported(
                            "selected diagnostic counter damage unobserved",
                        ));
                    }
                    let tei = raw[1] & 0x80 != 0;
                    let scrambling = raw[3] >> 6;
                    let flags = match self.kind {
                        PesFaultKind::PcrEncoding => 16,
                        PesFaultKind::OpcrWithoutPcr => 8,
                        _ => 0,
                    };
                    let envelope = match self.kind {
                        PesFaultKind::TransportError => tei && scrambling == 0,
                        PesFaultKind::Scrambling => !tei && scrambling != 0,
                        _ => !tei && scrambling == 0,
                    };
                    if !envelope || raw[5] != flags {
                        return Err(Error::Unsupported(
                            "selected diagnostic additional transport fault/context",
                        ));
                    }
                    if let PesFaultCarrier::Transport(packet) = &self.carrier
                        && (tei != packet.transport_error
                            || scrambling != packet.scrambling
                            || raw[3] & 15 != packet.continuity_counter)
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
                    let forbidden = self.kind == PesFaultKind::ForbiddenTimeFlags;
                    if body[6] != 0x80
                        || body[7] != (if forbidden { 0x40 } else { 0x80 })
                        || (!forbidden && body[8] != 5)
                        || (forbidden && !matches!(body[8], 0 | 5))
                    {
                        return Err(Error::Unsupported(
                            "selected diagnostic additional header fault/context",
                        ));
                    }
                    self.header_len = 9 + usize::from(body[8]);
                    if body.len() <= self.header_len {
                        return Err(Error::Unsupported(
                            "selected diagnostic empty/truncated PES unobserved",
                        ));
                    }
                }
                4 => {
                    if self.header_len == 14 {
                        let time = &body[9..14];
                        let markers = time[0] & 1 != 0 && time[2] & 1 != 0 && time[4] & 1 != 0;
                        if time[0] >> 4 != 2
                            || markers == (self.kind == PesFaultKind::TimestampMarkers)
                        {
                            return Err(Error::Unsupported(
                                "selected diagnostic additional timestamp fault/prefix",
                            ));
                        }
                        if self.kind != PesFaultKind::ForbiddenTimeFlags {
                            self.pts = Some(crate::pes::timestamp_bits(time));
                        }
                    }
                }
                5 => {
                    let result = self.header_ranges.try_reserve_exact(1);
                    self.check_reservation(inner, result)?;
                }
                6 => self
                    .header_ranges
                    .try_push(SourceRange {
                        entity: inner.scope().entity,
                        offset: self.carrier.source_offset() + self.start as u64,
                        bytes: self.header_len,
                    })
                    .map_err(|failure| failure.error)?,
                7 => {
                    let result = self.spans.try_reserve_exact(1);
                    self.check_reservation(inner, result)?;
                }
                8 => self
                    .spans
                    .try_push(DemuxSpan {
                        bytes: self
                            .carrier
                            .backing_bytes()
                            .slice(self.start + self.header_len..188)?,
                        source: SourceRange {
                            entity: inner.scope().entity,
                            offset: self.carrier.source_offset()
                                + (self.start + self.header_len) as u64,
                            bytes: body.len() - self.header_len,
                        },
                        transport: SourceRange {
                            entity: inner.scope().entity,
                            offset: self.carrier.source_offset(),
                            bytes: 188,
                        },
                        context: self.input,
                    })
                    .map_err(|failure| failure.error)?,
                9 => {
                    self.header = Some(PesFaultHeader {
                        bytes: self
                            .carrier
                            .backing_bytes()
                            .slice(self.start..self.start + self.header_len)?,
                        pts: self.pts,
                        markers_valid: self
                            .pts
                            .map(|_| self.kind != PesFaultKind::TimestampMarkers),
                        time_flags: body[7] >> 6,
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
        let packet = self.finish_source(inner)?;
        let kind = match packet.kind {
            PesFaultKind::TransportError => TransportFaultKind::TransportError,
            PesFaultKind::Scrambling => TransportFaultKind::Scrambling,
            _ => return Err(Error::Invalid("transport fault kind mismatch")),
        };
        let PesFaultCarrier::Transport(carrier) = packet.carrier else {
            return Err(Error::Invalid("transport fault carrier mismatch"));
        };
        Ok(TransportFaultPacket {
            program: packet.program,
            pid: packet.pid,
            source_origin: packet.source_origin,
            source_epochs: packet.source_epochs,
            position: packet.position,
            flags: packet.flags,
            duration90_raw: packet.duration90_raw,
            stream_id: packet.stream_id,
            kind,
            source_fault: packet.source_fault,
            carrier,
            input_context: packet.input_context,
            diagnostic_context: packet.diagnostic_context,
            encoded_header: TransportFaultHeader {
                bytes: packet.encoded_header.bytes,
                pts: packet
                    .encoded_header
                    .pts
                    .ok_or(Error::Invalid("transport fault encoded time missing"))?,
            },
            header_source_ranges: packet.header_source_ranges,
            payload_len: packet.payload_len,
            spans: packet.spans,
        })
    }
    fn finish_source(self, inner: &mut DemuxObserver) -> Result<PesFaultPacket> {
        let header = self
            .header
            .ok_or(Error::Invalid("transport fault header unfinished"))?;
        let (source_fault, diagnostic_context) = self
            .diagnostic
            .ok_or(Error::Invalid("selected source diagnostic missing"))?;
        let scope = inner.scope();
        let offset = self.carrier.source_offset();
        inner.note_recovered(
            self.carrier.backing_bytes().as_ref()[3] & 15,
            offset,
            diagnostic_context.source_event_ordinal,
        );
        Ok(PesFaultPacket {
            program: scope.program,
            pid: scope.pid,
            source_origin: SourceLocation {
                source: scope.entity,
                offset,
            },
            source_epochs: self.input.epochs,
            position: self.position,
            flags: 0,
            duration90_raw: 0,
            stream_id: scope.stream_id,
            kind: self.kind,
            source_fault,
            carrier: self.carrier,
            input_context: self.input,
            diagnostic_context,
            encoded_header: header,
            payload_len: self.spans.as_slice()[0].as_ref().len(),
            header_source_ranges: self.header_ranges,
            spans: self.spans,
        })
    }
}
