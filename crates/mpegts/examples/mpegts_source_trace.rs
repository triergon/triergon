//! Source trace with explicit development views. JSON/CLI/I/O are outside timing.
//! Parent owns compilation/execution; no native library is linked here.
#![forbid(unsafe_code)]
use std::{
    env,
    fs::File,
    io::{self, Read, Write},
    sync::atomic::AtomicBool,
};
use triergon_core::{
    Budget, Bytes, InputCursor, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step,
    WorkBudget,
};
use triergon_mpegts::clock::NativePesClockV1;
use triergon_mpegts::psi::{
    DescriptorNamespace, DvbNamespaceEvidence, DvbNativeViewProfile, ElementaryStream,
};
use triergon_mpegts::{
    CompactTraceEvent, PacketFormat, ParseLimits, Pes, TraceEvent, TransportStream,
};

pub(crate) fn hex(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    output.write_all(b"\"")?;
    for byte in bytes {
        write!(output, "{byte:02x}")?;
    }
    output.write_all(b"\"")
}
pub(crate) fn optional(output: &mut impl Write, value: Option<u64>) -> io::Result<()> {
    match value {
        Some(value) => write!(output, "{value}"),
        None => output.write_all(b"null"),
    }
}
pub(crate) fn signed_optional(output: &mut impl Write, value: Option<i128>) -> io::Result<()> {
    match value {
        Some(value) => write!(output, "{value}"),
        None => output.write_all(b"null"),
    }
}
fn native_clock_view(
    output: &mut impl Write,
    clock: &mut NativePesClockV1,
    source: &Pes,
    quarantined: bool,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let view = clock
        .project(source, &mut WorkBudget::new(1), cancel)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("native clock view yielded with its fixed work grant"))?;
    write!(
        output,
        ",\"native_clock_v1\":{{\"profile\":\"{:?}\",\"operation\":\"{}\",\"source_pin\":\"{}\",\"worker_sha256\":\"{}\",\"flags\":{},\"correct_ts_overflow\":{},\"find_stream_info\":{},\"program\":{},\"pid\":{},\"source_clock_epoch\":{},\"source_configuration_epoch\":{},\"source_clock_quarantined\":{quarantined},\"source_pts_raw33\":",
        view.profile,
        view.profile.operation(),
        view.profile.source_pin(),
        view.profile.worker_sha256(),
        view.profile.flags(),
        view.profile.default_overflow_correction(),
        view.profile.find_stream_info(),
        view.program,
        view.pid,
        view.source_epochs.clock,
        view.source_epochs.configuration
    )?;
    optional(output, view.source_pts_raw33)?;
    output.write_all(b",\"source_dts_raw33\":")?;
    optional(output, view.source_dts_raw33)?;
    output.write_all(b",\"pts90\":")?;
    signed_optional(output, view.pts90)?;
    output.write_all(b",\"dts90\":")?;
    signed_optional(output, view.dts90)?;
    write!(
        output,
        ",\"pts_origin\":\"{:?}\",\"dts_origin\":\"{:?}\",\"first_reference_raw90\":",
        view.pts_origin, view.dts_origin
    )?;
    optional(
        output,
        view.origin.map(|origin| origin.first_reference_raw90),
    )?;
    output.write_all(b",\"interval_origin90\":")?;
    signed_optional(output, view.origin.map(|origin| origin.interval_origin90))?;
    output.write_all(b",\"qualified\":false}")
}
fn dvb_signalling(output: &mut impl Write, stream: &ElementaryStream) -> io::Result<()> {
    output.write_all(b",\"dvb_signalling\":")?;
    let Some(facts) = stream.dvb_subtitling_signalling() else {
        return output.write_all(b"null");
    };
    write!(
        output,
        "{{\"namespace_evidence\":\"{:?}\",\"codec_identification\":\"{:?}\",\"descriptor_count\":{},\"service_count\":{},\"audio_conflict\":{},\"descriptors\":[",
        stream.descriptor_namespace(),
        stream.codec_identification(),
        facts.descriptors,
        facts.services,
        facts.audio_conflict
    )?;
    let mut first = true;
    for (ordinal, descriptor) in stream.descriptors().enumerate() {
        let descriptor = descriptor.map_err(io::Error::other)?;
        if descriptor.tag != 0x59 {
            continue;
        }
        let view = descriptor.dvb_subtitling().map_err(io::Error::other)?;
        if !first {
            output.write_all(b",")?;
        }
        first = false;
        write!(output, "{{\"tlv_ordinal\":{ordinal},\"raw_hex\":")?;
        hex(output, view.source)?;
        output.write_all(b",\"services\":[")?;
        for (index, service) in view.services().enumerate() {
            if index != 0 {
                output.write_all(b",")?;
            }
            output.write_all(b"{\"raw_hex\":")?;
            hex(output, service.source)?;
            output.write_all(b",\"language_hex\":")?;
            hex(output, &service.language_code)?;
            write!(
                output,
                ",\"subtitling_type\":{},\"type_in_dvb_range\":{},\"composition_page_id\":{},\"ancillary_page_id\":{}}}",
                service.subtitling_type,
                service.has_dvb_subtitling_type(),
                service.composition_page_id,
                service.ancillary_page_id
            )?;
        }
        output.write_all(b"]}")?;
    }
    output.write_all(b"],\"native_d0_single_service_v1\":")?;
    match stream.native_dvb_single_service_view(DvbNativeViewProfile::D0SingleServiceV1) {
        Ok(view) => {
            write!(
                output,
                "{{\"source_pin\":\"{}\",\"codecpar_hex\":",
                view.profile.source_pin()
            )?;
            hex(output, &view.bytes)?;
            output.write_all(b",\"qualified\":false}")?;
        }
        Err(error) => {
            output.write_all(b"{\"unsupported\":")?;
            json_string(output, &error.to_string())?;
            output.write_all(b"}")?;
        }
    }
    output.write_all(b"}")
}
pub(crate) fn event(
    output: &mut impl Write,
    event: TraceEvent,
    native_clock: Option<&mut NativePesClockV1>,
    quarantined: bool,
    cancel: &AtomicBool,
) -> io::Result<bool> {
    match event {
        TraceEvent::Transport(packet) => {
            write!(
                output,
                "{{\"kind\":\"transport\",\"source\":{},\"offset\":{},\"pid\":{},\"pusi\":{},\"cc\":{},\"tei\":{},\"scrambling\":{},\"prefix_hex\":",
                packet.source_id.map_or(0, |id| id.0),
                packet.source_offset,
                packet.pid,
                packet.payload_unit_start,
                packet.continuity_counter,
                packet.transport_error,
                packet.scrambling
            )?;
            if let Some(prefix) = packet.prefix.as_ref() {
                hex(output, prefix.as_ref())?;
            } else {
                output.write_all(b"null")?;
            }
            if let Some(pcr) = packet.adaptation.as_ref().and_then(|field| field.pcr) {
                write!(
                    output,
                    ",\"pcr_base\":{},\"pcr_extension\":{}",
                    pcr.base, pcr.extension
                )?;
            }
            output.write_all(b",\"raw_hex\":")?;
            hex(output, packet.source.as_ref())?;
            output.write_all(b"}")?;
        }
        TraceEvent::Continuity { pid, outcome } => write!(
            output,
            "{{\"kind\":\"continuity\",\"pid\":{pid},\"outcome\":\"{outcome:?}\"}}"
        )?,
        TraceEvent::Section(section) => {
            write!(
                output,
                "{{\"kind\":\"section\",\"pid\":{},\"raw_hex\":",
                section.pid()
            )?;
            hex(output, section.bytes().as_ref())?;
            output.write_all(b",\"crc_remainder\":")?;
            optional(output, section.crc_remainder().map(u64::from))?;
            output.write_all(b",\"source_ranges\":[")?;
            for (index, range) in section.source_ranges().iter().enumerate() {
                if index != 0 {
                    output.write_all(b",")?;
                }
                write!(
                    output,
                    "{{\"source\":{},\"offset\":{},\"bytes\":{}}}",
                    range.entity.0, range.offset, range.bytes
                )?;
            }
            output.write_all(b"]}")?;
        }
        TraceEvent::Pat(table) => {
            write!(
                output,
                "{{\"kind\":\"pat\",\"tsid\":{},\"version\":{},\"programs\":[",
                table.header.extension, table.header.version
            )?;
            for (index, entry) in table.programs.as_slice().iter().enumerate() {
                if index != 0 {
                    output.write_all(b",")?;
                }
                write!(
                    output,
                    "{{\"program\":{},\"pid\":{},\"network\":{}}}",
                    entry.program_number,
                    entry.pid,
                    entry.is_network_pid()
                )?;
            }
            output.write_all(b"]}")?;
        }
        TraceEvent::ProgramMap {
            map,
            active,
            configuration_epoch,
        } => {
            write!(
                output,
                "{{\"kind\":\"pmt\",\"program\":{},\"version\":{},\"active\":{active},\"configuration_epoch\":{configuration_epoch},\"pcr_pid\":{},\"program_descriptors_hex\":",
                map.header.extension, map.header.version, map.pcr_pid
            )?;
            hex(output, map.program_descriptors.as_ref())?;
            if matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_)) {
                write!(
                    output,
                    ",\"descriptor_namespace\":\"{:?}\"",
                    map.descriptor_namespace
                )?;
            }
            output.write_all(b",\"streams\":[")?;
            for (index, stream) in map.streams.as_slice().iter().enumerate() {
                if index != 0 {
                    output.write_all(b",")?;
                }
                write!(
                    output,
                    "{{\"pid\":{},\"stream_type\":{},\"descriptors_hex\":",
                    stream.pid, stream.stream_type
                )?;
                hex(output, stream.descriptor_bytes.as_ref())?;
                // Preserve the original default trace schema. Extra source/native
                // fields belong only to the explicit DVB diagnostic operation.
                if matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_)) {
                    dvb_signalling(output, stream)?;
                }
                output.write_all(b"}")?;
            }
            output.write_all(b"]}")?;
        }
        TraceEvent::Pes(pes) => {
            write!(
                output,
                "{{\"kind\":\"pes\",\"pid\":{},\"elementary_offset\":{},\"clock_epoch\":{},\"configuration_epoch\":{},\"stream_id\":{},\"pts_raw33\":",
                pes.pid,
                pes.elementary_offset,
                pes.epochs.clock,
                pes.epochs.configuration,
                pes.header.stream_id
            )?;
            optional(output, pes.header.pts_raw33)?;
            output.write_all(b",\"dts_raw33\":")?;
            optional(output, pes.header.dts_raw33)?;
            output.write_all(b",\"header_hex\":")?;
            hex(output, pes.header.as_bytes())?;
            write!(
                output,
                ",\"payload_bytes\":{},\"payload_hex\":\"",
                pes.payload_len
            )?;
            for part in pes.payload() {
                for byte in part {
                    write!(output, "{byte:02x}")?;
                }
            }
            output.write_all(b"\",\"source_ranges\":[")?;
            for (index, range) in pes.source_ranges().enumerate() {
                if index != 0 {
                    output.write_all(b",")?;
                }
                write!(
                    output,
                    "{{\"source\":{},\"offset\":{},\"bytes\":{}}}",
                    range.entity.0, range.offset, range.bytes
                )?;
            }
            output.write_all(b"],\"header_source_ranges\":[")?;
            for (index, range) in pes.header_source_ranges.as_slice().iter().enumerate() {
                if index != 0 {
                    output.write_all(b",")?;
                }
                write!(
                    output,
                    "{{\"source\":{},\"offset\":{},\"bytes\":{}}}",
                    range.entity.0, range.offset, range.bytes
                )?;
            }
            write!(output, "],\"byte_run_epoch\":{}", pes.byte_run_epoch)?;
            if let Some(clock) = native_clock {
                native_clock_view(output, clock, &pes, quarantined, cancel)?;
            }
            output.write_all(b"}")?;
        }
        TraceEvent::Timeline {
            program,
            clock_epoch,
        } => write!(
            output,
            "{{\"kind\":\"timeline\",\"program\":{program},\"clock_epoch\":{clock_epoch}}}"
        )?,
        TraceEvent::Discontinuity { epochs } => write!(
            output,
            "{{\"kind\":\"discontinuity\",\"clock_epoch\":{},\"configuration_epoch\":{}}}",
            epochs.clock, epochs.configuration
        )?,
        TraceEvent::Diagnostic(fault) => {
            // Error Debug contains only static owned-code strings/numbers. Escape
            // its quote/backslash/control characters through a streaming writer.
            write!(
                output,
                "{{\"kind\":\"diagnostic\",\"layer\":\"{:?}\",\"source\":{},\"offset\":{},\"pid\":",
                fault.kind, fault.location.source.0, fault.location.offset
            )?;
            optional(output, fault.pid.map(u64::from))?;
            output.write_all(b",\"error\":")?;
            let message = format!("{:?}", fault.error);
            json_string(output, &message)?;
            output.write_all(b"}")?;
        }
        TraceEvent::Finished => {
            output.write_all(b"{\"kind\":\"finished\"}\n")?;
            return Ok(true);
        }
    }
    output.write_all(b"\n")?;
    Ok(false)
}
pub(crate) fn json_string(output: &mut impl Write, text: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    for character in text.chars() {
        match character {
            '"' => output.write_all(b"\\\"")?,
            '\\' => output.write_all(b"\\\\")?,
            '\n' => output.write_all(b"\\n")?,
            '\r' => output.write_all(b"\\r")?,
            '\t' => output.write_all(b"\\t")?,
            c if c < ' ' => write!(output, "\\u{:04x}", u32::from(c))?,
            c => write!(output, "{c}")?,
        }
    }
    output.write_all(b"\"")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let format = match arguments.next().as_deref() {
        Some("188") => PacketFormat::Ts188,
        Some("192") => PacketFormat::M2ts192,
        _ => {
            return Err(
                "usage: mpegts_source_trace 188|192 PROGRAM [--dvb] [--native-clock-v1] [--media-entities-v1] [--compact] FILE... [--discontinuity FILE...]"
                    .into(),
            );
        }
    };
    let program: u16 = arguments
        .next()
        .ok_or("missing selected program")?
        .parse()?;
    let mut descriptor_namespace = DescriptorNamespace::Unspecified;
    let mut native_clock_enabled = false;
    let mut media_entities_enabled = false;
    let mut compact = false;
    let mut path = loop {
        let argument = arguments.next().ok_or("missing source entity")?;
        match argument.as_str() {
            "--dvb" => {
                descriptor_namespace =
                    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile)
            }
            "--native-clock-v1" => native_clock_enabled = true,
            "--media-entities-v1" => media_entities_enabled = true,
            "--compact" => compact = true,
            _ => break argument,
        }
    };
    let mut native_clock = if native_clock_enabled {
        Some(NativePesClockV1::new(program)?)
    } else {
        None
    };
    let budget = Budget::new(64 * 1024 * 1024)?;
    let cancel = AtomicBool::new(false);
    let mut source_id = 1_u64;
    let mut file = File::open(&path)?;
    let capabilities = SourceCapabilities {
        length: Some(file.metadata()?.len()),
        random_access: false,
    };
    let mut input = InputCursor::new(&budget, SourceId(source_id), capabilities, 16 * 1024)?;
    let mut demux = TransportStream::new_with_namespace(
        budget.clone(),
        format,
        ParseLimits::default(),
        program,
        descriptor_namespace,
    )?;
    let mut buffer = [0_u8; 16 * 1024];
    let stdout = io::stdout();
    let mut output = stdout.lock();
    loop {
        let mut work = WorkBudget::new(4096);
        let step = if compact {
            demux.poll_compact(&mut input, &mut work, &cancel)?
        } else {
            match demux.poll(&mut input, &mut work, &cancel)? {
                Step::Ready(value) => Step::Ready(CompactTraceEvent::Event(value)),
                Step::NeedInput(request) => Step::NeedInput(request),
                Step::End(location) => Step::End(location),
                Step::Yield => Step::Yield,
            }
        };
        match step {
            Step::Ready(CompactTraceEvent::NullRun(run)) => {
                for index in 0..run.len() {
                    event(
                        &mut output,
                        TraceEvent::Transport(run.packet(index)?),
                        native_clock.as_mut(),
                        demux.clock_is_quarantined(),
                        &cancel,
                    )?;
                }
            }
            Step::Ready(CompactTraceEvent::Event(value)) => {
                if event(
                    &mut output,
                    value,
                    native_clock.as_mut(),
                    demux.clock_is_quarantined(),
                    &cancel,
                )? {
                    break;
                }
            }
            Step::Yield => {}
            Step::NeedInput(request) => {
                let count = match file.read(&mut buffer[..request.maximum]) {
                    Ok(count) => count,
                    Err(error) => {
                        input.fail(request.source, request.token, SourceFailure::Transport)?;
                        return Err(error.into());
                    }
                };
                if count == 0 {
                    input.finish(request.source, request.token)?;
                } else {
                    input.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        Bytes::copy_from_slice(&budget, &buffer[..count])?,
                    )?)?;
                }
            }
            Step::End(_) => {
                if let Some(next) = arguments.next() {
                    let discontinuity = next == "--discontinuity";
                    path = if discontinuity {
                        arguments
                            .next()
                            .ok_or("missing entity after discontinuity")?
                    } else {
                        next
                    };
                    if discontinuity {
                        demux.discontinuity()?;
                        if media_entities_enabled && native_clock_enabled {
                            native_clock = Some(NativePesClockV1::new(program)?);
                        }
                    }
                    file = File::open(&path)?;
                    source_id = source_id.checked_add(1).ok_or("source id overflow")?;
                    input.continue_source(
                        SourceId(source_id),
                        SourceCapabilities {
                            length: Some(file.metadata()?.len()),
                            random_access: false,
                        },
                    )?;
                } else {
                    demux.end_of_source()?;
                }
            }
        }
    }
    Ok(())
}
