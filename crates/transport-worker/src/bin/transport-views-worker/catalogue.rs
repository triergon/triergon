use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::container::CodecId;
use triergon_core::{Budget, Error, SourceLocation, Step, WorkBudget};
use triergon_mpegts::compat::pes::SourceContext;
use triergon_mpegts::psi::{
    DescriptorNamespace, DvbNamespaceEvidence, ElementaryStream, PatCollector, PatReader, PatTable,
    PmtReader, ProgramMap, Section, SectionAssembler, SectionHeader, TableStep,
};
use triergon_mpegts::reference_trace::catalogue::{
    Catalogue, PsiCompatibility, PsiCompatibilityPolicies, PsiCompatibilitySourceStatus,
};
use triergon_mpegts::stream::{FaultKind, TraceEvent, TransportStream};
use triergon_mpegts::transport::{Continuity, ContinuityOutcome, PacketFormat, TransportPacket};
use triergon_mpegts::{AssemblyStep, ParseLimits, SourceRange};

use super::packets::{self, Carrier, GRANT, SOURCE, record};
use triergon_harness_runner::{Operation, Totals};

#[derive(Clone, Copy)]
enum Policy {
    Strict,
    Compatible,
}

#[derive(Clone, Copy)]
struct Case {
    format: PacketFormat,
    program: u16,
    namespace: DescriptorNamespace,
    policy: Policy,
}

pub(super) fn operation<'a>(
    prefix: &str,
    data: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [size, program, namespace, policy] = arguments else {
        return Err("expected FORMAT PROGRAM NAMESPACE strict|compatible".into());
    };
    let case = Case {
        format: packets::format(size)?,
        program: program
            .to_str()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n != 0)
            .ok_or("invalid program number")?,
        namespace: match namespace.to_str() {
            Some("unspecified") => DescriptorNamespace::Unspecified,
            Some("dvb") => {
                DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile)
            }
            Some("atsc") => DescriptorNamespace::Atsc,
            _ => return Err("expected unspecified, dvb or atsc namespace".into()),
        },
        policy: match policy.to_str() {
            Some("strict") => Policy::Strict,
            Some("compatible") => Policy::Compatible,
            _ => return Err("expected strict or compatible catalogue policy".into()),
        },
    };
    let (name, run): (_, CatalogueRun) = match prefix {
        "ts-catalogue-" => ("ts-catalogue", projected),
        "ts-psi-" => ("ts-psi-readers", standalone),
        "ts-video-descriptors-" => ("ts-video-descriptors", descriptors),
        _ => return Err("unknown catalogue view".into()),
    };
    Ok(Operation {
        name,
        run: Box::new(move |work, mut out| run(data, case, work, &mut out)),
    })
}

type CatalogueRun = fn(&[u8], Case, &mut Totals, &mut Option<&mut dyn Write>) -> Result<(), String>;

fn program_row(
    work: &mut Totals,
    out: &mut Option<&mut dyn Write>,
    number: u16,
    pmt: u16,
    pcr: u16,
    version: Option<u8>,
) -> Result<(), String> {
    record(
        work,
        out,
        5,
        &[
            0,
            number.into(),
            pmt.into(),
            pcr.into(),
            version.map_or(u64::MAX, u64::from),
        ],
        &[],
    )
}

fn block(
    work: &mut Totals,
    out: &mut Option<&mut dyn Write>,
    kind: u64,
    bytes: &[u8],
) -> Result<(), String> {
    record(work, out, 5, &[kind, bytes.len() as u64], bytes)
}

struct StreamRow<'a> {
    pid: u16,
    codec_tag: u32,
    codec: &'a str,
    language: &'a [u8],
    hearing_impaired: bool,
    visual_impaired: bool,
    dvb: Option<&'a [u8]>,
}

impl StreamRow<'_> {
    fn write(
        &self,
        program: u16,
        work: &mut Totals,
        out: &mut Option<&mut dyn Write>,
    ) -> Result<(), String> {
        record(
            work,
            out,
            5,
            &[
                1,
                program.into(),
                self.pid.into(),
                self.codec_tag.into(),
                self.hearing_impaired.into(),
                self.visual_impaired.into(),
            ],
            &[],
        )?;
        block(work, out, 2, self.codec.as_bytes())?;
        block(work, out, 3, self.language)?;
        if let Some(bytes) = self.dvb {
            block(work, out, 4, bytes)?;
        }
        Ok(())
    }
}

fn projected(
    data: &[u8],
    case: Case,
    work: &mut Totals,
    out: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    let budget = packets::budget()?;
    let limits = ParseLimits::default();
    let mut input = packets::input_cursor(&budget, data.len(), 4096)?;
    let mut transport = TransportStream::new_with_namespace(
        budget.clone(),
        case.format,
        limits,
        case.program,
        case.namespace,
    )
    .map_err(|e| e.to_string())?;
    let mut view = Catalogue::new_selected_carriage(budget, limits, case.program, case.format)
        .map_err(|e| e.to_string())?;
    let cancel = AtomicBool::new(false);
    let compatible = matches!(case.policy, Policy::Compatible);
    if compatible {
        view.enable_psi_compatibility(PsiCompatibilityPolicies::ALL)
            .map_err(|e| e.to_string())?;
    }
    let mut ordinal = 0;
    let mut observed = Vec::new();
    let result = loop {
        match transport.poll(&mut input, &mut WorkBudget::new(GRANT), &cancel) {
            Ok(Step::Ready(event)) => {
                let result = if compatible {
                    let context = SourceContext {
                        source_event_ordinal: ordinal,
                        epochs: transport.source_epochs(),
                        byte_run_epoch: None,
                    };
                    observed.push((context, SourceEvent::from(&event)));
                    view.required_psi_compatibility_work(&event)
                        .and_then(|units| {
                            view.observe_psi_compatibility(
                                &event,
                                SOURCE,
                                context,
                                &mut WorkBudget::new(units),
                                &cancel,
                            )
                        })
                } else {
                    view.observe(&event, &mut WorkBudget::new(GRANT), &cancel)
                };
                ordinal += 1;
                match result {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err("catalogue did not consume a fully funded event".into());
                    }
                    Err(error) => break Err(error),
                }
            }
            Ok(Step::NeedInput(request)) => packets::supply(&mut input, data, request)?,
            Ok(Step::Yield) => {}
            Ok(Step::End(_)) => {
                let end = transport
                    .media_entity_end(&mut input, &mut WorkBudget::new(GRANT), &cancel)
                    .and_then(|end| end.ok_or(Error::Invalid("missing media entity end")));
                break end.and_then(|end| {
                    if compatible {
                        let units = view.required_psi_compatibility_finish_work()?;
                        if !view.finish_psi_compatibility(
                            &end,
                            &input,
                            &mut WorkBudget::new(units),
                            &cancel,
                        )? {
                            return Err(Error::Invalid(
                                "catalogue did not consume a fully funded end",
                            ));
                        }
                        Ok(())
                    } else {
                        view.finish(&end, &input)
                    }
                });
            }
            Err(error) => break Err(error),
        }
    };
    if let Err(error) = result {
        eprintln!("catalogue projection: {error}");
        return record(work, out, 8, &[], &[]);
    }
    for evidence in view.evidence() {
        check_ranges(
            data,
            evidence.ranges.as_slice(),
            evidence
                .bytes
                .as_slice()
                .iter()
                .flat_map(|section| section.iter().copied()),
        )?;
    }
    for (index, evidence) in view.psi_compatibility_evidence().iter().enumerate() {
        let admitted = evidence.record();
        let bytes = evidence.section_bytes().as_ref();
        let header = wire_header(bytes).ok_or("missing retained PSI section header")?;
        if admitted.evidence_index != index
            || evidence.wire_header() != header
            || observed
                .get(admitted.context.source_event_ordinal as usize)
                .map(|item| item.0)
                != Some(admitted.context)
        {
            return Err("catalogue compatibility record differs from its source event".into());
        }
        let linked = view
            .evidence()
            .get(admitted.catalogue_evidence_index)
            .ok_or("missing linked PSI evidence")?;
        if !linked
            .bytes
            .as_slice()
            .iter()
            .any(|section| section.as_ref() == bytes)
        {
            return Err("catalogue record links to a different section".into());
        }
        check_ranges(data, evidence.source_ranges(), bytes.iter().copied())?;
        for carrier_index in 0..evidence.carrier_count() {
            let (carrier, context) = evidence
                .carrier(carrier_index)
                .ok_or("missing PSI evidence carrier")?;
            let location =
                usize::try_from(carrier.source_offset).map_err(|error| error.to_string())?;
            if carrier.source_id != Some(SOURCE)
                || data.get(location..location.saturating_add(carrier.source.len()))
                    != Some(carrier.source.as_ref())
                || observed.get(context.source_event_ordinal as usize)
                    != Some(&(context, SourceEvent::Carrier(carrier.source_offset)))
            {
                return Err("PSI evidence carrier differs from the offered input".into());
            }
        }
        check_psi_status(
            admitted.policy,
            header,
            evidence.source_status(),
            evidence.descriptor_stop_offset(),
            bytes,
            &observed,
        )?;
        if out.is_some() {
            eprintln!(
                "{}: {:?}",
                admitted.policy.label(),
                evidence.source_status()
            );
        }
    }
    // An admitted PMT can be empty or can update a PID earlier in stream order.
    let version = view.evidence().iter().rev().find_map(|evidence| {
        let bytes = evidence.bytes.as_slice().first()?.as_ref();
        let header = bytes.get(..6)?;
        (header[0] == 2 && u16::from_be_bytes([header[3], header[4]]) == case.program)
            .then(|| (header[5] >> 1) & 31)
    });
    for program in view.programs() {
        let selected = program.program == case.program;
        program_row(
            work,
            out,
            program.program,
            program.pmt_pid,
            program.pcr_pid,
            if selected { version } else { None },
        )?;
        if selected {
            for stream in view.streams() {
                StreamRow {
                    pid: stream.pid,
                    codec_tag: stream.codec_tag,
                    codec: stream.codec().name(),
                    language: stream.language.as_slice(),
                    hearing_impaired: stream.hearing_impaired,
                    visual_impaired: stream.visual_impaired,
                    dvb: stream.dvb_initialization(),
                }
                .write(program.program, work, out)?;
            }
        }
    }
    record(work, out, 7, &[], &[])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceEvent {
    Carrier(u64),
    Pat(SectionHeader),
    Diagnostic(FaultKind, Error, SourceLocation, Option<u16>),
    Other,
}

impl From<&TraceEvent> for SourceEvent {
    fn from(event: &TraceEvent) -> Self {
        match event {
            TraceEvent::Transport(packet) => Self::Carrier(packet.source_offset),
            TraceEvent::Section(section) if section.pid() == 0 => {
                wire_header(section.bytes()).map_or(Self::Other, Self::Pat)
            }
            TraceEvent::Diagnostic(fault) => {
                Self::Diagnostic(fault.kind, fault.error, fault.location, fault.pid)
            }
            _ => Self::Other,
        }
    }
}

fn wire_header(bytes: &[u8]) -> Option<SectionHeader> {
    let [table_id, _, _, high, low, version, section, last, ..] = *bytes else {
        return None;
    };
    Some(SectionHeader {
        table_id,
        extension: u16::from_be_bytes([high, low]),
        version: (version >> 1) & 31,
        current_next: version & 1 != 0,
        section_number: section,
        last_section_number: last,
    })
}

fn check_ranges(
    data: &[u8],
    ranges: &[SourceRange],
    mut expected: impl Iterator<Item = u8>,
) -> Result<(), String> {
    for range in ranges {
        let start = usize::try_from(range.offset).map_err(|error| error.to_string())?;
        let span = data
            .get(start..start.saturating_add(range.bytes))
            .ok_or("PSI range outside source")?;
        if range.entity != SOURCE || !span.iter().copied().eq(expected.by_ref().take(span.len())) {
            return Err("PSI evidence differs from the original source range".into());
        }
    }
    if expected.next().is_some() {
        return Err("PSI evidence ranges omit section bytes".into());
    }
    Ok(())
}

fn check_psi_status(
    policy: PsiCompatibility,
    header: SectionHeader,
    status: PsiCompatibilitySourceStatus,
    descriptor_stop: Option<usize>,
    bytes: &[u8],
    observed: &[(SourceContext, SourceEvent)],
) -> Result<(), String> {
    let valid = match status {
        PsiCompatibilitySourceStatus::IncompleteCurrentPat {
            section_number,
            last_section_number,
        } => {
            policy == PsiCompatibility::CurrentPatSectionBeforeWholeTable
                && header.table_id == 0
                && header.current_next
                && header.section_number == section_number
                && header.last_section_number == last_section_number
                && last_section_number != 0
        }
        PsiCompatibilitySourceStatus::NotRoutedByStrictIncompletePat {
            pat_source_event_ordinal,
        } => {
            policy == PsiCompatibility::CurrentPatSectionBeforeWholeTable
                && header.table_id == 2
                && matches!(observed.get(pat_source_event_ordinal as usize), Some((_, SourceEvent::Pat(pat))) if pat.table_id == 0 && pat.current_next && pat.last_section_number != 0)
        }
        PsiCompatibilitySourceStatus::RejectedPmt {
            kind,
            error,
            location,
            pid,
            source_event_ordinal,
            epochs,
        } => {
            header.table_id == 2
                && policy != PsiCompatibility::CurrentPatSectionBeforeWholeTable
                && observed
                    .get(source_event_ordinal as usize)
                    .is_some_and(|(context, event)| {
                        context.epochs == epochs
                            && *event == SourceEvent::Diagnostic(kind, error, location, Some(pid))
                    })
        }
    };
    let stop_matches = match policy {
        PsiCompatibility::PmtDescriptorStopsAfterStreamIdentity => {
            descriptor_stop.is_some_and(|offset| malformed_descriptor(bytes) == Some(offset))
        }
        PsiCompatibility::PmtNonzeroSection => {
            descriptor_stop.is_none()
                && (header.section_number != 0 || header.last_section_number != 0)
        }
        PsiCompatibility::CurrentPatSectionBeforeWholeTable => descriptor_stop.is_none(),
    };
    if valid && stop_matches {
        Ok(())
    } else {
        Err("PSI compatibility status disagrees with the wire and source diagnostic".into())
    }
}

fn malformed_descriptor(section: &[u8]) -> Option<usize> {
    let end = section.len().checked_sub(4)?;
    let program_length = usize::from(u16::from_be_bytes([
        *section.get(10)? & 15,
        *section.get(11)?,
    ]));
    let mut start = 12 + program_length;
    while start < end {
        let head = section.get(start..start + 5)?;
        let mut offset = start + 5;
        start = offset + usize::from(u16::from_be_bytes([head[3] & 15, head[4]]));
        if start > end {
            return None;
        }
        while offset < start {
            if offset + 2 > start {
                return Some(offset);
            }
            let next = offset + 2 + usize::from(section[offset + 1]);
            if next > start {
                return Some(offset);
            }
            offset = next;
        }
    }
    None
}

struct PsiPid {
    continuity: Continuity,
    assembler: SectionAssembler,
}

impl PsiPid {
    fn new(pid: u16, budget: &Budget, limits: ParseLimits) -> Result<Self, Error> {
        Ok(Self {
            continuity: Continuity::new(pid)?,
            assembler: SectionAssembler::new(pid, budget.clone(), limits)?,
        })
    }

    fn sections(
        &mut self,
        packet: TransportPacket,
        cancel: &AtomicBool,
    ) -> Result<Vec<Section>, Error> {
        let outcome = self.continuity.observe(&packet)?;
        if matches!(
            outcome,
            ContinuityOutcome::Damaged
                | ContinuityOutcome::Lost { .. }
                | ContinuityOutcome::SignalledDiscontinuity
                | ContinuityOutcome::ExcessDuplicate
        ) {
            self.assembler.discard();
        }
        if matches!(
            outcome,
            ContinuityOutcome::Damaged
                | ContinuityOutcome::Duplicate
                | ContinuityOutcome::ExcessDuplicate
        ) {
            return Ok(Vec::new());
        }
        if let Some(payload) = packet.payload
            && let Err(error) = self.assembler.push(
                payload,
                packet.payload_unit_start,
                SOURCE,
                packet.payload_source_offset,
            )
        {
            self.assembler.discard();
            if !syntax(error.error) {
                return Err(error.error);
            }
            return Ok(Vec::new());
        }
        let mut sections = Vec::new();
        loop {
            match self.assembler.poll(&mut WorkBudget::new(GRANT), cancel) {
                Ok(AssemblyStep::Output(section)) => sections.push(section),
                Ok(AssemblyStep::Yield) => {}
                Ok(_) => break,
                Err(error) if syntax(error) => {
                    self.assembler.discard();
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(sections)
    }
}

fn syntax(error: Error) -> bool {
    matches!(
        error,
        Error::Invalid(_) | Error::Truncated | Error::Limit { .. } | Error::Unsupported(_)
    )
}

struct Tables<'a> {
    budget: &'a Budget,
    limits: ParseLimits,
    namespace: DescriptorNamespace,
    pids: BTreeMap<u16, PsiPid>,
    collector: PatCollector,
    pat: Option<PatTable>,
    maps: BTreeMap<u16, ProgramMap>,
}

impl<'a> Tables<'a> {
    fn new(budget: &'a Budget, namespace: DescriptorNamespace) -> Result<Self, Error> {
        let limits = ParseLimits::default();
        Ok(Self {
            budget,
            limits,
            namespace,
            pids: BTreeMap::from([(0, PsiPid::new(0, budget, limits)?)]),
            collector: PatCollector::new(budget.clone(), limits)?,
            pat: None,
            maps: BTreeMap::new(),
        })
    }

    fn collect_pat(&mut self, section: &Section, cancel: &AtomicBool) -> Result<(), Error> {
        let mut reader = PatReader::new(section, self.budget, self.limits)?;
        if let TableStep::Output(section) = reader.poll(&mut WorkBudget::new(GRANT), cancel)? {
            self.collector.submit(section).map_err(|e| e.error)?;
            if let TableStep::Output(table) =
                self.collector.poll(&mut WorkBudget::new(GRANT), cancel)?
            {
                let programs = table.programs.as_slice();
                self.pids.retain(|pid, _| {
                    *pid == 0
                        || programs
                            .iter()
                            .any(|p| !p.is_network_pid() && p.pid == *pid)
                });
                for p in programs.iter().filter(|p| !p.is_network_pid()) {
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        self.pids.entry(p.pid)
                    {
                        entry.insert(PsiPid::new(p.pid, self.budget, self.limits)?);
                    }
                }
                self.maps.retain(|program, _| {
                    programs.iter().any(|p| {
                        p.program_number == *program
                            && self
                                .pat
                                .as_ref()
                                .is_some_and(|old| old.programs.as_slice().contains(p))
                    })
                });
                self.pat = Some(table);
            }
        }
        Ok(())
    }

    fn map(&self, section: &Section, cancel: &AtomicBool) -> Result<Option<ProgramMap>, Error> {
        let mut reader = if self.namespace == DescriptorNamespace::Unspecified {
            PmtReader::new(section, self.budget, self.limits)?
        } else {
            PmtReader::new_with_namespace(section, self.budget, self.limits, self.namespace)?
        };
        if let TableStep::Output(map) = reader.poll(&mut WorkBudget::new(GRANT), cancel)?
            && map.header.current_next
            && self.pat.as_ref().is_some_and(|pat| {
                pat.programs.as_slice().iter().any(|program| {
                    program.program_number == map.header.extension && program.pid == section.pid()
                })
            })
        {
            Ok(Some(map))
        } else {
            Ok(None)
        }
    }

    fn push(
        &mut self,
        carrier: Carrier,
        cancel: &AtomicBool,
        mut observe: impl FnMut(&ProgramMap) -> Result<(), String>,
    ) -> Result<(), String> {
        let packet = match carrier {
            Carrier::Packet(packet) => packet,
            Carrier::Rejected(_) => {
                for pid in self.pids.values_mut() {
                    pid.continuity.reset();
                    pid.assembler.discard();
                }
                return Ok(());
            }
        };
        let Some(pid) = self.pids.get_mut(&packet.pid) else {
            return Ok(());
        };
        for section in pid.sections(packet, cancel).map_err(|e| e.to_string())? {
            let result = if section.pid() == 0 {
                self.collect_pat(&section, cancel).map(|()| None)
            } else {
                self.map(&section, cancel)
            };
            match result {
                Ok(Some(map)) => {
                    observe(&map)?;
                    self.maps.insert(map.header.extension, map);
                }
                Ok(None) => {}
                Err(error) if syntax(error) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(())
    }
}

fn standalone(
    data: &[u8],
    case: Case,
    work: &mut Totals,
    out: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    let budget = packets::budget()?;
    let cancel = AtomicBool::new(false);
    let mut tables = Tables::new(&budget, case.namespace).map_err(|e| e.to_string())?;
    let clean = packets::walk(data, case.format, 4096, &budget, |carrier| {
        tables.push(carrier, &cancel, |_| Ok(()))
    })?;
    if let Some(pat) = tables.pat {
        for program in pat
            .programs
            .as_slice()
            .iter()
            .filter(|p| !p.is_network_pid())
        {
            let number = program.program_number;
            let map = tables.maps.get(&number);
            program_row(
                work,
                out,
                number,
                program.pid,
                map.map_or(0, |m| m.pcr_pid),
                map.map(|m| m.header.version),
            )?;
            if let Some(map) = map {
                for stream in map.streams.as_slice() {
                    source_stream(work, out, number, stream)?;
                }
            }
        }
    }
    record(work, out, if clean { 7 } else { 8 }, &[], &[])
}

fn source_stream(
    work: &mut Totals,
    out: &mut Option<&mut dyn Write>,
    program: u16,
    stream: &ElementaryStream,
) -> Result<(), String> {
    let codec = match stream.codec() {
        Some(CodecId::Mpeg2Video) => "mpeg2video",
        Some(CodecId::MpegAudio) => "mpeg-audio",
        Some(CodecId::H264) => "h264",
        Some(CodecId::Hevc) => "hevc",
        Some(CodecId::Aac) => "aac",
        Some(CodecId::AacLatm) => "aac_latm",
        Some(CodecId::Ac3) => "ac3",
        Some(CodecId::Eac3) => "eac3",
        Some(CodecId::Dts) => "dts",
        Some(CodecId::DvbSubtitle) => "dvb_subtitle",
        Some(CodecId::DvbTeletext) => "dvb_teletext",
        _ => "none",
    };
    let mut language = Vec::new();
    let mut pages = Vec::new();
    let mut hearing_impaired = false;
    let mut visual_impaired = false;
    for descriptor in stream.descriptors().map_while(Result::ok) {
        if descriptor.tag == 10 && language.is_empty() {
            for entry in descriptor.languages().map_err(|e| e.to_string())? {
                append_language(&mut language, &entry.language_code);
                hearing_impaired |= entry.audio_type == 2;
                visual_impaired |= entry.audio_type == 3;
            }
        }
        if codec == "dvb_subtitle" && descriptor.tag == 0x59 {
            for service in descriptor
                .dvb_subtitling()
                .map_err(|e| e.to_string())?
                .services()
            {
                append_language(&mut language, &service.language_code);
                pages.extend(service.composition_page_id.to_be_bytes());
                pages.extend(service.ancillary_page_id.to_be_bytes());
                pages.push(service.subtitling_type);
                hearing_impaired |= service.has_dvb_subtitling_type()
                    && (0x20..=0x26).contains(&service.subtitling_type);
            }
        }
    }
    StreamRow {
        pid: stream.pid,
        codec_tag: published_codec_tag(stream)?,
        codec,
        language: &language,
        hearing_impaired,
        visual_impaired,
        dvb: (codec == "dvb_subtitle").then_some(pages.as_slice()),
    }
    .write(program, work, out)
}

pub(super) fn published_codec_tag(stream: &ElementaryStream) -> Result<u32, String> {
    let mut tag = u32::from(stream.stream_type);
    for descriptor in stream.descriptors().map_while(Result::ok) {
        if descriptor.tag == 5 {
            let registration = descriptor.registration().map_err(|e| e.to_string())?;
            if matches!(
                (&registration.format_identifier, stream.codec()),
                (b"HEVC", Some(CodecId::Hevc)) | (b"EAC3", Some(CodecId::Eac3))
            ) {
                tag = u32::from_le_bytes(registration.format_identifier);
            }
        }
    }
    Ok(tag)
}

fn append_language(languages: &mut Vec<u8>, code: &[u8; 3]) {
    if !languages.is_empty() {
        languages.push(b',');
    }
    languages.extend_from_slice(code);
}

fn descriptors(
    data: &[u8],
    case: Case,
    work: &mut Totals,
    out: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    let budget = packets::budget()?;
    let cancel = AtomicBool::new(false);
    let mut tables = Tables::new(&budget, case.namespace).map_err(|e| e.to_string())?;
    let clean = packets::walk(data, case.format, 4096, &budget, |carrier| {
        tables.push(carrier, &cancel, |map| {
            for stream in map.streams.as_slice() {
                for descriptor in stream.descriptors() {
                    let descriptor = descriptor.map_err(|e| e.to_string())?;
                    let mut words = vec![
                        map.header.extension.into(),
                        map.header.version.into(),
                        stream.pid.into(),
                        descriptor.tag.into(),
                    ];
                    match descriptor.tag {
                        0x28 => {
                            let avc = descriptor.avc_video().map_err(|e| e.to_string())?;
                            words.push(avc.profile_idc.into());
                            words.extend(avc.constraint_set_flags.map(u64::from));
                            words.extend::<[u64; 5]>([
                                avc.compatible_flags.into(),
                                avc.level_idc.into(),
                                avc.still_present.into(),
                                avc.picture_24h.into(),
                                avc.frame_packing_sei_not_present.into(),
                            ]);
                        }
                        0x38 => {
                            let hevc = descriptor.hevc_video().map_err(|e| e.to_string())?;
                            words.extend([
                                hevc.profile_space.into(),
                                hevc.high_tier.into(),
                                hevc.profile_idc.into(),
                                hevc.profile_compatibility_indication.into(),
                                hevc.progressive_source.into(),
                                hevc.interlaced_source.into(),
                                hevc.non_packed_constraint.into(),
                                hevc.frame_only_constraint.into(),
                                hevc.copied_44bits,
                                hevc.level_idc.into(),
                                hevc.still_present.into(),
                                hevc.picture_24h.into(),
                                hevc.sub_pic_hrd_params_not_present.into(),
                                hevc.hdr_wcg_idc.into(),
                                hevc.temporal_ids.is_some().into(),
                            ]);
                            if let Some((min, max)) = hevc.temporal_ids {
                                words.extend([u64::from(min), u64::from(max)]);
                            }
                        }
                        _ => continue,
                    }
                    record(work, out, 3, &words, &[])?;
                }
            }
            Ok(())
        })
    })?;
    record(work, out, if clean { 7 } else { 8 }, &[], &[])
}
