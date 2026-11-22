//! The demuxer's programs and streams.
//!
//! [`Catalogue`] projects strict PAT and PMT events onto FFmpeg's program and
//! stream order, its absent-PCR value and language tags.
//! [`PsiCompatibilityPolicies`] adds the tables the demuxer accepts and the
//! source view does not, and [`PsiCompatibilityEvidence`] records each one with
//! its source status.
//!
//! The bounded catalogue preserves H.222.0 source validation. Compatibility
//! observations carry their source status, and only accepted tables activate routes.
use crate::psi::{DESCRIPTOR_LENGTH, PMT_PROGRAM_IDENTITY};
use crate::psi::{DescriptorNamespace, ElementaryStream, PatTable, ProgramMap, PsiCrc32, Section};
use crate::stream::{MediaEntityEnd, TraceEvent};
use crate::{PacketFormat, ParseLimits, SourceRange};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, InputCursor, Result, SourceId, WorkBudget};

mod psi_compatibility;
pub use psi_compatibility::{
    PsiCompatibility, PsiCompatibilityEvidence, PsiCompatibilityPolicies, PsiCompatibilityRecord,
    PsiCompatibilitySourceStatus,
};

/// One program of the catalogue, from the PAT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program {
    /// The 16-bit program number.
    pub program: u16,
    /// The 13-bit PID of the program's PMT.
    pub pmt_pid: u16,
    /// The demux view’s PCR PID, or zero when no valid PMT supplied one.
    pub pcr_pid: u16,
    /// Index into [`Catalogue::evidence`] of the section the entry was read from.
    pub evidence: usize,
}
/// One elementary stream of the selected program, from its PMT.
#[derive(Debug)]
pub struct Stream {
    /// The 13-bit elementary PID.
    pub pid: u16,
    /// The 8-bit PMT stream type.
    pub stream_type: u8,
    /// The demux codec tag: a matching registration FourCC in little-endian order, or the PMT
    /// stream type when there is no such registration.
    pub codec_tag: u32,
    /// Comma-separated ISO 639 languages from the first language list or from the ordered DVB
    /// subtitling services. Empty if no language is available.
    pub language: BudgetVec<u8>,
    /// Whether the selected language list includes audio type 2 or a DVB service for the hard of
    /// hearing.
    pub hearing_impaired: bool,
    /// Whether the selected language descriptor includes audio type 3.
    pub visual_impaired: bool,
    /// Index into [`Catalogue::evidence`] of the section the entry was read from.
    pub evidence: usize,
    codec: CatalogueCodec,
    dvb_initialization: Option<BudgetVec<u8>>,
}
impl Stream {
    /// The codec the demuxer opens for the stream.
    pub fn codec(&self) -> CatalogueCodec {
        self.codec
    }

    /// The DVB subtitle decoder initialization, with five bytes per service in PMT order:
    /// big-endian composition and ancillary page IDs followed by the subtitling type.
    pub fn dvb_initialization(&self) -> Option<&[u8]> {
        self.dvb_initialization.as_ref().map(BudgetVec::as_slice)
    }
}
/// Demux catalogue identity, never a source namespace or codec configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogueCodec {
    /// AVC video.
    Avc,
    /// HEVC video.
    Hevc,
    /// AAC in ADTS.
    AacAdts,
    /// AAC in LATM.
    AacLatm,
    /// MPEG-1 or MPEG-2 audio, without identifying an audio layer.
    MpegAudio,
    /// AC-3 audio.
    Ac3,
    /// E-AC-3 audio.
    Eac3,
    /// DVB subtitles carried in private PES packets.
    DvbSubtitle,
}
impl CatalogueCodec {
    /// The codec name the demuxer reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Avc => "h264",
            Self::Hevc => "hevc",
            Self::AacAdts => "aac",
            Self::AacLatm => "aac_latm",
            Self::MpegAudio => "mpeg-audio",
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
            Self::DvbSubtitle => "dvb_subtitle",
        }
    }
    /// The demuxer's media type code: 0 for video, 1 for audio, 3 for subtitles.
    pub fn media_type(self) -> i32 {
        match self {
            Self::Avc | Self::Hevc => 0,
            Self::DvbSubtitle => 3,
            _ => 1,
        }
    }
}
/// The source sections one catalogue entry was read from.
#[derive(Debug)]
pub struct CatalogueEvidence {
    /// Complete original PSI sections in table order, including their CRCs.
    pub bytes: BudgetVec<Bytes>,
    /// Where those bytes lie in the source.
    pub ranges: BudgetVec<SourceRange>,
}
/// The demuxer's programs and streams of one physical entity, built from strict PAT and PMT
/// events.
#[derive(Debug)]
pub struct Catalogue {
    budget: Budget,
    limits: ParseLimits,
    program: u16,
    entity: Option<SourceId>,
    programs: BudgetVec<Program>,
    streams: BudgetVec<Stream>,
    evidence: BudgetVec<CatalogueEvidence>,
    pending_pat: Option<(u16, u8)>,
    pending_pmt: Option<(u16, u8)>,
    accepted_pat: Option<usize>,
    accepted_pmt: Option<(u16, usize)>,
    retained_ranges: usize,
    ended: bool,
    fault: Option<Error>,
    compatibility: Option<psi_compatibility::CompatibilityState>,
    format: PacketFormat,
    selected_carriage: bool,
}
impl Catalogue {
    /// A catalogue of `program` for TS188 AVC and HEVC streams; see
    /// [`new_selected_carriage`](Self::new_selected_carriage) for audio and M2TS.
    ///
    /// Fails with `Invalid` for program 0 or invalid `limits`, and with `Unsupported` when
    /// `limits` allow more than 64 programs, 64 streams or 4096 source spans.
    pub fn new(budget: Budget, limits: ParseLimits, program: u16) -> Result<Self> {
        limits.validate()?;
        if limits.programs > 64 || limits.streams > 64 || limits.source_spans > 4096 {
            return Err(Error::Unsupported(
                "demux catalogue limits exceed 64 programs, 64 streams or 4096 source spans",
            ));
        }
        if program == 0 {
            return Err(Error::Invalid(
                "demux catalogue requires a nonzero program number",
            ));
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
            accepted_pat: None,
            accepted_pmt: None,
            retained_ranges: 0,
            ended: false,
            fault: None,
            compatibility: None,
            format: PacketFormat::Ts188,
            selected_carriage: false,
        })
    }
    /// TS188/M2TS192 demux view for video, audio and DVB subtitles.
    pub fn new_selected_carriage(
        budget: Budget,
        limits: ParseLimits,
        program: u16,
        format: PacketFormat,
    ) -> Result<Self> {
        let mut value = Self::new(budget, limits, program)?;
        value.format = format;
        value.selected_carriage = true;
        Ok(value)
    }
    fn codec(&self, stream: &ElementaryStream, program: &[u8]) -> Result<CatalogueCodec> {
        let kind = stream.stream_type;
        match kind {
            0x1b => Ok(CatalogueCodec::Avc),
            0x24 => Ok(CatalogueCodec::Hevc),
            _ if !self.selected_carriage => Err(Error::Unsupported(
                "demux video catalogue supports only AVC and HEVC stream types",
            )),
            0x03 | 0x04 => Ok(CatalogueCodec::MpegAudio),
            0x0f => Ok(CatalogueCodec::AacAdts),
            0x11 => Ok(CatalogueCodec::AacLatm),
            6 if stream.descriptor_namespace() != DescriptorNamespace::Atsc
                && stream
                    .descriptors()
                    .any(|item| item.is_ok_and(|descriptor| descriptor.tag == 0x59)) =>
            {
                Ok(CatalogueCodec::DvbSubtitle)
            }
            6 | 0x81 | 0x87 => {
                use crate::compat::pes::observer::{
                    PrivateAudioCarriage as C, PrivateAudioDescriptors,
                };
                let descriptors = PrivateAudioDescriptors {
                    program,
                    elementary: stream.descriptor_bytes.as_ref(),
                };
                // A registered DVB E-AC-3 route has both the registration and the
                // identifying audio descriptor. The registered-carriage check
                // validates that pair and rejects conflicting descriptors.
                let observed = if kind == 6
                    && matches!(stream.descriptor_namespace(), DescriptorNamespace::Dvb(_))
                    && stream.descriptors().any(|item| {
                        item.is_ok_and(|descriptor| {
                            descriptor.registration().is_ok_and(|registration| {
                                &registration.format_identifier == b"EAC3"
                            })
                        })
                    }) {
                    C::from_hls_pmt(kind, descriptors, self.limits)?
                } else {
                    C::from_observed_pmt(kind, descriptors, self.limits)?
                };
                Ok(match observed {
                    C::DvbAc3Tag | C::PrivateType81Ac3 => CatalogueCodec::Ac3,
                    C::DvbEac3Tag | C::PrivateType87Eac3 => CatalogueCodec::Eac3,
                })
            }
            _ => Err(Error::Unsupported(
                "stream type or descriptors are unsupported by the selected demux catalogue",
            )),
        }
    }
    /// PAT programs in demux order. The slice is empty before any PAT is admitted.
    pub fn programs(&self) -> &[Program] {
        self.programs.as_slice()
    }
    /// The streams of the selected program, in the demuxer's order.
    pub fn streams(&self) -> &[Stream] {
        self.streams.as_slice()
    }
    /// The retained source sections that the `evidence` indices name.
    pub fn evidence(&self) -> &[CatalogueEvidence] {
        self.evidence.as_slice()
    }
    #[cfg(test)]
    fn has_pending_current_pat(&self) -> bool {
        self.pending_pat.is_some()
    }
    /// Whether logical EOF has been observed.
    pub fn is_ended(&self) -> bool {
        self.ended
    }
    #[cfg(test)]
    fn fault(&self) -> Option<Error> {
        self.fault
    }
    /// Work units for atomically admitting one bounded source event.
    /// Returns `Unsupported` for an over-limit PAT or `Overflow` if the count exceeds `u64`.
    pub fn required_work(event: &TraceEvent) -> Result<u64> {
        if matches!(event,TraceEvent::Pat(t) if t.source_sections.len()>256 || t.source_ranges.len()>4096 || t.programs.len()>64)
        {
            return Err(Error::Unsupported(
                "demux catalogue PAT exceeds the section, range or program bound",
            ));
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
    /// Admits one strict source event when `work` covers [`required_work`](Self::required_work);
    /// `false` leaves the event unconsumed.
    ///
    /// Fails with `Unsupported` for a source shape outside the catalogue's profile, such as a
    /// rejected PMT or an entity discontinuity, with `Invalid` for an inconsistent table or after
    /// logical EOF, and with `Cancelled` when `cancel` is set. Errors are returned by every later
    /// call.
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
            return Err(Error::Invalid("demux catalogue input after logical EOF"));
        }
        if !work.try_charge(Self::required_work(event)?) {
            return Ok(false);
        }
        let result = match event {
            TraceEvent::Section(section)
                if self.selected_carriage && self.is_accepted_repeat(section) =>
            {
                self.section_source(section)
            }
            _ => self.observe_inner(event, cancel),
        };
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
    fn section_source(&mut self, section: &Section) -> Result<()> {
        if !section.bytes().belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        self.source(section.source_ranges())
    }
    fn is_accepted_repeat(&self, section: &Section) -> bool {
        let raw = section.bytes().as_ref();
        if section.crc_remainder() != Some(0) {
            return false;
        }
        let index = match (section.pid(), raw.first()) {
            (0, Some(0)) if self.pending_pat.is_none() => self.accepted_pat,
            (pid, Some(2)) if self.pending_pmt.is_none() => self
                .accepted_pmt
                .filter(|&(pmt_pid, _)| pid == pmt_pid)
                .map(|(_, index)| index),
            _ => None,
        };
        index
            .and_then(|index| self.evidence.as_slice().get(index))
            .is_some_and(|e| e.bytes.len() == 1 && e.bytes.as_slice()[0].as_ref() == raw)
    }
    fn retain(&mut self, backings: &[Bytes], ranges: &[SourceRange]) -> Result<usize> {
        if self.evidence.len() >= self.limits.source_spans {
            return Err(Error::Unsupported(
                "demux catalogue source record limit reached",
            ));
        }
        let retained = self
            .retained_ranges
            .checked_add(ranges.len())
            .ok_or(Error::Overflow)?;
        if retained > self.limits.source_spans {
            return Err(Error::Unsupported(
                "demux catalogue retained source range limit exceeded",
            ));
        }
        let backing_bytes = backings
            .iter()
            .try_fold(0usize, |n, b| n.checked_add(b.len()).ok_or(Error::Overflow))?;
        let range_bytes = ranges
            .iter()
            .try_fold(0usize, |n, r| n.checked_add(r.bytes).ok_or(Error::Overflow))?;
        if backing_bytes != range_bytes || (backing_bytes != 0 && ranges.is_empty()) {
            return Err(Error::Invalid(
                "demux catalogue retained bytes disagree with their source ranges",
            ));
        }
        let mut bytes = BudgetVec::new(&self.budget);
        let mut kept = BudgetVec::new(&self.budget);
        for b in backings {
            if !b.belongs_to(&self.budget) {
                return Err(Error::ForeignPool);
            }
            bytes.try_push(b.clone()).map_err(|e| e.error)?;
        }
        kept.try_extend_from_slice(ranges)?;
        self.source(ranges)?;
        let index = self.evidence.len();
        self.evidence
            .try_push(CatalogueEvidence {
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
                self.section_source(s)?;
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
                if f.kind == crate::stream::FaultKind::Psi
                    && matches!(
                        f.error,
                        Error::Invalid(DESCRIPTOR_LENGTH | PMT_PROGRAM_IDENTITY)
                    ) =>
            {
                return Err(Error::Unsupported(
                    "rejected PMT requires explicit compatibility review",
                ));
            }
            TraceEvent::Discontinuity { .. } => {
                return Err(Error::Unsupported(
                    "demux catalogue requires one physical entity",
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
            return Err(Error::Invalid(
                "demux catalogue requires a complete source PAT within its limits",
            ));
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
                return Err(Error::Invalid(
                    "demux catalogue PAT section disagrees with the source table",
                ));
            }
            let first = table
                .source_ranges
                .as_slice()
                .iter()
                .find(|r| usize::from(r.section_number) == section)
                .ok_or(Error::Invalid(
                    "demux catalogue PAT section is missing its source range",
                ))?
                .range;
            if previous.is_some_and(|old| old >= first.offset) {
                return Err(Error::Unsupported(
                    "demux catalogue requires PAT sections in section-number order",
                ));
            }
            previous = Some(first.offset);
            for entry in raw[8..raw.len() - 4].chunks_exact(4) {
                let expected = table
                    .programs
                    .as_slice()
                    .get(position)
                    .ok_or(Error::Invalid("demux catalogue PAT entry missing"))?;
                if expected.program_number != u16::from_be_bytes([entry[0], entry[1]])
                    || expected.pid != u16::from_be_bytes([entry[2], entry[3]]) & 0x1fff
                {
                    return Err(Error::Invalid(
                        "demux catalogue PAT entry disagrees with the encoded program or PID",
                    ));
                }
                if entry[2] & 0xe0 != 0xe0 || !(0x10..=0x1ffe).contains(&expected.pid) {
                    return Err(Error::Invalid(
                        "demux catalogue PAT has an invalid encoded PMT PID",
                    ));
                }
                position += 1;
            }
        }
        if position != table.programs.len() {
            return Err(Error::Invalid(
                "demux catalogue PAT entry count disagrees with the encoded table",
            ));
        }
        let mut ranges = BudgetVec::new(&self.budget);
        for r in table.source_ranges.as_slice() {
            ranges.try_push(r.range).map_err(|e| e.error)?;
        }
        let evidence = self.retain(table.source_sections.as_slice(), ranges.as_slice())?;
        self.accepted_pat = Some(evidence);
        if self.accepted_pmt.is_some_and(|(pid, _)| {
            !table
                .programs
                .as_slice()
                .iter()
                .any(|entry| entry.program_number == self.program && entry.pid == pid)
        }) {
            self.accepted_pmt = None;
        }
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
                    return Err(Error::Unsupported("demux catalogue program limit reached"));
                }
                self.programs
                    .try_push(Program {
                        program: e.program_number,
                        pmt_pid: e.pid,
                        pcr_pid: 0,
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
            return Err(Error::Invalid(
                "demux catalogue PMT disagrees with the source section",
            ));
        }
        if map.streams.len() > self.limits.streams
            || self
                .programs
                .as_slice()
                .iter()
                .all(|p| p.program != h.extension)
        {
            return Err(Error::Invalid("demux catalogue PMT has no PAT route"));
        }
        if map.pcr_pid != u16::from_be_bytes([raw[8], raw[9]]) & 0x1fff {
            return Err(Error::Invalid(
                "demux catalogue PMT disagrees with the encoded PCR PID",
            ));
        }
        let start = 12 + (usize::from(u16::from_be_bytes([raw[10], raw[11]]) & 0x0fff));
        if start > raw.len() - 4 || map.program_descriptors.as_ref() != &raw[12..start] {
            return Err(Error::Invalid(
                "demux catalogue program descriptors disagree with the encoded PMT",
            ));
        }
        if !map.program_descriptors.is_empty() {
            return Err(Error::Unsupported(
                "demux catalogue requires an empty program descriptor loop",
            ));
        }
        let mut at = start;
        for stream in map.streams.as_slice() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let encoded = raw.get(at..at + 5).ok_or(Error::Invalid(
                "demux catalogue PMT stream header is truncated",
            ))?;
            let end = at + 5 + usize::from(u16::from_be_bytes([encoded[3], encoded[4]]) & 0x0fff);
            if end > raw.len() - 4
                || stream.stream_type != encoded[0]
                || stream.pid != u16::from_be_bytes([encoded[1], encoded[2]]) & 0x1fff
                || stream.descriptor_bytes.as_ref() != &raw[at + 5..end]
            {
                return Err(Error::Invalid(
                    "demux catalogue stream disagrees with the encoded PMT entry",
                ));
            }
            if !(matches!(stream.stream_type, 0x1b | 0x24)
                || self.selected_carriage
                    && matches!(
                        stream.stream_type,
                        0x03 | 0x04 | 0x0f | 0x11 | 6 | 0x81 | 0x87
                    ))
            {
                return Err(Error::Unsupported(
                    "demux video catalogue supports only AVC and HEVC stream types",
                ));
            }
            at = end;
        }
        if at != raw.len() - 4 {
            return Err(Error::Invalid(
                "demux catalogue stream entries do not cover the encoded PMT",
            ));
        }
        let evidence = self.retain(
            std::slice::from_ref(&map.source),
            map.source_ranges.as_slice(),
        )?;
        for stream in map.streams.as_slice() {
            let codec = self.codec(stream, map.program_descriptors.as_ref())?;
            let mut codec_tag = u32::from(stream.stream_type);
            let mut language = BudgetVec::new(&self.budget);
            let mut hearing_impaired = false;
            let mut visual_impaired = false;
            let mut dvb_initialization =
                (codec == CatalogueCodec::DvbSubtitle).then(|| BudgetVec::new(&self.budget));
            for descriptor in stream.descriptors() {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let descriptor = descriptor?;
                if descriptor.tag == 0x59
                    && let Some(pages) = &mut dvb_initialization
                {
                    let subtitles = descriptor.dvb_subtitling()?;
                    for service in subtitles.services() {
                        Self::append_language(&mut language, &service.language_code)?;
                        pages.try_extend_from_slice(&service.composition_page_id.to_be_bytes())?;
                        pages.try_extend_from_slice(&service.ancillary_page_id.to_be_bytes())?;
                        pages
                            .try_push(service.subtitling_type)
                            .map_err(|e| e.error)?;
                        hearing_impaired |= (0x20..=0x26).contains(&service.subtitling_type);
                    }
                    continue;
                }
                if descriptor.tag == 0x05 {
                    let registration = descriptor.registration()?.format_identifier;
                    if matches!(
                        (&registration, codec),
                        (b"HEVC", CatalogueCodec::Hevc) | (b"EAC3", CatalogueCodec::Eac3)
                    ) {
                        codec_tag = u32::from_le_bytes(registration);
                        continue;
                    }
                }
                if stream.descriptor_namespace() == DescriptorNamespace::Atsc
                    && matches!(
                        (stream.stream_type, codec, descriptor.tag),
                        (0x81, CatalogueCodec::Ac3, 0x81) | (0x87, CatalogueCodec::Eac3, 0xcc)
                    )
                {
                    continue;
                }
                if !(matches!(descriptor.tag, 0x0a | 0xee)
                    || self.selected_carriage
                        && matches!(codec, CatalogueCodec::Ac3 | CatalogueCodec::Eac3)
                        && matches!(descriptor.tag, 0x6a | 0x7a))
                {
                    return Err(Error::Unsupported(
                        "descriptor is unsupported for video in the demux catalogue",
                    ));
                }
                if descriptor.tag == 0x0a && language.is_empty() && dvb_initialization.is_none() {
                    for e in descriptor.languages()? {
                        Self::append_language(&mut language, &e.language_code)?;
                        hearing_impaired |= e.audio_type == 2;
                        visual_impaired |= e.audio_type == 3;
                    }
                }
            }
            if dvb_initialization.as_ref().is_some_and(BudgetVec::is_empty) {
                return Err(Error::Invalid("DVB subtitle descriptor has no services"));
            }
            if let Some(old) = self
                .streams
                .as_mut_slice()
                .iter_mut()
                .find(|s| s.pid == stream.pid)
            {
                old.stream_type = stream.stream_type;
                old.codec_tag = codec_tag;
                old.codec = codec;
                old.language = language;
                old.hearing_impaired = hearing_impaired;
                old.visual_impaired = visual_impaired;
                old.dvb_initialization = dvb_initialization;
                old.evidence = evidence;
            } else {
                if self.streams.len() >= self.limits.streams {
                    return Err(Error::Unsupported("demux catalogue stream limit reached"));
                }
                self.streams
                    .try_push(Stream {
                        pid: stream.pid,
                        stream_type: stream.stream_type,
                        codec_tag,
                        codec,
                        language,
                        hearing_impaired,
                        visual_impaired,
                        dvb_initialization,
                        evidence,
                    })
                    .map_err(|e| e.error)?;
            }
        }
        let program = self
            .programs
            .as_mut_slice()
            .iter_mut()
            .find(|p| p.program == h.extension)
            .ok_or(Error::Invalid(
                "demux catalogue program missing at PMT update",
            ))?;
        program.pcr_pid = map.pcr_pid;
        self.accepted_pmt = Some((program.pmt_pid, evidence));
        if self.pending_pmt == Some((h.extension, h.version)) {
            self.pending_pmt = None;
        }
        Ok(())
    }
    fn append_language(language: &mut BudgetVec<u8>, code: &[u8; 3]) -> Result<()> {
        if code.contains(&0) {
            return Err(Error::Unsupported(
                "demux catalogue language code contains NUL",
            ));
        }
        if !language.is_empty() {
            language.try_push(b',').map_err(|e| e.error)?;
        }
        language.try_extend_from_slice(code)
    }
    fn validate_terminal_witness(&self, end: &MediaEntityEnd, input: &InputCursor) -> Result<()> {
        if let Some(e) = self.fault {
            return Err(e);
        }
        if self.ended {
            return Err(Error::Invalid(
                "demux catalogue already reached logical EOF",
            ));
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
            return Err(Error::Invalid(
                "demux catalogue entity end disagrees with the input cursor",
            ));
        }
        for e in self.evidence.as_slice() {
            for r in e.ranges.as_slice() {
                if r.entity != end.location.source || r.end()? > end.location.offset {
                    return Err(Error::Invalid(
                        "demux catalogue source range lies outside the completed entity",
                    ));
                }
            }
        }
        Ok(())
    }
    /// Seal completion using the parser’s physical end and the bound input cursor.
    /// A missing table, incomplete current PAT or mismatched EOF witness is refused.
    pub fn finish(&mut self, end: &MediaEntityEnd, input: &InputCursor) -> Result<()> {
        self.validate_terminal_witness(end, input)?;
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
    use crate::stream::TransportStream;
    use triergon_core::{InputWindow, SourceCapabilities, Step};
    fn fixture(budget: &Budget) -> (PatTable, MediaEntityEnd, InputCursor) {
        let data = include_bytes!("../../tests/data/ts188-basic.ts");
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
                            .media_entity_end(&mut input, &mut WorkBudget::new(1), &cancel)
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
        let mut view = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(!view.observe(&e, &mut WorkBudget::new(0), &cancel).unwrap());
        assert!(view.programs().is_empty());
        assert!(view.evidence().is_empty());
        assert!(
            view.observe(
                &e,
                &mut WorkBudget::new(Catalogue::required_work(&e).unwrap()),
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
        let mut view = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
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
            Catalogue::new(Budget::new(1024 * 1024).unwrap(), ParseLimits::default(), 1).unwrap();
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
        let mut view = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
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
        let mut view = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
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
        let mut view = Catalogue::new(b, ParseLimits::default(), 1).unwrap();
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
        let mut view = Catalogue::new(b.clone(), ParseLimits::default(), 1).unwrap();
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
        // The consumed PAT storage has also been dropped; no view keeps its bytes.
        assert!(b.used() < before);
    }
}
