use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_harness_protocol::write_hex;
use triergon_mpegts::ParseLimits;
use triergon_mpegts::mux::{MuxConfig, Muxer};
use triergon_mpegts::pes::Pes;
use triergon_mpegts::psi::{DescriptorNamespace, PsiCrc32};
use triergon_mpegts::stream::{TraceEvent, TransportStream};

use super::protocol::{Operation, Work};

const TS_PACKET: u64 = 188;
const WINDOW: usize = 4096;

struct Picture<'a> {
    pts: i64,
    dts: i64,
    key: bool,
    data: &'a [u8],
}

fn pictures(mut input: &[u8]) -> Option<Vec<Picture<'_>>> {
    let mut pictures = Vec::new();
    while !input.is_empty() {
        let (header, rest) = input.split_first_chunk::<21>()?;
        let pts = i64::from_be_bytes(header[..8].try_into().ok()?);
        let dts = i64::from_be_bytes(header[8..16].try_into().ok()?);
        let length = u32::from_be_bytes(header[17..].try_into().ok()?) as usize;
        let (data, next) = rest.split_at_checked(length)?;
        if !(0..1 << 33).contains(&pts)
            || !(0..1 << 33).contains(&dts)
            || header[16] > 1
            || data.is_empty()
        {
            return None;
        }
        pictures.push(Picture {
            pts,
            dts,
            key: header[16] == 1,
            data,
        });
        input = next;
    }
    (!pictures.is_empty()).then_some(pictures)
}

fn config(args: &[OsString]) -> Option<MuxConfig> {
    if args == ["default"] {
        return Some(MuxConfig::default());
    }
    let [
        tsid,
        network,
        program,
        pmt_pid,
        video_pid,
        stream_type,
        provider,
        service,
    ] = args
    else {
        return None;
    };
    Some(MuxConfig {
        transport_stream_id: tsid.to_str()?.parse().ok()?,
        original_network_id: network.to_str()?.parse().ok()?,
        program_number: program.to_str()?.parse().ok()?,
        pmt_pid: pmt_pid.to_str()?.parse().ok()?,
        video_pid: video_pid.to_str()?.parse().ok()?,
        stream_type: stream_type.to_str()?.parse().ok()?,
        provider_name: provider.to_str()?.into(),
        service_name: service.to_str()?.into(),
    })
}

fn mux(config: &MuxConfig, pictures: &[Picture<'_>]) -> triergon_core::Result<Vec<u8>> {
    let mut muxer = Muxer::new(config)?;
    let mut ts = Vec::new();
    for picture in pictures {
        muxer.write_picture(picture.data, picture.pts, picture.dts, picture.key, &mut ts);
    }
    Ok(ts)
}

pub(super) fn operation<'a>(
    _: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let (export, arguments) = match arguments.split_first() {
        Some((arg, rest)) if arg == "--emit-ts" => {
            let (path, args) = rest.split_first().ok_or("missing TS output path")?;
            (Some(path.clone()), args)
        }
        _ => (None, arguments),
    };
    let config = config(arguments)
        .ok_or("expected default or TSID ONID PROGRAM PMT VIDEO TYPE PROVIDER SERVICE")?;
    let pictures = pictures(input).ok_or("invalid mux picture input")?;
    Ok(Operation {
        name: "ts-mux-readback",
        run: Box::new(move |work, trace| {
            let mut rows = Rows { work, trace };
            match mux(&config, &pictures) {
                Ok(ts) => {
                    if let Some(path) = &export {
                        std::fs::write(path, ts).map_err(|error| error.to_string())
                    } else {
                        read_back(&ts, &mut rows)
                    }
                }
                Err(Error::Unsupported(_)) => rows.record(8, &[], &[]),
                Err(error) => Err(error.to_string()),
            }
        }),
    })
}

struct Rows<'work, 'trace> {
    work: &'work mut Work,
    trace: Option<&'trace mut dyn Write>,
}

impl Rows<'_, '_> {
    fn record(&mut self, kind: u64, words: &[u64], data: &[u8]) -> Result<(), String> {
        self.work.record(kind, words, data);
        if let Some(trace) = self.trace.as_mut() {
            let mut write_row = || -> std::io::Result<()> {
                write!(trace, "{kind}")?;
                for word in words {
                    write!(trace, " {word}")?;
                }
                if !data.is_empty() {
                    write!(trace, " ")?;
                    write_hex(trace, data)?;
                }
                writeln!(trace)
            };
            write_row().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn packet(&mut self, pes: &Pes, starts: &mut BTreeMap<u64, bool>) -> Result<(), String> {
        let start = pes
            .header_source_ranges
            .as_slice()
            .first()
            .ok_or("missing PES header range")?
            .offset;
        let offset = start / TS_PACKET * TS_PACKET;
        let key = starts.remove(&offset).ok_or("missing PES start carrier")?;
        let pts = pes.header.pts_raw33;
        let dts = pes.header.dts_raw33;
        let data: Vec<_> = pes.payload().flatten().copied().collect();
        self.record(
            1,
            &[
                u64::from(pes.pid),
                u64::from(pts.is_some()),
                pts.unwrap_or(0),
                u64::from(dts.is_some()),
                dts.unwrap_or(0),
                u64::from(key),
                offset,
                data.len() as u64,
            ],
            &data,
        )
    }
}

// ETSI EN 300 468, actual-TS SDT (5.2.3) and service descriptor (6.2.33).
// This writer emits single-packet sections with printable ASCII names.
fn service_names(payload: &[u8]) -> Option<(u16, &[u8], &[u8])> {
    let (&pointer, payload) = payload.split_first()?;
    let section = payload.get(usize::from(pointer)..)?;
    let [0x42, hi, lo, ..] = *section else {
        return None;
    };
    let length = (usize::from(hi & 0x0f) << 8) | usize::from(lo);
    let section = section.get(..3 + length)?;
    if section.len() < 25 || PsiCrc32::of(section) != 0 {
        return None;
    }
    let service_id = u16::from_be_bytes([section[11], section[12]]);
    let descriptors_len = (usize::from(section[14] & 0x0f) << 8) | usize::from(section[15]);
    let descriptors = section.get(16..16 + descriptors_len)?;
    let [0x48, length, _, provider_len, rest @ ..] = descriptors else {
        return None;
    };
    if usize::from(*length) + 2 != descriptors.len() || section.len() != 20 + descriptors.len() {
        return None;
    }
    let (provider, remainder) = rest.split_at_checked(usize::from(*provider_len))?;
    let (&service_len, service) = remainder.split_first()?;
    (service.len() == usize::from(service_len)).then_some((service_id, provider, service))
}

fn read_back(ts: &[u8], rows: &mut Rows<'_, '_>) -> Result<(), String> {
    let fail = |error: Error| error.to_string();
    let budget = Budget::new(16 * 1024 * 1024).map_err(fail)?;
    let raw = Bytes::copy_from_slice(&budget, ts).map_err(fail)?;
    let mut input = InputCursor::new(
        &budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(ts.len() as u64),
            random_access: false,
        },
        WINDOW,
    )
    .map_err(fail)?;
    let mut stream = TransportStream::new_single_program_segment(
        budget,
        ParseLimits::default(),
        DescriptorNamespace::Unspecified,
    )
    .map_err(fail)?;
    let mut starts = BTreeMap::new();
    let mut program = None;
    let mut names = None;
    let cancel = AtomicBool::new(false);
    loop {
        match stream
            .poll(&mut input, &mut WorkBudget::new(65536), &cancel)
            .map_err(fail)?
        {
            Step::Ready(TraceEvent::Transport(packet)) if packet.payload_unit_start => {
                if packet.pid == 0x11 {
                    names = Some(
                        service_names(
                            packet
                                .payload
                                .as_ref()
                                .ok_or("missing SDT payload")?
                                .as_ref(),
                        )
                        .map(|(id, provider, service)| (id, provider.to_vec(), service.to_vec()))
                        .ok_or("invalid mux SDT")?,
                    );
                } else if stream
                    .active_program()
                    .is_some_and(|map| map.streams().any(|stream| stream.pid() == packet.pid))
                {
                    let key = packet
                        .adaptation
                        .is_some_and(|field| field.random_access_indicator);
                    starts.insert(packet.source_offset, key);
                }
            }
            Step::Ready(TraceEvent::Pat(pat)) => {
                program = pat.programs.as_slice().first().copied();
            }
            Step::Ready(TraceEvent::Pes(pes)) => rows.packet(&pes, &mut starts)?,
            Step::Ready(TraceEvent::Diagnostic(fault)) => {
                return Err(format!(
                    "mux read-back at {}: {}",
                    fault.location.offset, fault.error
                ));
            }
            Step::Ready(TraceEvent::Finished) => break,
            Step::NeedInput(request) => {
                let start = request.offset as usize;
                let end = raw.len().min(start.saturating_add(request.maximum));
                if start < end {
                    let chunk = raw.slice(start..end).map_err(fail)?;
                    input
                        .provide_owned(
                            InputWindow::new(request.source, request.token, request.offset, chunk)
                                .map_err(fail)?,
                        )
                        .map_err(fail)?;
                } else {
                    input.finish(request.source, request.token).map_err(fail)?;
                }
            }
            Step::End(_) => stream.end_of_source().map_err(fail)?,
            Step::Yield | Step::Ready(_) => {}
            Step::Backpressure(never) => match never {},
        }
    }
    if !starts.is_empty() {
        return Err("unread PES start carriers".into());
    }
    let program = program.ok_or("missing mux PAT")?;
    let map = stream.active_program().ok_or("missing mux PMT")?;
    let (service_id, provider, service) = names.ok_or("missing mux SDT")?;
    if service_id != program.program_number {
        return Err("mux SDT service is absent from PAT".into());
    }
    rows.record(
        5,
        &[0, program.program_number.into(), program.pid.into()],
        &[],
    )?;
    for stream in map.streams() {
        rows.record(
            5,
            &[1, stream.pid().into(), stream.stream_type().into()],
            &[],
        )?;
    }
    rows.record(5, &[2, 1, provider.len() as u64], &provider)?;
    rows.record(5, &[3, 1, service.len() as u64], &service)?;
    rows.record(7, &[], &[])
}
