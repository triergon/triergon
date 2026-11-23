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

use triergon_harness_runner::{Operation, Totals};

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
    let mut wire = MuxWire::default();
    for picture in pictures {
        let begin = ts.len();
        muxer.write_picture(picture.data, picture.pts, picture.dts, picture.key, &mut ts);
        wire.check_picture(config, picture, &ts[begin..])?;
    }
    Ok(ts)
}

#[derive(Default)]
struct MuxWire {
    sdt: Option<i64>,
    pat_pmt: Option<i64>,
    previous_key: bool,
    next_cc: [u8; 4],
}

impl MuxWire {
    fn check_picture(
        &mut self,
        config: &MuxConfig,
        picture: &Picture<'_>,
        bytes: &[u8],
    ) -> triergon_core::Result<()> {
        // A 90 kHz clock advances 9,000 ticks in 100 ms and 45,000 in 500 ms.
        let sdt = self.sdt.is_none_or(|last| picture.dts >= last + 45_000);
        let pat_pmt = (picture.key && !self.previous_key)
            || self.pat_pmt.is_none_or(|last| picture.dts >= last + 9_000);
        let (packets, trailing) = bytes.as_chunks::<188>();
        if !trailing.is_empty() {
            return Err(Error::Invalid("mux appended an incomplete carrier"));
        }
        let mut packets = packets.iter();
        let tables = [
            (sdt, 0x11, 0x42),
            (pat_pmt, 0, 0),
            (pat_pmt, config.pmt_pid, 2),
        ];
        for (index, (due, pid, table_id)) in tables.into_iter().enumerate() {
            if due {
                let packet = packets.next().ok_or(Error::Truncated)?;
                self.check_header(packet, pid, index, true)?;
                if packet[3] & 0x30 != 0x10 || packet[4..6] != [0, table_id] {
                    return Err(Error::Invalid("mux table pointer or ID"));
                }
                // The section starts after the pointer field and continues in the
                // PID's next carriers until its declared length (EN 300 468 §5.1.2).
                let total = 3 + usize::from(u16::from_be_bytes([packet[6] & 15, packet[7]]));
                let mut section = packet[5..].to_vec();
                let mut last = packet;
                while section.len() < total {
                    let next = packets.next().ok_or(Error::Truncated)?;
                    self.check_header(next, pid, index, false)?;
                    if next[3] & 0x30 != 0x10 {
                        return Err(Error::Invalid("mux table continuation carrier"));
                    }
                    section.extend_from_slice(&next[4..]);
                    last = next;
                }
                let stuffing = section.split_off(total);
                if wire_crc_remainder(&section) != 0
                    || stuffing.iter().any(|&byte| byte != 0xff)
                    || stuffing.len() >= last.len() - 4
                {
                    return Err(Error::Invalid("mux table CRC or stuffing"));
                }
            }
        }
        let mut data = picture.data;
        let carriers = packets.len();
        for (index, packet) in packets.enumerate() {
            let first = index == 0;
            self.check_header(packet, config.video_pid, 3, first)?;
            let adaptation = packet[3] & 0x20 != 0;
            let payload = if adaptation {
                let end = 5 + usize::from(packet[4]);
                let field = packet.get(5..end).ok_or(Error::Truncated)?;
                let stuffing = if first {
                    field.get(7..).ok_or(Error::Truncated)?
                } else if let Some((&flags, rest)) = field.split_first() {
                    if flags != 0 {
                        return Err(Error::Invalid("mux continuation adaptation flags"));
                    }
                    rest
                } else {
                    field
                };
                if ((!first || !stuffing.is_empty()) && index + 1 != carriers)
                    || stuffing.iter().any(|&byte| byte != 0xff)
                {
                    return Err(Error::Invalid("mux video stuffing location or bytes"));
                }
                &packet[end..]
            } else {
                &packet[4..]
            };
            let payload = if first {
                let expected_flags = if picture.key { 0x50 } else { 0x10 };
                let base = u64::from_be_bytes([
                    0, 0, 0, packet[6], packet[7], packet[8], packet[9], packet[10],
                ]) >> 7;
                if !adaptation
                    || packet[4] < 7
                    || packet[5] != expected_flags
                    || base != picture.dts as u64
                    || packet[10] & 0x7f != 0x7e
                    || packet[11] != 0
                {
                    return Err(Error::Invalid("mux first carrier PCR or random access"));
                }
                checked_pes_payload(payload, picture)?
            } else {
                payload
            };
            data = data
                .strip_prefix(payload)
                .ok_or(Error::Invalid("mux changed coded picture bytes"))?;
        }
        if !data.is_empty() {
            return Err(Error::Invalid("mux dropped coded picture bytes"));
        }
        if sdt {
            self.sdt = Some(self.sdt.unwrap_or(picture.dts).max(picture.dts));
        }
        if pat_pmt {
            self.pat_pmt = Some(self.pat_pmt.unwrap_or(picture.dts).max(picture.dts));
        }
        self.previous_key = picture.key;
        Ok(())
    }

    fn check_header(
        &mut self,
        packet: &[u8; 188],
        pid: u16,
        index: usize,
        start: bool,
    ) -> triergon_core::Result<()> {
        let start_bits = if start { 0x40 } else { 0 };
        let actual = u16::from_be_bytes([packet[1] & 31, packet[2]]);
        if packet[0] != 0x47
            || packet[1] & 0xe0 != start_bits
            || actual != pid
            || packet[3] & 0xd0 != 0x10
            || packet[3] & 15 != self.next_cc[index]
        {
            return Err(Error::Invalid("mux carrier order, PID or continuity"));
        }
        self.next_cc[index] = (self.next_cc[index] + 1) % 16;
        Ok(())
    }
}

fn wire_crc_remainder(section: &[u8]) -> u32 {
    let mut residue = u32::MAX;
    for &byte in section {
        residue ^= u32::from(byte) << 24;
        for _ in 0..8 {
            let high_bit = residue >> 31;
            residue = (residue << 1) ^ (0x04c1_1db7 * high_bit);
        }
    }
    residue
}

fn checked_pes_payload<'a>(
    payload: &'a [u8],
    picture: &Picture<'_>,
) -> triergon_core::Result<&'a [u8]> {
    let (header, data) = payload.split_first_chunk::<19>().ok_or(Error::Truncated)?;
    let pts = &header[9..14];
    let dts = &header[14..19];
    if header[..9] != [0, 0, 1, 0xe0, 0, 0, 0x80, 0xc0, 10]
        || pts[0] >> 4 != 3
        || dts[0] >> 4 != 1
        || [pts[0], pts[2], pts[4], dts[0], dts[2], dts[4]]
            .into_iter()
            .any(|byte| byte & 1 == 0)
        || super::facts::wire_timestamp(pts) != Some(picture.pts as u64)
        || super::facts::wire_timestamp(dts) != Some(picture.dts as u64)
    {
        return Err(Error::Invalid("mux PES extent or encoded clocks"));
    }
    Ok(data)
}

pub(super) fn operation<'a>(
    _: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    // A registered refusal case asks the worker to name its typed unsupported detail.
    let (report_refusal, arguments) = match arguments.split_last() {
        Some((last, rest)) if last == "--report-refusal" => (true, rest),
        _ => (false, arguments),
    };
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
                Err(Error::Unsupported(detail)) => {
                    if report_refusal {
                        rows.work.unsupported = Some(detail);
                    }
                    rows.record(8, &[], &[])
                }
                Err(error) => Err(error.to_string()),
            }
        }),
    })
}

struct Rows<'work, 'trace> {
    work: &'work mut Totals,
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

// ETSI EN 300 468, actual-TS SDT (5.2.3) and service descriptor (6.2.33), from the
// payload of the SDT carriers that hold one section.
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
    (service.len() == usize::from(service_len)).then_some((
        service_id,
        decoded_text(provider),
        decoded_text(service),
    ))
}

/// The characters of an EN 300 468 Annex A text field the writer emits: printable
/// ASCII as it is, or UTF-8 behind the 0x15 selector of Table A.3.
fn decoded_text(field: &[u8]) -> &[u8] {
    field.strip_prefix(&[0x15]).unwrap_or(field)
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
    let mut sdt = Vec::new();
    let cancel = AtomicBool::new(false);
    loop {
        match stream
            .poll(&mut input, &mut WorkBudget::new(65536), &cancel)
            .map_err(fail)?
        {
            // An SDT section can continue in the PID's following carriers.
            Step::Ready(TraceEvent::Transport(packet)) if packet.pid == 0x11 => {
                if packet.payload_unit_start {
                    sdt.clear();
                }
                sdt.extend_from_slice(
                    packet
                        .payload
                        .as_ref()
                        .ok_or("missing SDT payload")?
                        .as_ref(),
                );
                if let Some((id, provider, service)) = service_names(&sdt) {
                    names = Some((id, provider.to_vec(), service.to_vec()));
                }
            }
            Step::Ready(TraceEvent::Transport(packet)) if packet.payload_unit_start => {
                if stream
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
