//! DTS-HD source packets compared with explicitly framed FFmpeg PES output.
use crate::runner::{Invocation, Result, Totals, measure};
use crate::ts_input::{self, FIRST_SOURCE, Request};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{Event, PacketInfo};
use triergon_core::{Budget, Error, InputWindow, Step, WorkBudget};
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{AccessUnit, Packet, Reader, ReaderEvent, ReaderLimits};

pub(crate) const PREFIX: &str = "ts-dts-hd-";
const OPERATION: &str = "mpegts-dts-hd-source-access-units";

fn packet(out: &mut Totals, packet: &Packet, carrier: usize, trace: bool) -> Result<()> {
    let AccessUnit::DtsExtension(frame) = packet.access_unit() else {
        return Err("expected extension-only DTS-HD".into());
    };
    let [source] = packet.sources() else {
        return Err("DTS-HD access unit must belong to one aligned finite PES".into());
    };
    let pes = source
        .pes
        .event()
        .pes_source()
        .ok_or("missing source PES")?;
    let first = pes
        .header_source_ranges
        .as_slice()
        .first()
        .ok_or("missing PES header")?;
    if first.entity != FIRST_SOURCE || source.elementary_range != frame.elementary_range() {
        return Err("DTS-HD source range differs from the access unit".into());
    }
    let span = frame.sample_span();
    let range = frame.elementary_range();
    let fields = [
        0, // The contract selects one elementary stream.
        u64::from(pes.pid),
        signed(packet.pts().unwrap_or(i64::MIN)),
        signed(packet.dts().unwrap_or(i64::MIN)),
        pes.header.pts_raw33.unwrap_or(1 << 63),
        pes.header.dts_raw33.unwrap_or(1 << 63),
        first.offset,
        first.offset / carrier as u64 * carrier as u64,
        u64::from(pes.header.declared_packet_length),
        pes.payload_len as u64,
        pes.elementary_offset,
        source.payload_range.start as u64,
        source.payload_range.end as u64,
        range.start,
        range.end,
        span.map_or(Ok(0), |time| u64::try_from(time.numerator()))?,
        span.map_or(Ok(0), |time| u64::try_from(time.denominator()))?,
        pes.header.as_bytes().len() as u64,
        u64::from(packet.clock().is_some()),
    ];
    for value in fields {
        fold(&mut out.digest, value);
    }
    fold_bytes(&mut out.digest, pes.header.as_bytes());
    fold_bytes(&mut out.digest, packet.payload());
    out.packets += 1;
    out.bytes += packet.payload().len() as u64;
    if trace {
        let mut payload_digest = 0;
        fold_bytes(&mut payload_digest, packet.payload());
        println!("{{\"fields\":{fields:?},\"payload_digest\":{payload_digest}}}");
    }
    Ok(())
}

fn operation(pool: &Budget, input: &Request, trace: bool) -> Result<Totals> {
    if !input.following.is_empty() {
        return Err("DTS-HD source comparison requires one finite entity".into());
    }
    let mut reader = Reader::new(
        pool,
        FIRST_SOURCE,
        ts_input::capabilities(&input.input),
        ReaderLimits {
            format: input.format,
            program: Some(input.program),
            descriptor_namespace: input.namespace,
            ..ReaderLimits::default()
        },
        64 * 1024,
    )?;
    let cancel = AtomicBool::new(false);
    let mut out = Totals::default();
    loop {
        match reader.step(&mut WorkBudget::new(4096), &cancel)? {
            Step::Ready(Event::Packet(unit)) => {
                packet(&mut out, &unit, input.format.bytes(), trace)?;
            }
            Step::Ready(Event::Other(ReaderEvent::Source(source))) => {
                if let TraceEvent::Diagnostic(fault) = source.event() {
                    return Err(format!("source diagnostic: {fault:?}").into());
                }
            }
            Step::Ready(Event::Other(
                ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. },
            )) => (),
            Step::Ready(event) => return Err(format!("unexpected DTS-HD event: {event:?}").into()),
            Step::Yield => (),
            Step::NeedInput(request) => {
                if request.source != FIRST_SOURCE {
                    return Err(Error::SourceMismatch.into());
                }
                let start = usize::try_from(request.offset)?;
                if start > input.input.len() {
                    return Err("DTS-HD reader requested bytes beyond EOF".into());
                }
                if start == input.input.len() {
                    reader.finish(request.source, request.token)?;
                } else {
                    let end = input.input.len().min(start + request.maximum);
                    reader.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        input.input.slice(start..end)?,
                    )?)?;
                }
            }
            Step::End(end) => {
                if end.source != FIRST_SOURCE
                    || end.offset != input.input.len() as u64
                    || out.packets == 0
                {
                    return Err("DTS-HD comparison ended without complete packet coverage".into());
                }
                return Ok(out);
            }
            Step::Backpressure(pressure) => {
                return Err(format!("DTS-HD backpressure: {pressure:?}").into());
            }
        }
    }
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, PREFIX, " SIZE PROGRAM --dvb", 3..=3, true)?;
    let pool = Budget::new(256 * 1024 * 1024)?;
    let request = ts_input::parse(&pool, Path::new(invocation.path), invocation.arguments)?;
    let trace = invocation.mode == triergon_harness_runner::Mode::Trace;
    measure(&invocation, OPERATION, Some(&pool), || {
        operation(&pool, &request, trace)
    })
}
