//! Explicit flags48 PSI compatibility. No Source PatTable, active ProgramMap
//! event, source diagnostic or epoch is synthesized by this optional operation.
use super::*;
use crate::demux_pes::{DemuxObserver, SourceContext};
use crate::psi::{
    Descriptors, PatReader, PmtReader, Section, SectionAssembler, SectionHeader, TableStep,
};
use crate::{AssemblyStep, FaultKind, NullCarrierRun, TraceFault, TransportPacket};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsiCompatibility {
    CurrentPatSectionBeforeWholeTable,
    PmtDescriptorStopsAfterStreamIdentity,
    PmtNonzeroSection,
}
impl PsiCompatibility {
    /// The policy's name in reference traces.
    pub fn label(self) -> &'static str {
        match self {
            Self::CurrentPatSectionBeforeWholeTable => "CurrentPatSectionBeforeWholeTableV1",
            Self::PmtDescriptorStopsAfterStreamIdentity => {
                "PmtDescriptorStopsAfterStreamIdentityV1"
            }
            Self::PmtNonzeroSection => "PmtNonzeroSectionV1",
        }
    }
    fn bit(self) -> u8 {
        match self {
            Self::CurrentPatSectionBeforeWholeTable => 1,
            Self::PmtDescriptorStopsAfterStreamIdentity => 2,
            Self::PmtNonzeroSection => 4,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PsiCompatibilityPolicies(u8);
impl PsiCompatibilityPolicies {
    pub const NONE: Self = Self(0);
    pub const ALL_OBSERVED: Self = Self(7);
    pub fn one(policy: PsiCompatibility) -> Self {
        Self(policy.bit())
    }
    pub fn contains(self, policy: PsiCompatibility) -> bool {
        self.0 & policy.bit() != 0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsiCompatibilitySourceStatus {
    IncompleteCurrentPat {
        section_number: u8,
        last_section_number: u8,
    },
    NotRoutedByStrictIncompletePat {
        pat_source_event_ordinal: u64,
    },
    RejectedPmt {
        kind: FaultKind,
        error: Error,
        location: triergon_core::SourceLocation,
        pid: u16,
        source_event_ordinal: u64,
        epochs: crate::SourceEpochs,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PsiCompatibilityRecord {
    pub evidence_index: usize,
    pub catalogue_evidence_index: usize,
    pub policy: PsiCompatibility,
    pub context: SourceContext,
}
#[derive(Debug)]
pub struct PsiCompatibilityEvidence {
    record: PsiCompatibilityRecord,
    source_status: PsiCompatibilitySourceStatus,
    header: SectionHeader,
    input: SectionInput,
    descriptor_stop: Option<usize>,
}
impl PsiCompatibilityEvidence {
    pub fn record(&self) -> PsiCompatibilityRecord {
        self.record
    }
    pub fn source_status(&self) -> PsiCompatibilitySourceStatus {
        self.source_status
    }
    pub fn wire_header(&self) -> SectionHeader {
        self.header
    }
    pub fn section_bytes(&self) -> &Bytes {
        self.input.section.bytes()
    }
    pub fn source_ranges(&self) -> &[SourceRange] {
        self.input.section.source_ranges()
    }
    pub fn descriptor_stop_offset(&self) -> Option<usize> {
        self.descriptor_stop
    }
    pub fn carrier_count(&self) -> usize {
        self.input.carriers.len()
    }
    pub fn carrier(&self, index: usize) -> Option<(&TransportPacket, SourceContext)> {
        self.input
            .carriers
            .as_slice()
            .get(index)
            .map(|c| (&c.packet, c.context))
    }
}
#[derive(Clone, Debug)]
struct Carrier {
    packet: TransportPacket,
    context: SourceContext,
}
#[derive(Debug)]
struct SectionInput {
    section: Section,
    carriers: BudgetVec<Carrier>,
    context: SourceContext,
}
#[derive(Clone, Copy, Debug)]
struct PartialPatRoute {
    entity: SourceId,
    tsid: u16,
    version: u8,
    last_section: u8,
    program: u16,
    pmt_pid: u16,
    context: SourceContext,
}
#[derive(Debug)]
pub(super) struct CompatibilityState {
    policies: PsiCompatibilityPolicies,
    history: BudgetVec<Carrier>,
    evidence: BudgetVec<PsiCompatibilityEvidence>,
    pending_source: Option<SectionInput>,
    route: Option<PartialPatRoute>,
    pat_identity: Option<(u16, u8, u8)>,
    pat_seen: [u64; 4],
    demux_pmt: Option<SectionAssembler>,
    last_event: Option<(SourceId, SourceContext)>,
}
impl CompatibilityState {
    fn new(budget: &Budget, policies: PsiCompatibilityPolicies) -> Self {
        Self {
            policies,
            history: BudgetVec::new(budget),
            evidence: BudgetVec::new(budget),
            pending_source: None,
            route: None,
            pat_identity: None,
            pat_seen: [0; 4],
            demux_pmt: None,
            last_event: None,
        }
    }
    fn require(&self, policy: PsiCompatibility) -> Result<()> {
        if !self.policies.contains(policy) {
            return Err(Error::Unsupported("PSI compatibility policy not enabled"));
        }
        Ok(())
    }
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
fn common_header(section: &Section, id: u8, budget: &Budget) -> Result<SectionHeader> {
    let b = section.bytes().as_ref();
    if !section.bytes().belongs_to(budget) {
        return Err(Error::ForeignPool);
    }
    if b.len() < 12
        || b.len() > 1024
        || b[0] != id
        || b.len() != 3 + usize::from(u16::from_be_bytes([b[1] & 15, b[2]]))
        || b[1] & 0xf0 != 0xb0
        || b[5] & 0xc0 != 0xc0
        || b[6] > b[7]
        || section.crc_remainder() != Some(0)
        || PsiCrc32::of(b) != 0
    {
        return Err(Error::Invalid(
            "PSI compatibility source section identity/CRC",
        ));
    }
    Ok(SectionHeader {
        table_id: id,
        extension: u16::from_be_bytes([b[3], b[4]]),
        version: (b[5] >> 1) & 31,
        current_next: b[5] & 1 != 0,
        section_number: b[6],
        last_section_number: b[7],
    })
}
fn validate_carrier(
    packet: &TransportPacket,
    context: SourceContext,
    source: SourceId,
    budget: &Budget,
    format: PacketFormat,
) -> Result<()> {
    if !packet.source.belongs_to(budget) {
        return Err(Error::ForeignPool);
    }
    DemuxObserver::transport_bounds(packet)?;
    let full = packet.source.as_ref();
    let prefix = format.prefix_bytes();
    if full.len() != format.bytes()
        || packet.prefix.as_ref().map(Bytes::as_ref) != (prefix != 0).then_some(&full[..prefix])
    {
        return Err(Error::Invalid("PSI compatibility declared carrier format"));
    }
    let raw = &full[prefix..];
    if packet.source_id != Some(source)
        || raw.len() != 188
        || !packet.source_offset.is_multiple_of(format.bytes() as u64)
        || context.byte_run_epoch.is_some()
        || raw[0] != 0x47
        || packet.pid != u16::from_be_bytes([raw[1] & 31, raw[2]])
        || packet.payload_unit_start != (raw[1] & 0x40 != 0)
        || packet.continuity_counter != raw[3] & 15
        || raw[1] & 0x80 != 0
        || packet.transport_error
        || packet.scrambling != 0
        || raw[3] & 0xc0 != 0
    {
        return Err(Error::Invalid("PSI compatibility actual carrier identity"));
    }
    let control = (raw[3] >> 4) & 3;
    if control == 0 {
        return Err(Error::Invalid("PSI compatibility reserved carrier control"));
    }
    let start = if control & 2 != 0 {
        5usize
            .checked_add(usize::from(raw[4]))
            .ok_or(Error::Overflow)?
    } else {
        4
    };
    if start > 188 {
        return Err(Error::Invalid("PSI compatibility carrier payload start"));
    }
    if control & 1 != 0 {
        let body = packet
            .payload
            .as_ref()
            .ok_or(Error::Invalid("PSI compatibility payload missing"))?;
        if packet.payload_source_offset
            != packet
                .source_offset
                .checked_add((prefix + start) as u64)
                .ok_or(Error::Overflow)?
            || body.as_ref() != &raw[start..]
        {
            return Err(Error::Invalid(
                "PSI compatibility original payload mismatch",
            ));
        }
    } else if packet.payload.is_some() {
        return Err(Error::Invalid("PSI compatibility unexpected payload"));
    }
    Ok(())
}
impl SectionInput {
    fn capture(
        section: &Section,
        context: SourceContext,
        history: &[Carrier],
        demux_only: bool,
        budget: &Budget,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        cancelled(cancel)?;
        if context.byte_run_epoch.is_some()
            || section.source_ranges().is_empty()
            || section.source_ranges().len() > 1024
            || section.bytes().len() > 1024
        {
            return Err(Error::Unsupported("PSI compatibility source range shape"));
        }
        let mut carriers = BudgetVec::new(budget);
        let mut cursor = 0usize;
        let mut bytes = 0usize;
        let mut previous_end = None;
        let mut copied_index = None;
        for r in section.source_ranges() {
            cancelled(cancel)?;
            let end = r.end()?;
            if r.bytes == 0 || previous_end.is_some_and(|old| old > r.offset) {
                return Err(Error::Invalid("PSI compatibility nonphysical range order"));
            }
            previous_end = Some(end);
            while let Some(c) = history.get(cursor) {
                cancelled(cancel)?;
                if c.packet
                    .source_offset
                    .checked_add(c.packet.source.len() as u64)
                    .ok_or(Error::Overflow)?
                    <= r.offset
                {
                    cursor += 1;
                } else {
                    break;
                }
            }
            let c = history.get(cursor).ok_or(Error::Invalid(
                "PSI compatibility actual carrier unavailable",
            ))?;
            if c.packet.pid != section.pid()
                || c.packet.source_id != Some(r.entity)
                || c.context.epochs != context.epochs
                || c.context.source_event_ordinal > context.source_event_ordinal
            {
                return Err(Error::Invalid(
                    "PSI compatibility section/carrier context mismatch",
                ));
            }
            let payload = c.packet.payload.as_ref().ok_or(Error::Invalid(
                "PSI compatibility carrier has no section payload",
            ))?;
            let payload_end = c
                .packet
                .payload_source_offset
                .checked_add(payload.len() as u64)
                .ok_or(Error::Overflow)?;
            if r.offset < c.packet.payload_source_offset || end > payload_end {
                return Err(Error::Invalid(
                    "PSI compatibility range outside actual payload",
                ));
            }
            let begin = usize::try_from(r.offset - c.packet.payload_source_offset)
                .map_err(|_| Error::Overflow)?;
            let output_end = bytes.checked_add(r.bytes).ok_or(Error::Overflow)?;
            if section.bytes().as_ref().get(bytes..output_end)
                != payload.as_ref().get(begin..begin + r.bytes)
            {
                return Err(Error::Invalid(
                    "PSI compatibility literal section bytes differ",
                ));
            }
            bytes = output_end;
            if copied_index != Some(cursor) {
                carriers.try_push(c.clone()).map_err(|e| e.error)?;
                copied_index = Some(cursor);
            }
        }
        if bytes != section.bytes().len() {
            return Err(Error::Invalid("PSI compatibility exact range accounting"));
        }
        let last = carriers
            .as_slice()
            .last()
            .ok_or(Error::Invalid("PSI compatibility carrier proof empty"))?;
        if (demux_only && last.context.source_event_ordinal != context.source_event_ordinal)
            || (!demux_only && last.context.source_event_ordinal >= context.source_event_ordinal)
        {
            return Err(Error::Invalid(
                "PSI compatibility source ordinal provenance",
            ));
        }
        Ok(Self {
            section: section.try_clone_charged(budget)?,
            carriers,
            context,
        })
    }
}
struct DecodedPmt {
    header: SectionHeader,
    pcr_pid: u16,
    streams: BudgetVec<Stream>,
    descriptor_stop: Option<usize>,
}
fn decode_rejected_pmt(
    input: &SectionInput,
    policy: PsiCompatibility,
    budget: &Budget,
    limits: ParseLimits,
    cancel: &AtomicBool,
) -> Result<DecodedPmt> {
    let h = common_header(&input.section, 2, budget)?;
    if !h.current_next || h.extension == 0 {
        return Err(Error::Unsupported(
            "PSI compatibility requires current nonzero PMT",
        ));
    }
    let raw = input.section.bytes().as_ref();
    if raw.len() < 16
        || raw[8] & 0xe0 != 0xe0
        || u16::from_be_bytes([raw[10], raw[11]]) & 0xfc00 != 0xf000
    {
        return Err(Error::Invalid("PSI compatibility PMT header bounds"));
    }
    if raw[10] & 3 != 0 || raw[11] != 0 {
        return Err(Error::Unsupported(
            "PSI compatibility program descriptors unobserved",
        ));
    }
    match policy {
        PsiCompatibility::PmtNonzeroSection
            if h.section_number != 0 || h.last_section_number != 0 => {}
        PsiCompatibility::PmtDescriptorStopsAfterStreamIdentity
            if h.section_number == 0 && h.last_section_number == 0 => {}
        _ => {
            return Err(Error::Invalid(
                "PSI compatibility policy/wire section mismatch",
            ));
        }
    }
    let mut streams = BudgetVec::new(budget);
    let mut at = 12usize;
    let mut stop = None;
    let mut descriptors = 0usize;
    while at < raw.len() - 4 {
        cancelled(cancel)?;
        let head = raw
            .get(at..at + 5)
            .ok_or(Error::Invalid("PSI compatibility ES header truncated"))?;
        if head[1] & 0xe0 != 0xe0 || u16::from_be_bytes([head[3], head[4]]) & 0xfc00 != 0xf000 {
            return Err(Error::Invalid("PSI compatibility ES reserved bits"));
        }
        let pid = u16::from_be_bytes([head[1], head[2]]) & 0x1fff;
        let end = at
            .checked_add(5)
            .and_then(|n| {
                n.checked_add(usize::from(u16::from_be_bytes([head[3], head[4]]) & 0x03ff))
            })
            .ok_or(Error::Overflow)?;
        if end > raw.len() - 4 || pid < 0x10 || pid == 0x1fff || !matches!(head[0], 0x1b | 0x24) {
            return Err(Error::Unsupported(
                "PSI compatibility ES identity/extent outside observed profile",
            ));
        }
        if streams.as_slice().iter().any(|s: &Stream| s.pid == pid)
            || streams.len() >= limits.streams
        {
            return Err(Error::Invalid("PSI compatibility duplicate/excess ES"));
        }
        let desc = &raw[at + 5..end];
        let mut language = BudgetVec::new(budget);
        let mut walked = 0usize;
        for item in Descriptors::new(desc) {
            cancelled(cancel)?;
            descriptors = descriptors.checked_add(1).ok_or(Error::Overflow)?;
            if descriptors > limits.descriptors {
                return Err(Error::Unsupported("PSI compatibility descriptor limit"));
            }
            let d = match item {
                Ok(d) => d,
                Err(Error::Invalid("descriptor length"))
                    if policy == PsiCompatibility::PmtDescriptorStopsAfterStreamIdentity
                        && stop.is_none() =>
                {
                    stop = Some(at + 5 + walked);
                    if end != raw.len() - 4 {
                        return Err(Error::Unsupported(
                            "PSI compatibility streams after malformed descriptor unobserved",
                        ));
                    }
                    break;
                }
                Err(e) => return Err(e),
            };
            if !matches!(d.tag, 0x0a | 0xee) {
                return Err(Error::Unsupported(
                    "PSI compatibility descriptor interpretation unobserved",
                ));
            }
            if d.tag == 0x0a && language.is_empty() {
                for entry in d.languages()? {
                    cancelled(cancel)?;
                    if entry.language_code.contains(&0) {
                        return Err(Error::Unsupported(
                            "PSI compatibility embedded NUL language",
                        ));
                    }
                    if !language.is_empty() {
                        language.try_push(b',').map_err(|e| e.error)?;
                    }
                    language.try_extend_from_slice(&entry.language_code)?;
                }
            }
            walked = walked.checked_add(d.source.len()).ok_or(Error::Overflow)?;
        }
        streams
            .try_push(Stream {
                pid,
                stream_type: head[0],
                codec: if head[0] == 0x1b {
                    CatalogueCodec::Avc
                } else {
                    CatalogueCodec::Hevc
                },
                language,
                evidence: 0,
            })
            .map_err(|e| e.error)?;
        at = end;
    }
    if at != raw.len() - 4
        || (policy == PsiCompatibility::PmtDescriptorStopsAfterStreamIdentity && stop.is_none())
    {
        return Err(Error::Invalid(
            "PSI compatibility actual descriptor rejection not present",
        ));
    }
    Ok(DecodedPmt {
        header: h,
        pcr_pid: u16::from_be_bytes([raw[8], raw[9]]) & 0x1fff,
        streams,
        descriptor_stop: stop,
    })
}
impl Catalogue {
    /// Opt-in only. Existing observe()/finish() retain their strict-source rules.
    pub fn enable_psi_compatibility(&mut self, policies: PsiCompatibilityPolicies) -> Result<()> {
        if self.compatibility.is_some()
            || self.entity.is_some()
            || !self.evidence.is_empty()
            || !self.programs.is_empty()
            || !self.streams.is_empty()
            || self.ended
            || self.fault.is_some()
        {
            return Err(Error::Invalid("PSI compatibility enable after observation"));
        }
        if policies == PsiCompatibilityPolicies::NONE {
            return Err(Error::Invalid(
                "PSI compatibility requires explicit policies",
            ));
        }
        self.compatibility = Some(CompatibilityState::new(&self.budget, policies));
        Ok(())
    }
    pub fn psi_compatibility_evidence(&self) -> &[PsiCompatibilityEvidence] {
        self.compatibility
            .as_ref()
            .map_or(&[], |s| s.evidence.as_slice())
    }
    pub fn has_psi_compatibility_route(&self, pid: u16) -> bool {
        self.compatibility
            .as_ref()
            .and_then(|s| s.route)
            .is_some_and(|route| {
                route.pmt_pid == pid
                    && route.last_section != 0
                    && self.pending_pat == Some((route.tsid, route.version))
            })
    }
    /// Includes bounded storage/provenance scans and possible pending PSI bytes.
    pub fn required_psi_compatibility_work(&self, event: &TraceEvent) -> Result<u64> {
        let s = self
            .compatibility
            .as_ref()
            .ok_or(Error::Invalid("PSI compatibility not enabled"))?;
        let pending = s.demux_pmt.as_ref().map_or(0, |a| a.partial_bytes().len());
        let n = match event {
            TraceEvent::Transport(p) => p.source.len(),
            TraceEvent::Section(s) => s.bytes().len(),
            TraceEvent::ProgramMap { map, .. } => map.source.len(),
            _ => 1024,
        };
        let units = n
            .checked_add(pending)
            .and_then(|v| v.checked_mul(256))
            .and_then(|v| v.checked_add(s.history.len().checked_mul(32)?))
            .and_then(|v| v.checked_add(8192))
            .ok_or(Error::Overflow)?;
        u64::try_from(units).map_err(|_| Error::Overflow)
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "keeps source status, wire header and native observation context distinct"
    )]
    fn store_compat_evidence(
        &mut self,
        s: &mut CompatibilityState,
        input: SectionInput,
        policy: PsiCompatibility,
        status: PsiCompatibilitySourceStatus,
        header: SectionHeader,
        descriptor_stop: Option<usize>,
        catalogue_index: usize,
        context: SourceContext,
    ) -> Result<PsiCompatibilityRecord> {
        if s.evidence.len() >= self.limits.source_spans {
            return Err(Error::Unsupported("PSI compatibility evidence limit"));
        }
        if let Some(previous) = s.evidence.as_slice().last() {
            let old_offset = previous.input.section.source_ranges()[0].offset;
            let new_offset = input.section.source_ranges()[0].offset;
            if (context.source_event_ordinal, new_offset)
                <= (previous.record.context.source_event_ordinal, old_offset)
            {
                return Err(Error::Invalid(
                    "PSI compatibility duplicate or reversed admission",
                ));
            }
        }
        let record = PsiCompatibilityRecord {
            evidence_index: s.evidence.len(),
            catalogue_evidence_index: catalogue_index,
            policy,
            context,
        };
        s.evidence
            .try_push(PsiCompatibilityEvidence {
                record,
                source_status: status,
                header,
                input,
                descriptor_stop,
            })
            .map_err(|e| e.error)?;
        Ok(record)
    }
    fn commit_current_pat_section(
        &mut self,
        s: &mut CompatibilityState,
        section: &Section,
        context: SourceContext,
        inner: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<PsiCompatibilityRecord> {
        let policy = PsiCompatibility::CurrentPatSectionBeforeWholeTable;
        s.require(policy)?;
        let h = common_header(section, 0, &self.budget)?;
        if section.pid() != 0 || !h.current_next || h.last_section_number == 0 {
            return Err(Error::Invalid(
                "PSI compatibility requires actual incomplete current PAT section",
            ));
        }
        let input = SectionInput::capture(
            section,
            context,
            s.history.as_slice(),
            false,
            &self.budget,
            cancel,
        )?;
        let mut reader = PatReader::new(section, &self.budget, self.limits)?;
        let decoded = match reader.poll(inner, cancel)? {
            TableStep::Output(t) => t,
            TableStep::Yield => return Err(Error::Invalid("reserved PSI PAT work exhausted")),
            TableStep::Done => {
                return Err(Error::Invalid("PSI compatibility PAT result missing"));
            }
        };
        if let Some(identity) = s.pat_identity
            && identity != (h.extension, h.version, h.last_section_number)
        {
            return Err(Error::Unsupported(
                "PSI compatibility partial PAT identity transition unobserved",
            ));
        }
        self.pending_pat = Some((h.extension, h.version));
        let index = self.retain(
            std::slice::from_ref(section.bytes()),
            section.source_ranges(),
        )?;
        for entry in decoded.programs.as_slice() {
            cancelled(cancel)?;
            if entry.is_network_pid() {
                continue;
            }
            if let Some(old) = self
                .programs
                .as_mut_slice()
                .iter_mut()
                .find(|p| p.program == entry.program_number)
            {
                if old.pmt_pid != entry.pid {
                    return Err(Error::Unsupported(
                        "PSI compatibility PAT route replacement unobserved",
                    ));
                }
                old.evidence = index;
            } else {
                if self.programs.len() >= self.limits.programs {
                    return Err(Error::Unsupported("PSI compatibility program limit"));
                }
                self.programs
                    .try_push(Program {
                        program: entry.program_number,
                        pmt_pid: entry.pid,
                        pcr_pid: 0,
                        evidence: index,
                    })
                    .map_err(|e| e.error)?;
            }
            if entry.program_number == self.program {
                let entity = section.source_ranges()[0].entity;
                s.route = Some(PartialPatRoute {
                    entity,
                    tsid: h.extension,
                    version: h.version,
                    last_section: h.last_section_number,
                    program: self.program,
                    pmt_pid: entry.pid,
                    context,
                });
                if s.demux_pmt.is_none() {
                    s.demux_pmt = Some(SectionAssembler::new(
                        entry.pid,
                        self.budget.clone(),
                        self.limits,
                    )?);
                }
            }
        }
        s.pat_identity = Some((h.extension, h.version, h.last_section_number));
        s.pat_seen[usize::from(h.section_number) / 64] |= 1u64 << (h.section_number % 64);
        // Actual Strict incompleteness is intentionally retained in pending_pat.
        self.store_compat_evidence(
            s,
            input,
            policy,
            PsiCompatibilitySourceStatus::IncompleteCurrentPat {
                section_number: h.section_number,
                last_section_number: h.last_section_number,
            },
            h,
            None,
            index,
            context,
        )
    }
    fn commit_rejected_pmt(
        &mut self,
        s: &mut CompatibilityState,
        input: &SectionInput,
        diagnostic: &TraceFault,
        context: SourceContext,
        policy: PsiCompatibility,
        cancel: &AtomicBool,
    ) -> Result<PsiCompatibilityRecord> {
        s.require(policy)?;
        let expected = match policy {
            PsiCompatibility::PmtDescriptorStopsAfterStreamIdentity => {
                Error::Invalid("descriptor length")
            }
            PsiCompatibility::PmtNonzeroSection => {
                Error::Invalid("PMT single-section program identity")
            }
            PsiCompatibility::CurrentPatSectionBeforeWholeTable => {
                return Err(Error::Invalid(
                    "PSI compatibility rejection policy is not PMT",
                ));
            }
        };
        let last = input.carriers.as_slice().last().ok_or(Error::Invalid(
            "PSI compatibility rejection carrier missing",
        ))?;
        if diagnostic.kind != FaultKind::Psi
            || diagnostic.error != expected
            || diagnostic.rejected.is_some()
            || diagnostic.pid != Some(input.section.pid())
            || diagnostic.location.source != last.packet.source_id.ok_or(Error::SourceMismatch)?
            || diagnostic.location.offset != last.packet.source_offset
            || context.epochs != input.context.epochs
            || context.byte_run_epoch.is_some()
            || context.source_event_ordinal
                != input
                    .context
                    .source_event_ordinal
                    .checked_add(1)
                    .ok_or(Error::Overflow)?
        {
            return Err(Error::Invalid(
                "PSI compatibility actual Strict rejection mismatch",
            ));
        }
        let parsed = decode_rejected_pmt(input, policy, &self.budget, self.limits, cancel)?;
        if parsed.header.extension != self.program
            || self
                .programs
                .as_slice()
                .iter()
                .all(|p| p.program != self.program || p.pmt_pid != input.section.pid())
        {
            return Err(Error::Invalid(
                "PSI compatibility rejected PMT has no actual PAT route",
            ));
        }
        let kept = SectionInput::capture(
            &input.section,
            input.context,
            s.history.as_slice(),
            false,
            &self.budget,
            cancel,
        )?;
        let index = self.retain(
            std::slice::from_ref(input.section.bytes()),
            input.section.source_ranges(),
        )?;
        for decoded in parsed.streams.as_slice() {
            cancelled(cancel)?;
            let stream = (decoded.pid, decoded.stream_type);
            let mut language = BudgetVec::new(&self.budget);
            language.try_extend_from_slice(decoded.language.as_slice())?;
            if let Some(old) = self
                .streams
                .as_mut_slice()
                .iter_mut()
                .find(|p| p.pid == stream.0)
            {
                old.stream_type = stream.1;
                old.codec = decoded.codec;
                old.language = language;
                old.evidence = index;
            } else {
                if self.streams.len() >= self.limits.streams {
                    return Err(Error::Unsupported("PSI compatibility stream limit"));
                }
                self.streams
                    .try_push(Stream {
                        pid: stream.0,
                        stream_type: stream.1,
                        codec: decoded.codec,
                        language,
                        evidence: index,
                    })
                    .map_err(|e| e.error)?;
            }
        }
        let selected_program = self.program;
        self.programs
            .as_mut_slice()
            .iter_mut()
            .find(|p| p.program == selected_program)
            .ok_or(Error::Invalid("PSI compatibility program disappeared"))?
            .pcr_pid = parsed.pcr_pid;
        if self.pending_pmt != Some((parsed.header.extension, parsed.header.version)) {
            return Err(Error::Invalid(
                "PSI compatibility source PMT pending identity mismatch",
            ));
        }
        // This resolves only the optional demux catalogue. The actual Strict
        // error stays in source_status and the caller's unchanged source trace.
        self.pending_pmt = None;
        self.store_compat_evidence(
            s,
            kept,
            policy,
            PsiCompatibilitySourceStatus::RejectedPmt {
                kind: diagnostic.kind,
                error: diagnostic.error,
                location: diagnostic.location,
                pid: input.section.pid(),
                source_event_ordinal: context.source_event_ordinal,
                epochs: context.epochs,
            },
            parsed.header,
            parsed.descriptor_stop,
            index,
            context,
        )
    }
    fn commit_demux_routed_pmt(
        &mut self,
        s: &mut CompatibilityState,
        section: &Section,
        context: SourceContext,
        inner: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let route = s
            .route
            .ok_or(Error::Invalid("PSI compatibility native route missing"))?;
        if section.pid() != route.pmt_pid
            || context.source_event_ordinal <= route.context.source_event_ordinal
            || context.epochs != route.context.epochs
        {
            return Err(Error::Invalid(
                "PSI compatibility native route context mismatch",
            ));
        }
        let input = SectionInput::capture(
            section,
            context,
            s.history.as_slice(),
            true,
            &self.budget,
            cancel,
        )?;
        if input
            .section
            .source_ranges()
            .iter()
            .any(|r| r.entity != route.entity)
        {
            return Err(Error::SourceMismatch);
        }
        let mut reader = PmtReader::new(section, &self.budget, self.limits)?;
        let map = match reader.poll(inner, cancel)? {
            TableStep::Output(m) => m,
            TableStep::Yield => return Err(Error::Invalid("reserved PSI PMT work exhausted")),
            TableStep::Done => {
                return Err(Error::Invalid(
                    "PSI compatibility routed PMT result missing",
                ));
            }
        };
        if map.header.extension != route.program || !map.header.current_next {
            return Err(Error::Invalid(
                "PSI compatibility routed PMT program/current mismatch",
            ));
        }
        let header = map.header;
        self.pmt(&map, cancel)?;
        // The PMT's actual storage is the most recent retained evidence record.
        let metadata_index = self
            .evidence
            .len()
            .checked_sub(1)
            .ok_or(Error::Invalid("PSI compatibility PMT evidence missing"))?;
        self.store_compat_evidence(
            s,
            input,
            PsiCompatibility::CurrentPatSectionBeforeWholeTable,
            PsiCompatibilitySourceStatus::NotRoutedByStrictIncompletePat {
                pat_source_event_ordinal: route.context.source_event_ordinal,
            },
            header,
            None,
            metadata_index,
            context,
        )?;
        Ok(())
    }
    fn observe_psi_inner(
        &mut self,
        s: &mut CompatibilityState,
        event: &TraceEvent,
        source: SourceId,
        context: SourceContext,
        inner: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if context.byte_run_epoch.is_some()
            || s.last_event.is_some_and(|(old_source, old)| {
                old_source != source || old.source_event_ordinal >= context.source_event_ordinal
            })
        {
            return Err(Error::Invalid(
                "PSI compatibility source event order/entity",
            ));
        }
        match event {
            TraceEvent::Transport(packet) => {
                validate_carrier(packet, context, source, &self.budget, self.format)?;
                let is_psi = packet.pid == 0
                    || self
                        .programs
                        .as_slice()
                        .iter()
                        .any(|p| p.program == self.program && p.pmt_pid == packet.pid);
                if !is_psi {
                    self.observe_inner(event, cancel)?;
                    s.last_event = Some((source, context));
                    return Ok(());
                }
                if s.history.len() >= self.limits.source_spans
                    || s.history
                        .as_slice()
                        .last()
                        .is_some_and(|c| c.packet.source_offset >= packet.source_offset)
                {
                    return Err(Error::Unsupported(
                        "PSI compatibility carrier history bound/order",
                    ));
                }
                s.history
                    .try_push(Carrier {
                        packet: packet.clone(),
                        context,
                    })
                    .map_err(|e| e.error)?;
                if let Some(route) = s.route.filter(|route| {
                    route.pmt_pid == packet.pid
                        && self.pending_pat == Some((route.tsid, route.version))
                }) {
                    if route.entity != source {
                        return Err(Error::SourceMismatch);
                    }
                    if let Some(body) = packet.payload.as_ref() {
                        s.demux_pmt
                            .as_mut()
                            .ok_or(Error::Invalid("PSI compatibility assembler missing"))?
                            .push(
                                body.clone(),
                                packet.payload_unit_start,
                                source,
                                packet.payload_source_offset,
                            )
                            .map_err(|e| e.error)?;
                        loop {
                            let step = s
                                .demux_pmt
                                .as_mut()
                                .ok_or(Error::Invalid("PSI compatibility assembler lost"))?
                                .poll(inner, cancel)?;
                            match step {
                                AssemblyStep::Output(section) => self
                                    .commit_demux_routed_pmt(s, &section, context, inner, cancel)?,
                                AssemblyStep::NeedPayload => break,
                                AssemblyStep::Yield => {
                                    return Err(Error::Invalid(
                                        "reserved PSI section work exhausted",
                                    ));
                                }
                                AssemblyStep::Finished => {
                                    return Err(Error::Invalid(
                                        "PSI compatibility assembler finished before end",
                                    ));
                                }
                            }
                        }
                    }
                }
                self.observe_inner(event, cancel)?;
            }
            TraceEvent::Section(section) => {
                let prior_pending_pat = self.pending_pat;
                self.observe_inner(event, cancel)?;
                let raw = section.bytes().as_ref();
                // A repeated complete, byte-identical PAT may have no second
                // Strict PatTable event. Only the explicit selected view uses
                // the previously validated complete table as that proof. A
                // different body/version or an already pending table never
                // takes this path; physical source provenance is still checked.
                if self.selected_carriage
                    && prior_pending_pat.is_none()
                    && section.pid() == 0
                    && raw.len() >= 12
                    && raw[0] == 0
                    && raw[5] & 1 != 0
                    && raw[6] == 0
                    && raw[7] == 0
                    && section.crc_remainder() == Some(0)
                    && self
                        .programs
                        .as_slice()
                        .iter()
                        .find(|p| p.program == self.program)
                        .and_then(|p| self.evidence.as_slice().get(p.evidence))
                        .is_some_and(|e| {
                            e.bytes.len() == 1 && e.bytes.as_slice()[0].as_ref() == raw
                        })
                {
                    let checked = SectionInput::capture(
                        section,
                        context,
                        s.history.as_slice(),
                        false,
                        &self.budget,
                        cancel,
                    )?;
                    self.retain(
                        std::slice::from_ref(checked.section.bytes()),
                        checked.section.source_ranges(),
                    )?;
                    self.pending_pat = None;
                }
                if section.pid() == 0
                    && raw.len() >= 12
                    && raw[0] == 0
                    && raw[5] & 1 != 0
                    && raw[7] != 0
                    && section.crc_remainder() == Some(0)
                {
                    self.commit_current_pat_section(s, section, context, inner, cancel)?;
                } else if raw.len() >= 12
                    && raw[0] == 2
                    && raw[5] & 1 != 0
                    && section.crc_remainder() == Some(0)
                {
                    if s.pending_source.is_some() {
                        return Err(Error::Invalid("PSI compatibility undrained source section"));
                    }
                    s.pending_source = Some(SectionInput::capture(
                        section,
                        context,
                        s.history.as_slice(),
                        false,
                        &self.budget,
                        cancel,
                    )?);
                }
            }
            TraceEvent::Diagnostic(fault)
                if s.pending_source.is_some() && fault.kind == FaultKind::Psi =>
            {
                let policy = match fault.error {
                    Error::Invalid("descriptor length") => {
                        PsiCompatibility::PmtDescriptorStopsAfterStreamIdentity
                    }
                    Error::Invalid("PMT single-section program identity") => {
                        PsiCompatibility::PmtNonzeroSection
                    }
                    _ => {
                        return Err(Error::Unsupported(
                            "PSI compatibility different actual source rejection",
                        ));
                    }
                };
                let pending = s
                    .pending_source
                    .take()
                    .ok_or(Error::Invalid("PSI compatibility source pending lost"))?;
                if let Err(e) =
                    self.commit_rejected_pmt(s, &pending, fault, context, policy, cancel)
                {
                    s.pending_source = Some(pending);
                    return Err(e);
                }
            }
            TraceEvent::ProgramMap { map, .. } => {
                if let Some(pending) = s.pending_source.as_ref()
                    && pending.section.bytes().as_ref() != map.source.as_ref()
                {
                    return Err(Error::Invalid(
                        "PSI compatibility actual map/section mismatch",
                    ));
                }
                self.observe_inner(event, cancel)?;
                s.pending_source = None;
            }
            _ => self.observe_inner(event, cancel)?,
        }
        s.last_event = Some((source, context));
        Ok(())
    }
    /// Named opt-in event entry point. None of this runs through observe(). A
    /// false return means the exact input event is still unconsumed.
    pub fn observe_psi_compatibility(
        &mut self,
        event: &TraceEvent,
        source: SourceId,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        cancelled(cancel)?;
        if let Some(e) = self.fault {
            return Err(e);
        }
        if self.ended {
            return Err(Error::Invalid("PSI compatibility after logical end"));
        }
        let cost = self.required_psi_compatibility_work(event)?;
        if !work.try_charge(cost) {
            return Ok(false);
        }
        let mut state = self
            .compatibility
            .take()
            .ok_or(Error::Invalid("PSI compatibility not enabled"))?;
        let result = self.observe_psi_inner(
            &mut state,
            event,
            source,
            context,
            &mut WorkBudget::new(cost),
            cancel,
        );
        self.compatibility = Some(state);
        if let Err(e) = result {
            self.fault = Some(e);
            return Err(e);
        }
        Ok(true)
    }
    /// Advance the literal event order over clear, unflagged null carriers.
    pub fn observe_psi_null_run(
        &mut self,
        run: &NullCarrierRun,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        cancelled(cancel)?;
        if let Some(error) = self.fault {
            return Err(error);
        }
        if self.ended {
            return Err(Error::Invalid("PSI compatibility after logical end"));
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        let result = (|| {
            let state = self
                .compatibility
                .as_mut()
                .ok_or(Error::Invalid("PSI compatibility not enabled"))?;
            let source = run.start().source;
            if !run.bytes().belongs_to(&self.budget) {
                return Err(Error::ForeignPool);
            }
            if run.format() != self.format
                || context.byte_run_epoch.is_some()
                || state.last_event.is_some_and(|(old_source, old)| {
                    old_source != source || old.source_event_ordinal >= context.source_event_ordinal
                })
                || self.programs.as_slice().iter().any(|p| p.pmt_pid == 0x1fff)
            {
                return Err(Error::Invalid(
                    "PSI compatibility null run format/route/order",
                ));
            }
            let source_event_ordinal = context
                .source_event_ordinal
                .checked_add((run.len() - 1) as u64)
                .ok_or(Error::Overflow)?;
            state.last_event = Some((
                source,
                SourceContext {
                    source_event_ordinal,
                    ..context
                },
            ));
            Ok(())
        })();
        if let Err(error) = result {
            self.fault = Some(error);
            return Err(error);
        }
        Ok(true)
    }
    /// Direct named source-section API; incompleteness has no fabricated error.
    pub fn observe_current_pat_section(
        &mut self,
        section: &Section,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<PsiCompatibilityRecord>> {
        cancelled(cancel)?;
        if let Some(e) = self.fault {
            return Err(e);
        }
        if self.ended {
            return Err(Error::Invalid("PSI compatibility after logical end"));
        }
        let cost = self.required_psi_compatibility_work(&TraceEvent::Finished)?;
        if !work.try_charge(cost) {
            return Ok(None);
        }
        let mut state = self
            .compatibility
            .take()
            .ok_or(Error::Invalid("PSI compatibility not enabled"))?;
        let result = self.commit_current_pat_section(
            &mut state,
            section,
            context,
            &mut WorkBudget::new(cost),
            cancel,
        );
        self.compatibility = Some(state);
        match result {
            Ok(record) => Ok(Some(record)),
            Err(e) => {
                self.fault = Some(e);
                Err(e)
            }
        }
    }
    /// The actual source Section and actual later Strict diagnostic are both
    /// required. section_context never pretends the diagnostic happened earlier.
    #[allow(
        clippy::too_many_arguments,
        reason = "explicit independent source and diagnostic contexts are required"
    )]
    pub fn observe_rejected_psi(
        &mut self,
        section: &Section,
        section_context: SourceContext,
        diagnostic: &TraceFault,
        context: SourceContext,
        policy: PsiCompatibility,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<PsiCompatibilityRecord>> {
        cancelled(cancel)?;
        if let Some(e) = self.fault {
            return Err(e);
        }
        if self.ended {
            return Err(Error::Invalid("PSI compatibility after logical end"));
        }
        let cost = self.required_psi_compatibility_work(&TraceEvent::Finished)?;
        if !work.try_charge(cost) {
            return Ok(None);
        }
        let mut state = self
            .compatibility
            .take()
            .ok_or(Error::Invalid("PSI compatibility not enabled"))?;
        let result = (|| {
            let input = SectionInput::capture(
                section,
                section_context,
                state.history.as_slice(),
                false,
                &self.budget,
                cancel,
            )?;
            let record =
                self.commit_rejected_pmt(&mut state, &input, diagnostic, context, policy, cancel)?;
            if state.pending_source.as_ref().is_some_and(|pending| {
                pending.context == section_context
                    && pending.section.bytes().as_ref() == section.bytes().as_ref()
            }) {
                state.pending_source = None;
            }
            Ok(record)
        })();
        self.compatibility = Some(state);
        match result {
            Ok(record) => Ok(Some(record)),
            Err(e) => {
                self.fault = Some(e);
                Err(e)
            }
        }
    }
    pub fn required_psi_compatibility_finish_work(&self) -> Result<u64> {
        let state = self
            .compatibility
            .as_ref()
            .ok_or(Error::Invalid("PSI compatibility not enabled"))?;
        u64::try_from(
            state
                .history
                .len()
                .checked_add(self.retained_ranges)
                .and_then(|n| n.checked_add(state.evidence.len()))
                .and_then(|n| n.checked_mul(32))
                .and_then(|n| n.checked_add(8192))
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)
    }
    /// Real EOF is separately witnessed. It does not make a pending Strict PAT
    /// complete; the explicit demux per-section coverage is retained instead.
    pub fn finish_psi_compatibility(
        &mut self,
        end: &MediaEntityEnd,
        input: &InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        cancelled(cancel)?;
        self.validate_terminal_witness(end, input)?;
        let cost = self.required_psi_compatibility_finish_work()?;
        if !work.try_charge(cost) {
            return Ok(false);
        }
        let mut state = self
            .compatibility
            .take()
            .ok_or(Error::Invalid("PSI compatibility not enabled"))?;
        let result = (|| {
            if state.pending_source.is_some() || self.pending_pmt.is_some() {
                return Err(Error::Invalid(
                    "PSI compatibility unconfirmed source PMT at end",
                ));
            }
            if let Some((tsid, version)) = self.pending_pat
                && (!state
                    .policies
                    .contains(PsiCompatibility::CurrentPatSectionBeforeWholeTable)
                    || state
                        .pat_identity
                        .is_none_or(|identity| identity.0 != tsid || identity.1 != version)
                    || state.pat_seen.iter().all(|v| *v == 0))
            {
                return Err(Error::Unsupported(
                    "PSI compatibility pending PAT has no actual section coverage",
                ));
            }
            if let Some(assembler) = state.demux_pmt.as_mut() {
                if !assembler.partial_bytes().is_empty() {
                    return Err(Error::Unsupported(
                        "PSI compatibility truncated native-routed PMT",
                    ));
                }
                assembler.end_of_source();
                match assembler.poll(&mut WorkBudget::new(cost), cancel)? {
                    AssemblyStep::Finished => {}
                    _ => {
                        return Err(Error::Invalid(
                            "PSI compatibility native section input not drained",
                        ));
                    }
                }
            }
            for record in state.evidence.as_slice() {
                cancelled(cancel)?;
                for carrier in record.input.carriers.as_slice() {
                    if carrier.packet.source_id != Some(end.location.source)
                        || carrier
                            .packet
                            .source_offset
                            .checked_add(carrier.packet.source.len() as u64)
                            .ok_or(Error::Overflow)?
                            > end.location.offset
                    {
                        return Err(Error::Invalid(
                            "PSI compatibility provenance beyond actual end",
                        ));
                    }
                }
            }
            Ok(())
        })();
        self.compatibility = Some(state);
        match result {
            Ok(()) => {
                self.ended = true;
                Ok(true)
            }
            Err(e) => {
                self.fault = Some(e);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod guards {
    use super::*;
    use crate::{SourceEpochs, TransportStream};
    use triergon_core::{InputWindow, SourceCapabilities, Step};
    const BAD_DESC: &[u8] = include_bytes!("../../tests/data/pmt-descriptor-overrun.ts");
    const BAD_NUMBER: &[u8] = include_bytes!("../../tests/data/pmt-multi-section-forbidden.ts");
    const PARTIAL_PAT: &[u8] = include_bytes!("../../tests/data/pat-incomplete-version.ts");
    struct Item {
        event: TraceEvent,
        context: SourceContext,
    }
    struct SourceRun {
        items: Vec<Item>,
        end: MediaEntityEnd,
        input: InputCursor,
    }
    fn source(data: &[u8], budget: &Budget) -> SourceRun {
        let mut input = InputCursor::new(
            budget,
            SourceId(1),
            SourceCapabilities {
                length: Some(data.len() as u64),
                random_access: false,
            },
            4096,
        )
        .unwrap();
        let mut stream = TransportStream::new(
            budget.clone(),
            PacketFormat::Ts188,
            ParseLimits::default(),
            1,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut items = Vec::new();
        let mut end = None;
        for _ in 0..500_000 {
            match stream
                .poll(&mut input, &mut WorkBudget::new(17), &cancel)
                .unwrap()
            {
                Step::Ready(event) => {
                    let context = SourceContext {
                        source_event_ordinal: items.len() as u64,
                        epochs: stream.source_epochs(),
                        byte_run_epoch: None,
                    };
                    let done = matches!(event, TraceEvent::Finished);
                    items.push(Item { event, context });
                    if done {
                        return SourceRun {
                            items,
                            end: end.unwrap(),
                            input,
                        };
                    }
                }
                Step::Yield => {}
                Step::NeedInput(r) => {
                    let at = usize::try_from(r.offset).unwrap();
                    if at == data.len() {
                        input.finish(r.source, r.token).unwrap();
                    } else {
                        let limit = data.len().min(at + r.maximum.min(7));
                        input
                            .provide(
                                &InputWindow::new(
                                    r.source,
                                    r.token,
                                    r.offset,
                                    Bytes::copy_from_slice(budget, &data[at..limit]).unwrap(),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Step::End(_) => {
                    end = stream
                        .media_entity_end(&mut input, &mut WorkBudget::new(1), &cancel)
                        .unwrap();
                    stream.end_of_source().unwrap();
                }
            }
        }
        panic!("source guard fixture did not terminate")
    }
    fn view(budget: &Budget, policies: PsiCompatibilityPolicies) -> Catalogue {
        let mut v = Catalogue::new(budget.clone(), ParseLimits::default(), 1).unwrap();
        v.enable_psi_compatibility(policies).unwrap();
        v
    }
    fn feed(v: &mut Catalogue, item: &Item) -> Result<bool> {
        let cost = v.required_psi_compatibility_work(&item.event)?;
        v.observe_psi_compatibility(
            &item.event,
            SourceId(1),
            item.context,
            &mut WorkBudget::new(cost),
            &AtomicBool::new(false),
        )
    }
    fn before_rejection(v: &mut Catalogue, run: &SourceRun) -> usize {
        let index = run
            .items
            .iter()
            .position(|i| matches!(&i.event, TraceEvent::Diagnostic(f) if f.kind == FaultKind::Psi))
            .unwrap();
        for item in &run.items[..index] {
            assert!(feed(v, item).unwrap());
        }
        index
    }
    #[test]
    fn no_policy_cannot_activate_the_operation() {
        let b = Budget::new(1024 * 1024).unwrap();
        let mut v = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
        assert!(
            v.enable_psi_compatibility(PsiCompatibilityPolicies::NONE)
                .is_err()
        );
    }
    #[test]
    fn policies_are_explicit_and_independent() {
        let only = PsiCompatibilityPolicies::one(PsiCompatibility::PmtNonzeroSection);
        assert!(only.contains(PsiCompatibility::PmtNonzeroSection));
        assert!(!only.contains(PsiCompatibility::PmtDescriptorStopsAfterStreamIdentity));
        assert!(!only.contains(PsiCompatibility::CurrentPatSectionBeforeWholeTable));
    }
    #[test]
    fn default_observe_still_refuses_the_rejected_pmt() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
        let mut error = None;
        for item in &run.items {
            if let Err(e) = v.observe(
                &item.event,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false),
            ) {
                error = Some(e);
                break;
            }
        }
        assert_eq!(
            error,
            Some(Error::Unsupported(
                "rejected PMT requires explicit compatibility review"
            ))
        );
    }
    #[test]
    fn enabling_does_not_change_the_default_entry_point() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let mut error = None;
        for item in &run.items {
            if let Err(e) = v.observe(
                &item.event,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false),
            ) {
                error = Some(e);
                break;
            }
        }
        assert!(error.is_some());
        assert!(v.psi_compatibility_evidence().is_empty());
    }
    #[test]
    fn zero_work_does_not_consume_a_source_event() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let item = &run.items[0];
        assert!(
            !v.observe_psi_compatibility(
                &item.event,
                SourceId(1),
                item.context,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
        assert!(v.compatibility.as_ref().unwrap().history.is_empty());
        assert!(feed(&mut v, item).unwrap());
    }
    #[test]
    fn cancelled_event_does_not_consume_source_or_allocate_evidence() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let item = &run.items[0];
        assert_eq!(
            v.observe_psi_compatibility(
                &item.event,
                SourceId(1),
                item.context,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(true)
            ),
            Err(Error::Cancelled)
        );
        assert!(v.compatibility.as_ref().unwrap().history.is_empty());
        assert!(v.psi_compatibility_evidence().is_empty());
        assert!(feed(&mut v, item).unwrap());
    }
    #[test]
    fn foreign_pool_actual_carrier_is_refused() {
        let a = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &a);
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        assert_eq!(feed(&mut v, &run.items[0]), Err(Error::ForeignPool));
        assert!(v.fault().is_some());
    }
    #[test]
    fn repeated_source_ordinal_is_sticky() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        assert!(feed(&mut v, &run.items[0]).unwrap());
        assert!(feed(&mut v, &run.items[0]).is_err());
        assert!(v.fault().is_some());
    }
    #[test]
    fn changed_entity_cannot_reuse_a_source_context() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let item = &run.items[0];
        assert!(
            v.observe_psi_compatibility(
                &item.event,
                SourceId(2),
                item.context,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn actual_raw_carrier_cannot_have_its_decoded_pid_replaced() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let TraceEvent::Transport(original) = &run.items[0].event else {
            panic!("carrier fixture")
        };
        let mut changed = original.clone();
        changed.pid = 4096;
        let event = TraceEvent::Transport(changed);
        assert!(
            v.observe_psi_compatibility(
                &event,
                SourceId(1),
                run.items[0].context,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn a_different_real_source_error_is_not_the_enabled_policy() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let index = before_rejection(&mut v, &run);
        let TraceEvent::Diagnostic(f) = &run.items[index].event else {
            panic!("diagnostic")
        };
        let mut different = f.clone();
        different.error = Error::Invalid("descriptor header");
        let event = TraceEvent::Diagnostic(different);
        assert!(
            v.observe_psi_compatibility(
                &event,
                SourceId(1),
                run.items[index].context,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(v.psi_compatibility_evidence().is_empty());
    }
    #[test]
    fn a_rejection_from_another_carrier_is_not_accepted() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let index = before_rejection(&mut v, &run);
        let TraceEvent::Diagnostic(f) = &run.items[index].event else {
            panic!("diagnostic")
        };
        let mut changed = f.clone();
        changed.location.offset = 0;
        let event = TraceEvent::Diagnostic(changed);
        assert!(
            v.observe_psi_compatibility(
                &event,
                SourceId(1),
                run.items[index].context,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn epoch_change_does_not_relabel_the_rejected_source_section() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let index = before_rejection(&mut v, &run);
        let mut changed = run.items[index].context;
        changed.epochs = SourceEpochs {
            clock: 9,
            configuration: changed.epochs.configuration,
        };
        assert!(
            v.observe_psi_compatibility(
                &run.items[index].event,
                SourceId(1),
                changed,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn future_diagnostic_ordinal_is_not_an_actual_consecutive_rejection() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let index = before_rejection(&mut v, &run);
        let mut changed = run.items[index].context;
        changed.source_event_ordinal += 10;
        assert!(
            v.observe_psi_compatibility(
                &run.items[index].event,
                SourceId(1),
                changed,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
    }
    #[test]
    fn unsupported_source_policy_remains_a_refusal() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(
            &b,
            PsiCompatibilityPolicies::one(PsiCompatibility::PmtNonzeroSection),
        );
        let index = before_rejection(&mut v, &run);
        assert!(feed(&mut v, &run.items[index]).is_err());
        assert!(v.psi_compatibility_evidence().is_empty());
    }
    #[test]
    fn a_source_section_without_its_real_transports_cannot_be_admitted() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let item = run
            .items
            .iter()
            .find(|i| matches!(i.event, TraceEvent::Section(_)))
            .unwrap();
        assert!(feed(&mut v, item).is_err());
        assert!(v.programs().is_empty());
    }
    #[test]
    fn partial_pat_retains_source_incompleteness_and_real_demux_metadata() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        for item in &run.items {
            if !matches!(item.event, TraceEvent::Finished) {
                assert!(feed(&mut v, item).unwrap());
            }
        }
        assert!(v.has_pending_current_pat());
        assert_eq!(v.programs()[0].pcr_pid, 256);
        assert_eq!(v.streams()[0].pid, 256);
        assert!(v.psi_compatibility_evidence().iter().any(|e| matches!(
            e.source_status(),
            PsiCompatibilitySourceStatus::IncompleteCurrentPat {
                last_section_number: 1,
                ..
            }
        )));
        let cost = v.required_psi_compatibility_finish_work().unwrap();
        assert!(
            v.finish_psi_compatibility(
                &run.end,
                &run.input,
                &mut WorkBudget::new(cost),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
        assert!(
            v.has_pending_current_pat(),
            "native EOF cannot turn a partial source table into a complete source table"
        );
    }
    #[test]
    fn original_nonzero_section_numbers_are_not_rewritten() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_NUMBER, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        for item in &run.items {
            if !matches!(item.event, TraceEvent::Finished) {
                assert!(feed(&mut v, item).unwrap());
            }
        }
        let e = &v.psi_compatibility_evidence()[0];
        assert_eq!(
            (
                e.wire_header().section_number,
                e.wire_header().last_section_number
            ),
            (1, 1)
        );
        assert!(matches!(
            e.source_status(),
            PsiCompatibilitySourceStatus::RejectedPmt {
                error: Error::Invalid("PMT single-section program identity"),
                ..
            }
        ));
    }
    #[test]
    fn original_descriptor_tail_and_actual_error_location_are_retained() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        for item in &run.items {
            if !matches!(item.event, TraceEvent::Finished) {
                assert!(feed(&mut v, item).unwrap());
            }
        }
        let e = &v.psi_compatibility_evidence()[0];
        assert_eq!(e.descriptor_stop_offset(), Some(17));
        assert_eq!(
            &e.section_bytes().as_ref()[17..23],
            &[0x0a, 8, b'e', b'n', b'g', 0]
        );
        assert!(matches!(
            e.source_status(),
            PsiCompatibilitySourceStatus::RejectedPmt {
                location: triergon_core::SourceLocation { offset: 188, .. },
                ..
            }
        ));
    }
    #[test]
    fn end_with_unconfirmed_actual_source_diagnostic_is_not_eof() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        let _ = before_rejection(&mut v, &run);
        let cost = v.required_psi_compatibility_finish_work().unwrap();
        assert!(
            v.finish_psi_compatibility(
                &run.end,
                &run.input,
                &mut WorkBudget::new(cost),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(!v.is_ended());
    }
    #[test]
    fn cancelled_end_cannot_publish_a_successful_object() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        for item in &run.items {
            if !matches!(item.event, TraceEvent::Finished) {
                assert!(feed(&mut v, item).unwrap());
            }
        }
        assert_eq!(
            v.finish_psi_compatibility(
                &run.end,
                &run.input,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(true)
            ),
            Err(Error::Cancelled)
        );
        assert!(!v.is_ended());
    }
    #[test]
    fn wrong_core_cursor_cannot_supply_the_demux_terminal_witness() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        for item in &run.items {
            if !matches!(item.event, TraceEvent::Finished) {
                assert!(feed(&mut v, item).unwrap());
            }
        }
        let wrong = InputCursor::new(
            &b,
            SourceId(2),
            SourceCapabilities {
                length: Some(376),
                random_access: false,
            },
            4096,
        )
        .unwrap();
        assert!(
            v.finish_psi_compatibility(
                &run.end,
                &wrong,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(!v.is_ended());
    }
    #[test]
    fn exhausted_retained_history_is_explicit_and_releases_its_backings() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(BAD_DESC, &b);
        let before = b.used();
        let limits = ParseLimits {
            source_spans: 1,
            ..ParseLimits::default()
        };
        let mut v = Catalogue::new(b.clone(), limits, 1).unwrap();
        v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL_OBSERVED)
            .unwrap();
        let mut rejected = false;
        for item in &run.items {
            if feed(&mut v, item).is_err() {
                rejected = true;
                break;
            }
        }
        assert!(rejected);
        assert!(!v.is_ended());
        drop(v);
        assert_eq!(b.used(), before);
    }
    #[test]
    fn invalid_crc_never_activates_a_compatibility_fact() {
        let mut bytes = BAD_DESC.to_vec();
        *bytes.last_mut().unwrap() ^= 1;
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(&bytes, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        for item in &run.items {
            if !matches!(item.event, TraceEvent::Finished) {
                assert!(feed(&mut v, item).unwrap());
            }
        }
        assert!(v.psi_compatibility_evidence().is_empty());
        assert!(v.streams().is_empty());
        assert!(run.items.iter().any(|item| matches!(&item.event, TraceEvent::Diagnostic(f) if f.error == Error::Invalid("PSI table identity/CRC"))));
    }
    #[test]
    fn zero_terminal_work_keeps_the_witness_retryable() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &b);
        let mut v = view(&b, PsiCompatibilityPolicies::ALL_OBSERVED);
        for item in &run.items {
            if !matches!(item.event, TraceEvent::Finished) {
                assert!(feed(&mut v, item).unwrap());
            }
        }
        assert!(
            !v.finish_psi_compatibility(
                &run.end,
                &run.input,
                &mut WorkBudget::new(0),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
        assert!(!v.is_ended());
        let cost = v.required_psi_compatibility_finish_work().unwrap();
        assert!(
            v.finish_psi_compatibility(
                &run.end,
                &run.input,
                &mut WorkBudget::new(cost),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
    }
    #[test]
    fn compatibility_cannot_be_enabled_after_a_source_fact() {
        let b = Budget::new(8 * 1024 * 1024).unwrap();
        let run = source(PARTIAL_PAT, &b);
        let mut v = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
        let section = run
            .items
            .iter()
            .find(|item| matches!(item.event, TraceEvent::Section(_)))
            .unwrap();
        assert!(
            v.observe(
                &section.event,
                &mut WorkBudget::new(1_000_000),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
        assert!(
            v.enable_psi_compatibility(PsiCompatibilityPolicies::ALL_OBSERVED)
                .is_err()
        );
    }
}
