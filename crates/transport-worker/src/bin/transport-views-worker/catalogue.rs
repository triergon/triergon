use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::container::CodecId;
use triergon_core::{Budget, Error, Step, WorkBudget};
use triergon_mpegts::compat::pes::SourceContext;
use triergon_mpegts::psi::{
    DescriptorNamespace, DvbNamespaceEvidence, ElementaryStream, PatCollector, PatReader, PatTable,
    PmtReader, ProgramMap, Section, SectionAssembler, TableStep,
};
use triergon_mpegts::reference_trace::catalogue::{Catalogue, PsiCompatibilityPolicies};
use triergon_mpegts::stream::TransportStream;
use triergon_mpegts::transport::{Continuity, ContinuityOutcome, PacketFormat, TransportPacket};
use triergon_mpegts::{AssemblyStep, ParseLimits};

use super::packets::{self, Carrier, GRANT, SOURCE, record};
use super::protocol::{Operation, Work};

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

type CatalogueRun = fn(&[u8], Case, &mut Work, &mut Option<&mut dyn Write>) -> Result<(), String>;

fn program_row(
    work: &mut Work,
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
    work: &mut Work,
    out: &mut Option<&mut dyn Write>,
    kind: u64,
    bytes: &[u8],
) -> Result<(), String> {
    record(work, out, 5, &[kind, bytes.len() as u64], bytes)
}

struct StreamRow<'a> {
    pid: u16,
    stream_type: u8,
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
        work: &mut Work,
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
                self.stream_type.into(),
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
    work: &mut Work,
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
    let result = loop {
        match transport.poll(&mut input, &mut WorkBudget::new(GRANT), &cancel) {
            Ok(Step::Ready(event)) => {
                let result = if compatible {
                    let context = SourceContext {
                        source_event_ordinal: ordinal,
                        epochs: transport.source_epochs(),
                        byte_run_epoch: None,
                    };
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
    for evidence in view.psi_compatibility_evidence() {
        let admitted = evidence.record();
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
                    stream_type: stream.stream_type,
                    codec: stream.codec().name(),
                    language: stream.language.as_slice(),
                    hearing_impaired: false,
                    visual_impaired: false,
                    dvb: None,
                }
                .write(program.program, work, out)?;
            }
        }
    }
    record(work, out, 7, &[], &[])
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
    work: &mut Work,
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
    work: &mut Work,
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
        stream_type: stream.stream_type,
        codec,
        language: &language,
        hearing_impaired,
        visual_impaired,
        dvb: (codec == "dvb_subtitle").then_some(pages.as_slice()),
    }
    .write(program, work, out)
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
    work: &mut Work,
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
