//! Bounded native catalogue view of actual strict PAT/PMT events.
//!
//! H.222.0 2.4.4/2.6 remain the source grammar. The separate d32/flags48
//! compatibility view supplies native stream order, absent-PCR zero and language
//! tags. It does not activate incomplete PATs or rejected PMTs.
use crate::psi::{PatTable, ProgramMap, PsiCrc32};
use crate::{MediaEntityEndV1, PacketFormat, ParseLimits, SourceRange, TraceEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, InputCursor, Result, SourceId, WorkBudget};

mod psi_compatibility;
pub use psi_compatibility::{
    NativePsiCompatibilityEvidenceV1, NativePsiCompatibilityPoliciesV1,
    NativePsiCompatibilityReceiptV1, NativePsiCompatibilitySourceStatusV1,
    NativePsiCompatibilityV1,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeProgramV1 {
    pub program: u16,
    pub pmt_pid: u16,
    /// Native AVProgram default when no valid PMT was observed; not a source PCR.
    pub native_pcr_pid: u16,
    pub evidence: usize,
}
#[derive(Debug)]
pub struct NativeStreamV1 {
    pub pid: u16,
    pub stream_type: u8,
    pub language: BudgetVec<u8>,
    pub evidence: usize,
    native_codec: NativeCatalogueCodecV2,
}
impl NativeStreamV1 {
    pub fn native_codec_v2(&self) -> NativeCatalogueCodecV2 {
        self.native_codec
    }
}
/// Pinned native catalogue identity, never a source namespace or codec configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCatalogueCodecV2 {
    Avc,
    Hevc,
    AacAdts,
    AacLatm,
    Ac3,
    Eac3,
}
impl NativeCatalogueCodecV2 {
    pub fn name(self) -> &'static str {
        match self {
            Self::Avc => "h264",
            Self::Hevc => "hevc",
            Self::AacAdts => "aac",
            Self::AacLatm => "aac_latm",
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
        }
    }
    pub fn media_type(self) -> i32 {
        match self {
            Self::Avc | Self::Hevc => 0,
            _ => 1,
        }
    }
}
#[derive(Debug)]
pub struct NativeCatalogueEvidenceV1 {
    pub bytes: BudgetVec<Bytes>,
    pub ranges: BudgetVec<SourceRange>,
}
#[derive(Debug)]
pub struct NativeCatalogueV1 {
    budget: Budget,
    limits: ParseLimits,
    program: u16,
    entity: Option<SourceId>,
    programs: BudgetVec<NativeProgramV1>,
    streams: BudgetVec<NativeStreamV1>,
    evidence: BudgetVec<NativeCatalogueEvidenceV1>,
    pending_pat: Option<(u16, u8)>,
    pending_pmt: Option<(u16, u8)>,
    retained_ranges: usize,
    ended: bool,
    fault: Option<Error>,
    compatibility: Option<psi_compatibility::CompatibilityStateV1>,
    format: PacketFormat,
    selected_carriage_v2: bool,
}
impl NativeCatalogueV1 {
    pub fn new(budget: Budget, limits: ParseLimits, program: u16) -> Result<Self> {
        limits.validate()?;
        if limits.programs > 64 || limits.streams > 64 || limits.source_spans > 4096 {
            return Err(Error::Unsupported(
                "native catalogue bounded profile limits",
            ));
        }
        if program == 0 {
            return Err(Error::Invalid("native catalogue program zero"));
        }
        Ok(Self {
            programs: BudgetVec::new(&budget),
            streams: BudgetVec::new(&budget),
            evidence: BudgetVec::new(&budget),
            budget,
            limits,
            program,
            entity: None,
            pending_pat: None,
            pending_pmt: None,
            retained_ranges: 0,
            ended: false,
            fault: None,
            compatibility: None,
            format: PacketFormat::Ts188,
            selected_carriage_v2: false,
        })
    }
    /// Explicit TS188/M2TS192 audio/video native view. The default
    /// constructor remains the TS188/video profile; source semantics are untouched.
    pub fn new_selected_carriage_v2(
        budget: Budget,
        limits: ParseLimits,
        program: u16,
        format: PacketFormat,
    ) -> Result<Self> {
        let mut value = Self::new(budget, limits, program)?;
        value.format = format;
        value.selected_carriage_v2 = true;
        Ok(value)
    }
    fn native_codec(
        &self,
        kind: u8,
        program: &[u8],
        elementary: &[u8],
    ) -> Result<NativeCatalogueCodecV2> {
        match kind {
            0x1b => Ok(NativeCatalogueCodecV2::Avc),
            0x24 => Ok(NativeCatalogueCodecV2::Hevc),
            _ if !self.selected_carriage_v2 => Err(Error::Unsupported(
                "native catalogue selected video types only",
            )),
            0x0f => Ok(NativeCatalogueCodecV2::AacAdts),
            0x11 => Ok(NativeCatalogueCodecV2::AacLatm),
            6 | 0x81 | 0x87 => {
                use crate::native_pes::{
                    NativePrivateAudioCarriage as C, NativePrivateAudioDescriptors,
                };
                let observed = C::from_observed_pmt(
                    kind,
                    NativePrivateAudioDescriptors {
                        program,
                        elementary,
                    },
                    self.limits,
                )?;
                Ok(match observed {
                    C::DvbAc3Tag | C::PrivateType81Ac3 => NativeCatalogueCodecV2::Ac3,
                    C::DvbEac3Tag | C::PrivateType87Eac3 => NativeCatalogueCodecV2::Eac3,
                })
            }
            _ => Err(Error::Unsupported(
                "native catalogue selected carriage unobserved",
            )),
        }
    }
    pub fn programs(&self) -> &[NativeProgramV1] {
        self.programs.as_slice()
    }
    pub fn streams(&self) -> &[NativeStreamV1] {
        self.streams.as_slice()
    }
    pub fn evidence(&self) -> &[NativeCatalogueEvidenceV1] {
        self.evidence.as_slice()
    }
    /// Current raw PAT section seen, but no complete strict table yet.
    pub fn has_pending_current_pat(&self) -> bool {
        self.pending_pat.is_some()
    }
    pub fn is_ended(&self) -> bool {
        self.ended
    }
    pub fn fault(&self) -> Option<Error> {
        self.fault
    }
    /// Atomic work grant, explicitly costed by actual bounded syntax bytes. A
    /// false return leaves state untouched; callers may supply required_work().
    pub fn required_work(event: &TraceEvent) -> Result<u64> {
        if matches!(event,TraceEvent::Pat(t) if t.source_sections.len()>256 || t.source_ranges.len()>4096 || t.programs.len()>64)
        {
            return Err(Error::Unsupported("native catalogue PAT shape bound"));
        }
        let bytes = match event {
            TraceEvent::Pat(t) => t
                .source_sections
                .as_slice()
                .iter()
                .try_fold(0usize, |n, b| n.checked_add(b.len()).ok_or(Error::Overflow))?,
            TraceEvent::ProgramMap { map, .. } => map.source.len(),
            TraceEvent::Section(s) => s.bytes().len(),
            _ => 1,
        };
        u64::try_from(
            bytes
                .checked_mul(8)
                .and_then(|v| v.checked_add(1))
                .ok_or(Error::Overflow)?,
        )
        .map_err(|_| Error::Overflow)
    }
    pub fn observe(
        &mut self,
        event: &TraceEvent,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Some(e) = self.fault {
            return Err(e);
        }
        if self.ended {
            return Err(Error::Invalid("native catalogue after logical EOF"));
        }
        if !work.try_charge(Self::required_work(event)?) {
            return Ok(false);
        }
        let result = self.observe_inner(event, cancel);
        if let Err(e) = result {
            self.fault = Some(e);
            return Err(e);
        }
        Ok(true)
    }
    fn source(&mut self, ranges: &[SourceRange]) -> Result<()> {
        for r in ranges {
            r.end()?;
            if r.bytes == 0 || self.entity.is_some_and(|s| s != r.entity) {
                return Err(Error::SourceMismatch);
            }
            self.entity = Some(r.entity);
        }
        Ok(())
    }
    fn retain(&mut self, owners: &[Bytes], ranges: &[SourceRange]) -> Result<usize> {
        if self.evidence.len() >= self.limits.source_spans {
            return Err(Error::Unsupported("native catalogue evidence limit"));
        }
        let retained = self
            .retained_ranges
            .checked_add(ranges.len())
            .ok_or(Error::Overflow)?;
        if retained > self.limits.source_spans {
            return Err(Error::Unsupported(
                "native catalogue retained source-range limit",
            ));
        }
        let owner_bytes = owners
            .iter()
            .try_fold(0usize, |n, b| n.checked_add(b.len()).ok_or(Error::Overflow))?;
        let range_bytes = ranges
            .iter()
            .try_fold(0usize, |n, r| n.checked_add(r.bytes).ok_or(Error::Overflow))?;
        if owner_bytes != range_bytes || (owner_bytes != 0 && ranges.is_empty()) {
            return Err(Error::Invalid("native catalogue source accounting"));
        }
        let mut bytes = BudgetVec::new(&self.budget);
        let mut kept = BudgetVec::new(&self.budget);
        for b in owners {
            if !b.belongs_to(&self.budget) {
                return Err(Error::ForeignPool);
            }
            bytes.try_push(b.clone()).map_err(|e| e.error)?;
        }
        kept.try_extend_from_slice(ranges)?;
        self.source(ranges)?;
        let index = self.evidence.len();
        self.evidence
            .try_push(NativeCatalogueEvidenceV1 {
                bytes,
                ranges: kept,
            })
            .map_err(|e| e.error)?;
        self.retained_ranges = retained;
        Ok(index)
    }
    fn observe_inner(&mut self, event: &TraceEvent, cancel: &AtomicBool) -> Result<()> {
        match event {
            TraceEvent::Section(s) => {
                if !s.bytes().belongs_to(&self.budget) {
                    return Err(Error::ForeignPool);
                }
                self.source(s.source_ranges())?;
                let b = s.bytes().as_ref();
                if s.crc_remainder() == Some(0) && b.len() >= 12 && b[5] & 1 != 0 {
                    let identity = (u16::from_be_bytes([b[3], b[4]]), (b[5] >> 1) & 31);
                    if s.pid() == 0 && b[0] == 0 {
                        self.pending_pat = Some(identity);
                    }
                    if b[0] == 2 {
                        self.pending_pmt = Some(identity);
                    }
                }
            }
            TraceEvent::Pat(t) => self.pat(t, cancel)?,
            TraceEvent::ProgramMap {
                map, active: true, ..
            } => self.pmt(map, cancel)?,
            TraceEvent::Diagnostic(f)
                if f.kind == crate::FaultKind::Psi
                    && matches!(
                        f.error,
                        Error::Invalid("descriptor length" | "PMT single-section program identity")
                    ) =>
            {
                return Err(Error::Unsupported(
                    "rejected PMT requires explicit compatibility review",
                ));
            }
            TraceEvent::Discontinuity { .. } => {
                return Err(Error::Unsupported(
                    "native catalogue is one physical entity",
                ));
            }
            _ => {}
        }
        Ok(())
    }
    fn pat(&mut self, table: &PatTable, cancel: &AtomicBool) -> Result<()> {
        let h = table.header;
        if h.table_id != 0
            || !h.current_next
            || table.source_sections.len() != usize::from(h.last_section_number) + 1
            || table.programs.len() > self.limits.programs
        {
            return Err(Error::Invalid("native catalogue incomplete source PAT"));
        }
        let mut position = 0;
        let mut previous = None;
        for (section, b) in table.source_sections.as_slice().iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !b.belongs_to(&self.budget) {
                return Err(Error::ForeignPool);
            }
            let raw = b.as_ref();
            if raw.len() < 12
                || raw.len() > 1024
                || (raw.len() - 12) % 4 != 0
                || raw.len() != 3 + usize::from(u16::from_be_bytes([raw[1] & 15, raw[2]]))
                || raw[1] & 0xf0 != 0xb0
                || raw[5] & 0xc0 != 0xc0
                || raw[0] != 0
                || raw[6] != section as u8
                || raw[7] != h.last_section_number
                || u16::from_be_bytes([raw[3], raw[4]]) != h.extension
                || (raw[5] >> 1) & 31 != h.version
                || raw[5] & 1 == 0
                || PsiCrc32::of(raw) != 0
            {
                return Err(Error::Invalid("native catalogue PAT source identity"));
            }
            let first = table
                .source_ranges
                .as_slice()
                .iter()
                .find(|r| usize::from(r.section_number) == section)
                .ok_or(Error::Invalid("native catalogue PAT source range"))?
                .range;
            if previous.is_some_and(|old| old >= first.offset) {
                return Err(Error::Unsupported(
                    "native PAT section arrival order outside bounded catalogue",
                ));
            }
            previous = Some(first.offset);
            for entry in raw[8..raw.len() - 4].chunks_exact(4) {
                let expected = table
                    .programs
                    .as_slice()
                    .get(position)
                    .ok_or(Error::Invalid("native catalogue PAT entries"))?;
                if expected.program_number != u16::from_be_bytes([entry[0], entry[1]])
                    || expected.pid != u16::from_be_bytes([entry[2], entry[3]]) & 0x1fff
                {
                    return Err(Error::Invalid(
                        "native catalogue PAT encoded entry mismatch",
                    ));
                }
                if entry[2] & 0xe0 != 0xe0 || !(0x10..=0x1ffe).contains(&expected.pid) {
                    return Err(Error::Invalid("native catalogue PAT encoded PID"));
                }
                position += 1;
            }
        }
        if position != table.programs.len() {
            return Err(Error::Invalid("native catalogue PAT entry count"));
        }
        let mut ranges = BudgetVec::new(&self.budget);
        for r in table.source_ranges.as_slice() {
            ranges.try_push(r.range).map_err(|e| e.error)?;
        }
        let evidence = self.retain(table.source_sections.as_slice(), ranges.as_slice())?;
        for e in table.programs.as_slice() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if e.is_network_pid() {
                continue;
            }
            if let Some(p) = self
                .programs
                .as_mut_slice()
                .iter_mut()
                .find(|p| p.program == e.program_number)
            {
                p.pmt_pid = e.pid;
                p.evidence = evidence;
            } else {
                if self.programs.len() >= self.limits.programs {
                    return Err(Error::Unsupported("native catalogue program limit"));
                }
                self.programs
                    .try_push(NativeProgramV1 {
                        program: e.program_number,
                        pmt_pid: e.pid,
                        native_pcr_pid: 0,
                        evidence,
                    })
                    .map_err(|e| e.error)?;
            }
        }
        if self.pending_pat == Some((h.extension, h.version)) {
            self.pending_pat = None;
        }
        Ok(())
    }
    fn pmt(&mut self, map: &ProgramMap, cancel: &AtomicBool) -> Result<()> {
        let h = map.header;
        let raw = map.source.as_ref();
        if !map.source.belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        if h.table_id != 2
            || !h.current_next
            || h.section_number != 0
            || h.last_section_number != 0
            || raw.len() < 16
            || raw.len() > 1024
            || raw.len() != 3 + usize::from(u16::from_be_bytes([raw[1] & 15, raw[2]]))
            || raw[1] & 0xf0 != 0xb0
            || raw[5] & 0xc0 != 0xc0
            || raw[8] & 0xe0 != 0xe0
            || raw[10] & 0xf0 != 0xf0
            || raw[0] != 2
            || PsiCrc32::of(raw) != 0
            || u16::from_be_bytes([raw[3], raw[4]]) != h.extension
            || (raw[5] >> 1) & 31 != h.version
            || raw[5] & 1 == 0
            || raw[6] != 0
            || raw[7] != 0
        {
            return Err(Error::Invalid("native catalogue PMT source identity"));
        }
        if map.streams.len() > self.limits.streams
            || self
                .programs
                .as_slice()
                .iter()
                .all(|p| p.program != h.extension)
        {
            return Err(Error::Invalid("native catalogue PMT has no PAT route"));
        }
        if map.pcr_pid != u16::from_be_bytes([raw[8], raw[9]]) & 0x1fff {
            return Err(Error::Invalid("native catalogue PCR mismatch"));
        }
        let start = 12 + (usize::from(u16::from_be_bytes([raw[10], raw[11]]) & 0x0fff));
        if start > raw.len() - 4 || map.program_descriptors.as_ref() != &raw[12..start] {
            return Err(Error::Invalid("native catalogue program descriptors"));
        }
        if !map.program_descriptors.is_empty() {
            return Err(Error::Unsupported(
                "native catalogue program descriptor interpretation unobserved",
            ));
        }
        let mut at = start;
        for stream in map.streams.as_slice() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let encoded = raw
                .get(at..at + 5)
                .ok_or(Error::Invalid("native catalogue stream extent"))?;
            let end = at + 5 + usize::from(u16::from_be_bytes([encoded[3], encoded[4]]) & 0x0fff);
            if end > raw.len() - 4
                || stream.stream_type != encoded[0]
                || stream.pid != u16::from_be_bytes([encoded[1], encoded[2]]) & 0x1fff
                || stream.descriptor_bytes.as_ref() != &raw[at + 5..end]
            {
                return Err(Error::Invalid("native catalogue encoded stream mismatch"));
            }
            if !matches!(stream.stream_type, 0x1b | 0x24)
                && !(self.selected_carriage_v2
                    && matches!(stream.stream_type, 0x0f | 0x11 | 6 | 0x81 | 0x87))
            {
                return Err(Error::Unsupported(
                    "native catalogue selected video types only",
                ));
            }
            at = end;
        }
        if at != raw.len() - 4 {
            return Err(Error::Invalid("native catalogue stream count"));
        }
        let evidence = self.retain(
            std::slice::from_ref(&map.source),
            map.source_ranges.as_slice(),
        )?;
        for stream in map.streams.as_slice() {
            let codec = self.native_codec(
                stream.stream_type,
                map.program_descriptors.as_ref(),
                stream.descriptor_bytes.as_ref(),
            )?;
            let mut language = BudgetVec::new(&self.budget);
            for descriptor in stream.descriptors() {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let descriptor = descriptor?;
                if !matches!(descriptor.tag, 0x0a | 0xee)
                    && !(self.selected_carriage_v2
                        && matches!(
                            codec,
                            NativeCatalogueCodecV2::Ac3 | NativeCatalogueCodecV2::Eac3
                        )
                        && matches!(descriptor.tag, 0x6a | 0x7a))
                {
                    return Err(Error::Unsupported(
                        "native catalogue descriptor outside observed video profile",
                    ));
                }
                if descriptor.tag == 0x0a && language.is_empty() {
                    for e in descriptor.languages()? {
                        if e.language_code.contains(&0) {
                            return Err(Error::Unsupported(
                                "native catalogue embedded NUL language",
                            ));
                        }
                        if !language.is_empty() {
                            language.try_push(b',').map_err(|e| e.error)?;
                        }
                        language.try_extend_from_slice(&e.language_code)?;
                    }
                }
            }
            if let Some(old) = self
                .streams
                .as_mut_slice()
                .iter_mut()
                .find(|s| s.pid == stream.pid)
            {
                old.stream_type = stream.stream_type;
                old.native_codec = codec;
                old.language = language;
                old.evidence = evidence;
            } else {
                if self.streams.len() >= self.limits.streams {
                    return Err(Error::Unsupported("native catalogue stream limit"));
                }
                self.streams
                    .try_push(NativeStreamV1 {
                        pid: stream.pid,
                        stream_type: stream.stream_type,
                        native_codec: codec,
                        language,
                        evidence,
                    })
                    .map_err(|e| e.error)?;
            }
        }
        self.programs
            .as_mut_slice()
            .iter_mut()
            .find(|p| p.program == h.extension)
            .ok_or(Error::Invalid("native catalogue program vanished"))?
            .native_pcr_pid = map.pcr_pid;
        if self.pending_pmt == Some((h.extension, h.version)) {
            self.pending_pmt = None;
        }
        Ok(())
    }
    fn validate_terminal_witness_v1(
        &self,
        end: &MediaEntityEndV1,
        input: &InputCursor,
    ) -> Result<()> {
        if let Some(e) = self.fault {
            return Err(e);
        }
        if self.ended {
            return Err(Error::Invalid("native catalogue duplicate logical EOF"));
        }
        if !self.budget.same_pool(input.budget()) || !self.budget.same_pool(&end.budget) {
            return Err(Error::ForeignPool);
        }
        if end.program != self.program
            || end.format != self.format
            || end.location.source != input.source()
            || self.entity.is_some_and(|s| s != input.source())
            || end.location.offset != input.position()
            || end.token != input.token()
        {
            return Err(Error::Invalid("native catalogue terminal witness mismatch"));
        }
        for e in self.evidence.as_slice() {
            for r in e.ranges.as_slice() {
                if r.entity != end.location.source || r.end()? > end.location.offset {
                    return Err(Error::Invalid(
                        "native catalogue evidence beyond actual end",
                    ));
                }
            }
        }
        Ok(())
    }
    /// Real core/framer witness; advertised length alone never establishes EOF.
    pub fn finish(&mut self, end: &MediaEntityEndV1, input: &InputCursor) -> Result<()> {
        self.validate_terminal_witness_v1(end, input)?;
        if self.pending_pat.is_some() {
            return Err(Error::Unsupported(
                "incomplete current PAT requires compatibility review",
            ));
        }
        if self.pending_pmt.is_some() {
            return Err(Error::Unsupported(
                "unaccepted PMT requires compatibility review",
            ));
        }
        self.ended = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TransportStream;
    use triergon_core::{InputWindow, SourceCapabilities, Step};
    fn fixture(budget: &Budget) -> (PatTable, MediaEntityEndV1, InputCursor) {
        let data = include_bytes!("../tests/data/ts188-basic.ts");
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
        let mut demux = TransportStream::new(
            budget.clone(),
            PacketFormat::Ts188,
            ParseLimits::default(),
            1,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut pat = None;
        for _ in 0..10000 {
            match demux
                .poll(&mut input, &mut WorkBudget::new(4096), &cancel)
                .unwrap()
            {
                Step::Ready(TraceEvent::Pat(t)) => pat = Some(t),
                Step::Ready(_) | Step::Yield => {}
                Step::NeedInput(r) => {
                    let at = usize::try_from(r.offset).unwrap();
                    if at == data.len() {
                        input.finish(r.source, r.token).unwrap();
                    } else {
                        let end = (at + r.maximum).min(data.len());
                        input
                            .provide(
                                &InputWindow::new(
                                    r.source,
                                    r.token,
                                    r.offset,
                                    Bytes::copy_from_slice(budget, &data[at..end]).unwrap(),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Step::End(_) => {
                    return (
                        pat.unwrap(),
                        demux
                            .media_entity_end_v1(&mut input, &mut WorkBudget::new(1), &cancel)
                            .unwrap()
                            .unwrap(),
                        input,
                    );
                }
            }
        }
        panic!("bounded fixture parser did not finish")
    }
    #[test]
    fn empty_work_is_retryable_without_mutation() {
        let b = Budget::new(1024 * 1024).unwrap();
        let (pat, _, _) = fixture(&b);
        let e = TraceEvent::Pat(pat);
        let mut view = NativeCatalogueV1::new(b, ParseLimits::default(), 1).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(!view.observe(&e, &mut WorkBudget::new(0), &cancel).unwrap());
        assert!(view.programs().is_empty());
        assert!(view.evidence().is_empty());
        assert!(
            view.observe(
                &e,
                &mut WorkBudget::new(NativeCatalogueV1::required_work(&e).unwrap()),
                &cancel
            )
            .unwrap()
        );
        assert_eq!(view.programs()[0].program, 1);
    }
    #[test]
    fn cancellation_keeps_the_actual_table_unconsumed() {
        let b = Budget::new(1024 * 1024).unwrap();
        let (pat, _, _) = fixture(&b);
        let e = TraceEvent::Pat(pat);
        let mut view = NativeCatalogueV1::new(b, ParseLimits::default(), 1).unwrap();
        let cancel = AtomicBool::new(true);
        assert_eq!(
            view.observe(&e, &mut WorkBudget::new(4096), &cancel),
            Err(Error::Cancelled)
        );
        assert!(view.evidence().is_empty());
        cancel.store(false, Ordering::Relaxed);
        assert!(
            view.observe(&e, &mut WorkBudget::new(4096), &cancel)
                .unwrap()
        );
    }
    #[test]
    fn foreign_pool_is_not_admitted_as_source_evidence() {
        let b = Budget::new(1024 * 1024).unwrap();
        let (pat, _, _) = fixture(&b);
        let mut view =
            NativeCatalogueV1::new(Budget::new(1024 * 1024).unwrap(), ParseLimits::default(), 1)
                .unwrap();
        assert_eq!(
            view.observe(
                &TraceEvent::Pat(pat),
                &mut WorkBudget::new(4096),
                &AtomicBool::new(false)
            ),
            Err(Error::ForeignPool)
        );
        assert!(view.evidence().is_empty());
    }
    #[test]
    fn changed_public_pat_entry_cannot_replace_the_encoded_fact() {
        let b = Budget::new(1024 * 1024).unwrap();
        let (mut pat, _, _) = fixture(&b);
        pat.programs.as_mut_slice()[0].pid += 1;
        let mut view = NativeCatalogueV1::new(b, ParseLimits::default(), 1).unwrap();
        assert!(
            view.observe(
                &TraceEvent::Pat(pat),
                &mut WorkBudget::new(4096),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(view.fault().is_some());
        assert!(view.programs().is_empty());
    }
    #[test]
    fn source_range_beyond_the_real_drained_end_is_rejected() {
        let b = Budget::new(1024 * 1024).unwrap();
        let (mut pat, end, input) = fixture(&b);
        pat.source_ranges.as_mut_slice()[0].range.offset = end.location().offset + 1;
        let mut view = NativeCatalogueV1::new(b, ParseLimits::default(), 1).unwrap();
        assert!(
            view.observe(
                &TraceEvent::Pat(pat),
                &mut WorkBudget::new(4096),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
        assert!(view.finish(&end, &input).is_err());
        assert!(!view.is_ended());
    }
    #[test]
    fn actual_end_from_another_cursor_is_not_eof() {
        let b = Budget::new(1024 * 1024).unwrap();
        let (pat, end, _) = fixture(&b);
        let input = InputCursor::new(
            &b,
            SourceId(2),
            SourceCapabilities {
                length: Some(end.location().offset),
                random_access: false,
            },
            4096,
        )
        .unwrap();
        let mut view = NativeCatalogueV1::new(b, ParseLimits::default(), 1).unwrap();
        assert!(
            view.observe(
                &TraceEvent::Pat(pat),
                &mut WorkBudget::new(4096),
                &AtomicBool::new(false)
            )
            .unwrap()
        );
        assert!(view.finish(&end, &input).is_err());
        assert!(!view.is_ended());
    }
    #[test]
    fn failed_charged_growth_releases_all_owned_capacity() {
        let b = Budget::new(1024 * 1024).unwrap();
        let (pat, _, _) = fixture(&b);
        let before = b.used();
        let held = b.reserve(b.available() - 16).unwrap();
        let mut view = NativeCatalogueV1::new(b.clone(), ParseLimits::default(), 1).unwrap();
        assert!(
            view.observe(
                &TraceEvent::Pat(pat),
                &mut WorkBudget::new(4096),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(view.fault().is_some());
        drop(view);
        drop(held);
        // The consumed PAT owner has also been dropped; no view keeps its bytes.
        assert!(b.used() < before);
    }
}
