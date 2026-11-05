//! Explicit finite AVC diagnostic view, implemented after actual8/28 runtime RED.
use super::*;
use crate::{FaultKind, TraceFault};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSelectedFault {
    PcrEncoding,
    TransportError,
    Scrambling,
    TimestampMarkers,
    ForbiddenTimeFlags,
}
impl NativeSelectedFault {
    pub fn source_error(self) -> Error {
        match self {
            Self::PcrEncoding => Error::Invalid("PCR reserved bits/extension"),
            Self::TransportError => Error::Invalid("TS transport error indicator"),
            Self::Scrambling => Error::Unsupported("scrambled TS payload"),
            Self::TimestampMarkers => Error::Invalid("PES timestamp markers"),
            Self::ForbiddenTimeFlags => Error::Invalid("forbidden PES time flags"),
        }
    }
    pub fn source_layer(self) -> FaultKind {
        match self {
            Self::TimestampMarkers | Self::ForbiddenTimeFlags => FaultKind::Pes,
            _ => FaultKind::Transport,
        }
    }
}
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one charged original input inline; no uncharged Box"
)]
pub enum NativeDiagnosticCarrier {
    Transport(TransportPacket),
    Rejected(RejectedTransport),
}
impl NativeDiagnosticCarrier {
    pub fn backing_bytes(&self) -> &Bytes {
        match self {
            Self::Transport(p) => &p.source,
            Self::Rejected(p) => p.bytes(),
        }
    }
    pub fn source_offset(&self) -> u64 {
        match self {
            Self::Transport(p) => p.source_offset,
            Self::Rejected(p) => p.source_offset(),
        }
    }
}
/// Encoded native fact, not a source PesHeader. Invalid marker bits remain raw.
#[derive(Debug)]
pub struct NativeDiagnosticHeader {
    pub(crate) bytes: Bytes,
    pub(crate) pts: Option<u64>,
    pub(crate) markers_valid: bool,
    pub(crate) time_flags: u8,
}
impl NativeDiagnosticHeader {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }
    pub fn backing_bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn encoded_pts_raw33(&self) -> Option<u64> {
        self.pts
    }
    pub fn markers_valid(&self) -> bool {
        self.markers_valid
    }
    pub fn time_flags(&self) -> u8 {
        self.time_flags
    }
}
#[derive(Debug)]
pub struct NativeDiagnosticPesPacket {
    pub profile: NativeClockProfile,
    pub program: u16,
    pub pid: u16,
    pub source_origin: SourceLocation,
    pub source_epochs: SourceEpochs,
    pub native_position: i64,
    pub native_flags: u32,
    pub native_duration90_raw: i64,
    pub native_stream_id: u8,
    pub selected_fault: NativeSelectedFault,
    pub source_fault: TraceFault,
    pub carrier: NativeDiagnosticCarrier,
    pub input_context: NativeSourceContext,
    pub diagnostic_context: NativeSourceContext,
    pub encoded_header: NativeDiagnosticHeader,
    pub header_source_ranges: BudgetVec<SourceRange>,
    pub payload_len: usize,
    pub spans: BudgetVec<NativePesSpan>,
}
impl NativeDiagnosticPesPacket {
    pub fn payload(&self) -> impl Iterator<Item = &[u8]> {
        self.spans.as_slice().iter().map(AsRef::as_ref)
    }
}

#[derive(Debug)]
pub(super) struct SelectedPending {
    carrier: NativeDiagnosticCarrier,
    kind: NativeSelectedFault,
    input: NativeSourceContext,
    diagnostic: Option<(TraceFault, NativeSourceContext)>,
    start: usize,
    phase: u8,
    header_len: usize,
    pts: Option<u64>,
    header_ranges: BudgetVec<SourceRange>,
    spans: BudgetVec<NativePesSpan>,
}
impl SelectedPending {
    pub(super) fn rejection(&self) -> Option<&RejectedTransport> {
        match &self.carrier {
            NativeDiagnosticCarrier::Rejected(r) => Some(r),
            _ => None,
        }
    }
    fn counter(&self) -> u8 {
        self.carrier.backing_bytes().as_ref()[3] & 15
    }
}

/// Constant bounded hint only; no source header is validated or replaced here.
/// The staged carrier cannot emit until its actual strict diagnostic arrives.
pub(super) fn has_selected_hint(packet: &TransportPacket) -> bool {
    if packet.transport_error || packet.scrambling != 0 {
        return true;
    }
    if !packet.payload_unit_start {
        return false;
    }
    let Some(p) = packet.payload.as_ref().map(AsRef::as_ref) else {
        return false;
    };
    p.len() >= 9
        && p[..4] == [0, 0, 1, 0xe0]
        && (p[7] >> 6 == 1
            || (p[7] >> 6 == 2
                && p.len() >= 14
                && (p[9] >> 4 != 2 || p[9] & 1 == 0 || p[11] & 1 == 0 || p[13] & 1 == 0)))
}

impl NativePesRecoveryObserver {
    /// Explicit d32/default-options finite AVC diagnostic profile, distinct from
    /// the legacy observer. This is not permission to recover other fault kinds.
    pub fn new_selected_avc_faults_v1(
        profile: NativeClockProfile,
        scope: NativePesScope,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        if scope.stream_type != 0x1b || scope.stream_id != 0xe0 {
            return Err(Error::Unsupported(
                "selected diagnostics require explicit AVC carriage",
            ));
        }
        let mut observer = Self::new(
            profile,
            scope,
            NativeRegisteredCarriage::Avc,
            NativeRejectedPolicy::OpcrWithoutPcrV1,
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
        c: NativeSourceContext,
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
        if !self.after_pmt_binding_v1(c)
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
    #[allow(
        clippy::result_large_err,
        reason = "original full charged carrier returned inline"
    )]
    pub(super) fn stage_selected_input(
        &mut self,
        packet: TransportPacket,
        c: NativeSourceContext,
    ) -> std::result::Result<(), PushError<TransportPacket>> {
        let admission = (|| {
            self.selected_context(packet.source_id, packet.source_offset, c, &packet.source)?;
            NativePesObserver::transport_bounds(&packet)?;
            if packet.pid != self.scope().pid
                || packet.prefix.is_some()
                || !packet.payload_unit_start
            {
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
            if start + body.len() != 188 {
                return Err(Error::Invalid("selected diagnostic payload extent"));
            }
            let kind = if packet.transport_error {
                NativeSelectedFault::TransportError
            } else if packet.scrambling != 0 {
                NativeSelectedFault::Scrambling
            } else if body.as_ref().get(7).is_some_and(|b| b >> 6 == 1) {
                NativeSelectedFault::ForbiddenTimeFlags
            } else {
                NativeSelectedFault::TimestampMarkers
            };
            Ok((kind, start))
        })();
        let (kind, start) = match admission {
            Ok(v) => v,
            Err(error) => {
                return Err(PushError {
                    error,
                    value: packet,
                });
            }
        };
        self.last = Some((c.source_event_ordinal, packet.source_offset));
        self.entity_first = false;
        self.pending = Some(RecoveryPending::Selected(SelectedPending {
            carrier: NativeDiagnosticCarrier::Transport(packet),
            kind,
            input: c,
            diagnostic: None,
            start,
            phase: 0,
            header_len: 0,
            pts: None,
            header_ranges: BudgetVec::new(&self.budget),
            spans: BudgetVec::new(&self.budget),
        }));
        Ok(())
    }
    /// Accept the actual strict diagnostic with its complete original rejected
    /// carrier. No TransportPacket, PCR or PesHeader is forged for this path.
    #[allow(
        clippy::result_large_err,
        reason = "recover the original charged source diagnostic inline"
    )]
    pub fn push_diagnostic_rejection_v1(
        &mut self,
        fault: TraceFault,
        c: NativeSourceContext,
    ) -> std::result::Result<(), PushError<TraceFault>> {
        if let Err(error) = self.inner.validate_native_input(None) {
            return Err(PushError {
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
                || fault.error != NativeSelectedFault::PcrEncoding.source_error()
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
            Ok((r.clone(), f.payload_start))
        })();
        let (r, start) = match admission {
            Ok(v) => v,
            Err(error) => {
                return Err(PushError {
                    error,
                    value: fault,
                });
            }
        };
        self.last = Some((c.source_event_ordinal, r.source_offset()));
        self.entity_first = false;
        self.pending = Some(RecoveryPending::Selected(SelectedPending {
            carrier: NativeDiagnosticCarrier::Rejected(r),
            kind: NativeSelectedFault::PcrEncoding,
            input: c,
            diagnostic: Some((fault, c)),
            start,
            phase: 0,
            header_len: 0,
            pts: None,
            header_ranges: BudgetVec::new(&self.budget),
            spans: BudgetVec::new(&self.budget),
        }));
        Ok(())
    }
    /// A wrong/stale diagnostic leaves the one original pending owner intact.
    pub fn confirm_source_fault_v1(
        &mut self,
        fault: &TraceFault,
        c: NativeSourceContext,
    ) -> Result<()> {
        let Some(RecoveryPending::Selected(p)) = self.pending.as_mut() else {
            return Err(Error::Invalid(
                "no selected source fault awaiting diagnostic",
            ));
        };
        if p.diagnostic.is_some()
            || fault.rejected.is_some()
            || fault.kind != p.kind.source_layer()
            || fault.error != p.kind.source_error()
            || fault.pid != Some(self.inner.scope().pid)
            || fault.location.source != self.inner.scope().entity
            || fault.location.offset != p.carrier.source_offset()
            || c.epochs != p.input.epochs
            || c.byte_run_epoch.is_some()
            || c.source_event_ordinal <= p.input.source_event_ordinal
        {
            return Err(Error::Invalid(
                "selected source diagnostic context mismatch",
            ));
        }
        p.diagnostic = Some((fault.clone(), c));
        self.last = Some((c.source_event_ordinal, p.carrier.source_offset()));
        self.entity_first = false;
        Ok(())
    }
    /// Acknowledge only the real next source Timeline after a previously seen
    /// validated PCR/DI boundary has drained. No native clock context is reset.
    pub fn commit_source_timeline_v1(&mut self, c: NativeSourceContext) -> Result<()> {
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
        self.inner.commit_clock_epoch_v1(c.epochs);
        self.pending_timeline = None;
        Ok(())
    }
    /// Each unit validates one fixed <=6-byte field or commits one charged span
    /// action. Pressure retry never repeats the completed syntax phases.
    pub(super) fn poll_selected(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<NativePesObservation>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(AssemblyStep::Yield);
            }
            let Some(RecoveryPending::Selected(p)) = self.pending.as_mut() else {
                return Err(Error::Invalid("selected pending owner missing"));
            };
            if p.diagnostic.is_none() {
                return Ok(AssemblyStep::NeedPayload);
            }
            if !work.try_charge(1) {
                return Ok(AssemblyStep::Yield);
            }
            let raw = p.carrier.backing_bytes().as_ref();
            let body = raw
                .get(p.start..)
                .ok_or(Error::Invalid("selected diagnostic payload offset"))?;
            match p.phase {
                0 => {
                    let s = self.inner.scope();
                    if raw[0] != 0x47
                        || raw[1] & 0x40 == 0
                        || (u16::from(raw[1] & 31) << 8) | u16::from(raw[2]) != s.pid
                        || raw[3] & 0x30 != 0x30
                        || p.start != 5 + usize::from(raw[4])
                        || p.start > 188
                        || p.start < 6
                    {
                        return Err(Error::Unsupported(
                            "selected diagnostic raw envelope unobserved",
                        ));
                    }
                    if self
                        .inner
                        .recovery_counter()
                        .is_some_and(|cc| ((cc + 1) & 15) != (raw[3] & 15))
                    {
                        return Err(Error::Unsupported(
                            "selected diagnostic counter damage unobserved",
                        ));
                    }
                    let tei = raw[1] & 0x80 != 0;
                    let scramble = raw[3] >> 6;
                    let envelope_ok = match p.kind {
                        NativeSelectedFault::PcrEncoding => !tei && scramble == 0 && raw[5] == 16,
                        NativeSelectedFault::TransportError => tei && scramble == 0 && raw[5] == 0,
                        NativeSelectedFault::Scrambling => !tei && scramble != 0 && raw[5] == 0,
                        _ => !tei && scramble == 0 && raw[5] == 0,
                    };
                    if !envelope_ok {
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
                    if body.len() > self.inner.limits().pes_bytes {
                        return Err(crate::limit(body.len(), self.inner.limits().pes_bytes));
                    }
                }
                3 => {
                    let forbidden = p.kind == NativeSelectedFault::ForbiddenTimeFlags;
                    if body[6] != 0x80
                        || body[7] != (if forbidden { 0x40 } else { 0x80 })
                        || (!forbidden && body[8] != 5)
                        || (forbidden && !matches!(body[8], 0 | 5))
                    {
                        return Err(Error::Unsupported(
                            "selected diagnostic additional header fault/context",
                        ));
                    }
                    p.header_len = 9 + usize::from(body[8]);
                    if p.header_len >= body.len() {
                        return Err(Error::Unsupported(
                            "selected diagnostic empty/truncated PES unobserved",
                        ));
                    }
                }
                4 => {
                    if p.header_len == 14 {
                        let t = &body[9..14];
                        let valid = t[0] & 1 != 0 && t[2] & 1 != 0 && t[4] & 1 != 0;
                        if t[0] >> 4 != 2
                            || valid == (p.kind == NativeSelectedFault::TimestampMarkers)
                        {
                            return Err(Error::Unsupported(
                                "selected diagnostic additional timestamp fault/prefix",
                            ));
                        }
                        if p.kind != NativeSelectedFault::ForbiddenTimeFlags {
                            p.pts = Some(
                                (u64::from((t[0] >> 1) & 7) << 30)
                                    | (u64::from(t[1]) << 22)
                                    | (u64::from(t[2] >> 1) << 15)
                                    | (u64::from(t[3]) << 7)
                                    | u64::from(t[4] >> 1),
                            );
                        }
                    }
                }
                5 => p.header_ranges.try_reserve_exact(1)?,
                6 => {
                    p.header_ranges
                        .try_push(SourceRange {
                            entity: self.inner.scope().entity,
                            offset: p.carrier.source_offset() + p.start as u64,
                            bytes: p.header_len,
                        })
                        .map_err(|e| e.error)?;
                }
                7 => p.spans.try_reserve_exact(1)?,
                8 => {
                    p.spans
                        .try_push(NativePesSpan {
                            bytes: p
                                .carrier
                                .backing_bytes()
                                .slice(p.start + p.header_len..188)?,
                            source: SourceRange {
                                entity: self.inner.scope().entity,
                                offset: p.carrier.source_offset() + (p.start + p.header_len) as u64,
                                bytes: body.len() - p.header_len,
                            },
                            transport: SourceRange {
                                entity: self.inner.scope().entity,
                                offset: p.carrier.source_offset(),
                                bytes: 188,
                            },
                            context: p.input,
                        })
                        .map_err(|e| e.error)?;
                }
                9 => {
                    let header = NativeDiagnosticHeader {
                        bytes: p
                            .carrier
                            .backing_bytes()
                            .slice(p.start..p.start + p.header_len)?,
                        pts: p.pts,
                        markers_valid: p.pts.is_some()
                            && p.kind != NativeSelectedFault::TimestampMarkers,
                        time_flags: body[7] >> 6,
                    };
                    let offset = p.carrier.source_offset();
                    let counter = p.counter();
                    let position = self.inner.media_native_position_v1(offset)?;
                    let Some(RecoveryPending::Selected(p)) = self.pending.take() else {
                        unreachable!()
                    };
                    let (fault, diagnostic) = p
                        .diagnostic
                        .ok_or(Error::Invalid("selected source diagnostic missing"))?;
                    self.inner
                        .note_recovered(counter, offset, diagnostic.source_event_ordinal);
                    let scope = self.inner.scope();
                    return Ok(AssemblyStep::Output(
                        NativePesObservation::FromSelectedDiagnostic(NativeDiagnosticPesPacket {
                            profile: self.inner.profile(),
                            program: scope.program,
                            pid: scope.pid,
                            source_origin: SourceLocation {
                                source: scope.entity,
                                offset,
                            },
                            source_epochs: p.input.epochs,
                            native_position: position,
                            native_flags: 0,
                            native_duration90_raw: 0,
                            native_stream_id: 0xe0,
                            selected_fault: p.kind,
                            source_fault: fault,
                            carrier: p.carrier,
                            input_context: p.input,
                            diagnostic_context: diagnostic,
                            encoded_header: header,
                            payload_len: p.spans.as_slice()[0].as_ref().len(),
                            header_source_ranges: p.header_ranges,
                            spans: p.spans,
                        }),
                    ));
                }
                _ => return Err(Error::Invalid("selected diagnostic phase")),
            }
            p.phase += 1;
        }
    }
}
