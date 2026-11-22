use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::{
    Budget, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId, SourceLocation,
    Step, WorkBudget,
};
use triergon_mpegts::stream::{CompactTraceEvent, TraceEvent, TransportStream};
use triergon_mpegts::transport::{
    FrameState, PacketFormat, Pcr, TransportFramer, TransportInspection, TransportInspector,
    TransportObservation, TransportPacket, parse_transport,
};
use triergon_mpegts::{AssemblyStep, ParseLimits};

use super::protocol::{Operation, Work};

pub(super) const SOURCE: SourceId = SourceId(1);
pub(super) const GRANT: u64 = 1 << 20;

pub(super) fn format(value: &OsString) -> Result<PacketFormat, String> {
    match value.to_str() {
        Some("188") => Ok(PacketFormat::Ts188),
        Some("192") => Ok(PacketFormat::M2ts192),
        _ => Err("expected transport packet size 188 or 192".into()),
    }
}

pub(super) fn budget() -> Result<Budget, String> {
    Budget::new(64 * 1024 * 1024).map_err(|e| e.to_string())
}

pub(super) fn input_cursor(
    budget: &Budget,
    size: usize,
    window: usize,
) -> Result<InputCursor, String> {
    InputCursor::new(
        budget,
        SOURCE,
        SourceCapabilities {
            length: Some(size as u64),
            random_access: false,
        },
        window,
    )
    .map_err(|e| e.to_string())
}

pub(super) fn supply(
    input: &mut InputCursor,
    data: &[u8],
    request: InputRequest,
) -> Result<(), String> {
    let offset = usize::try_from(request.offset).map_err(|e| e.to_string())?;
    let remaining = data
        .get(offset..)
        .ok_or("transport requested past the end of input")?;
    if remaining.is_empty() {
        input
            .finish(request.source, request.token)
            .map_err(|e| e.to_string())
    } else {
        let bytes = triergon_core::Bytes::copy_from_slice(
            input.budget(),
            &remaining[..remaining.len().min(request.maximum)],
        )
        .map_err(|e| e.to_string())?;
        let window = InputWindow::new(request.source, request.token, request.offset, bytes)
            .map_err(|e| e.to_string())?;
        input.provide_owned(window).map_err(|e| e.to_string())
    }
}

pub(super) enum Carrier {
    Packet(TransportPacket),
    Rejected(triergon_mpegts::transport::RejectedTransport),
}

pub(super) fn walk(
    data: &[u8],
    format: PacketFormat,
    window: usize,
    budget: &Budget,
    mut carrier: impl FnMut(Carrier) -> Result<(), String>,
) -> Result<bool, String> {
    let cancel = AtomicBool::new(false);
    let mut framer = TransportFramer::new(budget.clone(), format);
    let mut offset = 0;
    for window_bytes in data.chunks(window) {
        let mut rest = window_bytes;
        while !rest.is_empty() {
            let step = framer.push(rest, offset, &mut WorkBudget::new(GRANT), &cancel);
            rest = &rest[step.consumed..];
            offset += step.consumed as u64;
            match step.state {
                FrameState::Packet(packet) => carrier(Carrier::Packet(packet))?,
                FrameState::NeedInput if rest.is_empty() => {}
                FrameState::Error(error) => carrier(Carrier::Rejected(
                    framer.take_rejection().ok_or_else(|| error.to_string())?,
                ))?,
                _ => return Err("transport framer did not advance with a full work grant".into()),
            }
        }
    }
    match framer.finish() {
        Ok(()) => Ok(true),
        Err(triergon_core::Error::Truncated) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn record(
    work: &mut Work,
    out: &mut Option<&mut dyn Write>,
    kind: u64,
    words: &[u64],
    payload: &[u8],
) -> Result<(), String> {
    work.record(kind, words, payload);
    if let Some(out) = out {
        write!(out, "{kind}").map_err(|e| e.to_string())?;
        for word in words {
            write!(out, " {word}").map_err(|e| e.to_string())?;
        }
        if !payload.is_empty() {
            write!(out, " ").map_err(|e| e.to_string())?;
            for byte in payload {
                write!(out, "{byte:02x}").map_err(|e| e.to_string())?;
            }
        }
        writeln!(out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn boundary(
    work: &mut Work,
    out: &mut Option<&mut dyn Write>,
    format: PacketFormat,
    offset: u64,
    carrier: &[u8],
) -> Result<(), String> {
    let prefix = format.prefix_bytes();
    let ts = &carrier[prefix..];
    record(work, out, 6, &[offset + prefix as u64, ts.len() as u64], ts)
}

fn clock(words: &mut Vec<u64>, pcr: Option<Pcr>) -> Result<(), String> {
    words.push(u64::from(pcr.is_some()));
    if let Some(pcr) = pcr {
        let time = pcr.raw_time().map_err(|e| e.to_string())?;
        words.extend([
            pcr.base,
            u64::from(pcr.extension),
            pcr.ticks_27mhz(),
            time.numerator() as u64,
            time.denominator() as u64,
        ]);
    }
    Ok(())
}

fn block(
    work: &mut Work,
    out: &mut Option<&mut dyn Write>,
    offset: u64,
    field: u64,
    value: Option<&[u8]>,
) -> Result<(), String> {
    if let Some(value) = value {
        record(work, out, 3, &[offset, field, value.len() as u64], value)?;
    }
    Ok(())
}

fn fields(
    work: &mut Work,
    out: &mut Option<&mut dyn Write>,
    packet: &TransportPacket,
) -> Result<(), String> {
    let offset = packet.source_offset;
    let field = packet.adaptation.as_ref();
    let control = 2 * u64::from(field.is_some()) + u64::from(packet.payload.is_some());
    let mut words = vec![
        offset,
        packet.source.len() as u64,
        u64::from(
            packet.source.as_ref()[packet.prefix.as_ref().map_or(0, triergon_core::Bytes::len)],
        ),
        packet.transport_error.into(),
        packet.payload_unit_start.into(),
        packet.transport_priority.into(),
        packet.pid.into(),
        packet.scrambling.into(),
        control,
        packet.continuity_counter.into(),
        packet.payload_source_offset,
        packet.payload.as_ref().map_or(0, triergon_core::Bytes::len) as u64,
    ];
    if let Some(field) = field {
        let flags = u64::from(field.discontinuity) * 128
            + u64::from(field.random_access_indicator) * 64
            + u64::from(field.elementary_priority) * 32
            + u64::from(field.pcr.is_some()) * 16
            + u64::from(field.opcr.is_some()) * 8
            + u64::from(field.splice_countdown.is_some()) * 4
            + u64::from(field.private_data.is_some()) * 2
            + u64::from(field.extension.is_some());
        words.extend([(field.source.len() - 1) as u64, flags]);
        clock(&mut words, field.pcr)?;
        if let Some(bits) = field.pcr_reserved_bits() {
            words.push(bits.into());
        }
        clock(&mut words, field.opcr)?;
        if let Some(splice) = field.splice_countdown {
            words.push(triergon_harness_protocol::digest::signed(i64::from(splice)));
        }
    }
    record(work, out, 6, &words, &[])?;
    block(work, out, offset, 0, packet.prefix.as_deref())?;
    if let Some(field) = field {
        block(work, out, offset, 1, field.private_data.as_deref())?;
        block(work, out, offset, 2, field.extension.as_deref())?;
    }
    Ok(())
}

fn compact_fields(
    data: &[u8],
    format: PacketFormat,
    window: usize,
    budget: &Budget,
    work: &mut Work,
    out: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    let mut input = input_cursor(budget, data.len(), window)?;
    let mut stream = TransportStream::new(budget.clone(), format, ParseLimits::default(), 1)
        .map_err(|e| e.to_string())?;
    let cancel = AtomicBool::new(false);
    let mut saw_run = false;
    loop {
        match stream
            .poll_compact(&mut input, &mut WorkBudget::new(GRANT), &cancel)
            .map_err(|e| e.to_string())?
        {
            Step::Ready(CompactTraceEvent::NullRun(run)) => {
                saw_run = true;
                for index in 0..run.len() {
                    let packet = run.packet(index).map_err(|e| e.to_string())?;
                    fields(work, out, &packet)?;
                }
            }
            Step::Ready(CompactTraceEvent::Event(TraceEvent::Transport(packet))) => {
                fields(work, out, &packet)?
            }
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(request) => supply(&mut input, data, request)?,
            Step::End(_) => break,
        }
    }
    if !saw_run {
        return Err("the compact control did not produce a null run".into());
    }
    record(work, out, 7, &[], &[])
}

#[derive(Clone, Copy)]
enum View {
    Boundaries,
    Fields,
    Compact,
}

pub(super) fn operation<'a>(
    _prefix: &str,
    data: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [size, window, tail @ ..] = arguments else {
        return Err("ts-packets requires FORMAT WINDOW [fields|compact]".into());
    };
    let format = format(size)?;
    let window = match window.to_str() {
        Some("1") => 1,
        Some("7") => 7,
        Some("4096") => 4096,
        _ => return Err("expected a transport input window of 1, 7 or 4096".into()),
    };
    let view = match tail {
        [] => View::Boundaries,
        [mode] if mode == "fields" => View::Fields,
        [mode] if mode == "compact" => View::Compact,
        _ => return Err("expected fields or compact packet view".into()),
    };
    Ok(Operation {
        name: "ts-packets",
        run: Box::new(move |work, mut out| {
            let budget = budget()?;
            if matches!(view, View::Compact) {
                return compact_fields(data, format, window, &budget, work, &mut out);
            }
            let clean = walk(data, format, window, &budget, |carrier| {
                match (view, carrier) {
                    (View::Boundaries, Carrier::Packet(packet)) => {
                        boundary(work, &mut out, format, packet.source_offset, &packet.source)
                    }
                    (View::Boundaries, Carrier::Rejected(packet)) => boundary(
                        work,
                        &mut out,
                        format,
                        packet.source_offset(),
                        packet.bytes(),
                    ),
                    (View::Fields, Carrier::Packet(packet)) => {
                        let packet = TransportObservation::from(&packet)
                            .to_packet()
                            .map_err(|e| e.to_string())?;
                        let detached =
                            parse_transport(packet.source.clone(), format, packet.source_offset)
                                .map_err(|e| e.to_string())?;
                        // The independent parse must return the same public fields.
                        let mut detached_work = Work::default();
                        let mut observed_work = Work::default();
                        fields(&mut detached_work, &mut None, &detached)?;
                        fields(&mut observed_work, &mut None, &packet)?;
                        if detached_work.digest != observed_work.digest
                            || detached_work.packets != observed_work.packets
                            || detached_work.bytes != observed_work.bytes
                        {
                            return Err("detached parse and transport observation disagree".into());
                        }
                        fields(work, &mut out, &packet)
                    }
                    (View::Fields, Carrier::Rejected(packet)) => {
                        let cancel = AtomicBool::new(false);
                        let location = SourceLocation {
                            source: SOURCE,
                            offset: packet.source_offset(),
                        };
                        let mut inspector = TransportInspector::new(
                            packet.bytes().clone(),
                            format,
                            location,
                            &budget,
                        )
                        .map_err(|e| e.error.to_string())?;
                        let AssemblyStep::Output(TransportInspection::Rejected(rejected)) =
                            inspector
                                .poll(&mut WorkBudget::new(GRANT), &cancel)
                                .map_err(|e| e.to_string())?
                        else {
                            return Err("attached inspector accepted a rejected carrier".into());
                        };
                        let range = rejected
                            .source_range()
                            .ok_or("attached rejection lacks a source range")?;
                        record(work, &mut out, 8, &[range.offset, range.bytes as u64], &[])
                    }
                    (View::Compact, _) => unreachable!("compact view has its own source driver"),
                }
            })?;
            if clean || matches!(view, View::Boundaries) {
                // The boundary contract compares complete carriers. The field
                // contract additionally compares the strict EOF result.
                record(work, &mut out, 7, &[], &[])
            } else {
                let tail = data.len() % format.bytes();
                record(
                    work,
                    &mut out,
                    8,
                    &[(data.len() - tail) as u64, tail as u64],
                    &[],
                )
            }
        }),
    })
}
