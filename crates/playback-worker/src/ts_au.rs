//! Complete transport units through the consumer facade and public source facts.
//!
//! The separate demux-clock policy places source PES anchors for comparison. It
//! never changes the source packet's timestamps or supplies a clock to later AUs.
use crate::runner::{Invocation, MAX_INPUT, Refusal, Result, Totals, measure};
use std::sync::atomic::AtomicBool;
use triergon::{
    AnyOther, AnyPacket, AnyReader, ContainerReader, Event, PacketInfo, Step, TrackId, TrackInfo,
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputWindow, Rounding, SourceCapabilities,
    SourceId, TimeBase, WorkBudget,
};
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_mpegts::compat::input::{InputBootstrap, InputStep};
use triergon_mpegts::compat::pes::DemuxClock;
use triergon_mpegts::psi::{DescriptorNamespace, DvbNamespaceEvidence};
use triergon_mpegts::reference_trace::input::PositionBasis;
use triergon_mpegts::{
    AccessUnit, AudioEofPolicy, Packet, PacketFormat, Reader, ReaderEvent, ReaderLimits, TraceEvent,
};

pub const PREFIX: &str = "ts-au-";
const OPERATION: &str = "mpegts-access-unit-packets";
const WINDOW: usize = 64 * 1024;
const WORK: u64 = 4096;
const SOURCE: SourceId = SourceId(1);

struct Request {
    bytes: Bytes,
    limits: ReaderLimits,
    more_entities: bool,
}

struct TrackProjection {
    id: TrackId,
    pending_ac3: Option<Packet>,
    ac3_duration_known: bool,
}

#[derive(Clone, Copy)]
struct PacketPosition {
    format: PacketFormat,
    basis: PositionBasis,
}

impl PacketPosition {
    fn at(self, offset: u64) -> Result<i64> {
        let carrier = match self.format {
            PacketFormat::Ts188 => offset / 188 * 188,
            PacketFormat::M2ts192 => offset / 192 * 192,
        };
        let prefix = u64::from(
            self.format == PacketFormat::M2ts192 && self.basis == PositionBasis::SyncStart,
        ) * 4;
        Ok(i64::try_from(
            carrier.checked_add(prefix).ok_or(Error::Overflow)?,
        )?)
    }
}

fn is_ac3(packet: &Packet) -> bool {
    match packet.access_unit() {
        AccessUnit::Ac3(_) => true,
        // The reader admits only ADTS and explicitly DVB-signalled AC-3 audio.
        // Its private transport tag remains separate from neutral codec identity.
        AccessUnit::IncompleteAudio { .. } => packet.track_info().stream().stream_type == 0x06,
        _ => false,
    }
}

fn ac3_header(packet: &Packet) -> Option<triergon_framing::Ac3FrameHeader> {
    match packet.access_unit() {
        AccessUnit::Ac3(frame) => Some(frame.facts().header),
        AccessUnit::IncompleteAudio { bytes, .. } if is_ac3(packet) => {
            triergon_framing::probe_ac3_header(bytes.as_ref()).ok()
        }
        _ => None,
    }
}

fn ac3_duration(packet: &Packet, known: bool) -> Result<i64> {
    if !known {
        return Ok(0);
    }
    ac3_header(packet).map_or(Ok(0), |header| {
        Ok(header
            .sample_span()?
            .to_i64_ticks(TimeBase::new(1, 90_000)?, Rounding::NearestAway)?)
    })
}

/// Apply the existing bounded input-position comparison to an M2TS prefix.
/// This inspection remains inside each complete measured operation.
fn position_basis(pool: &Budget, request: &Request, cancel: &AtomicBool) -> Result<PositionBasis> {
    if request.limits.format == PacketFormat::Ts188 {
        return Ok(PositionBasis::CarrierStart);
    }
    let finite = request.bytes.len() <= InputBootstrap::MAX_BORROWED_BYTES;
    let count = if finite {
        request.bytes.len()
    } else {
        InputBootstrap::M2TS_PREFIX_BYTES
    };
    let mut input = InputCursor::new(
        pool,
        SOURCE,
        SourceCapabilities {
            length: Some(request.bytes.len() as u64),
            random_access: false,
        },
        count,
    )?;
    let need = input.request();
    let window = InputWindow::new(need.source, need.token, 0, request.bytes.slice(0..count)?)?;
    input.provide(&window)?;
    let mut bootstrap = if finite {
        let mut bootstrap = InputBootstrap::new(&input, &window, PacketFormat::M2ts192)?;
        bootstrap.observe_entity_eof(&input, window.source, &window.token, count as u64)?;
        bootstrap
    } else {
        InputBootstrap::new_m2ts_prefix(&input, &window)?
    };
    loop {
        match bootstrap.poll(&input, &mut WorkBudget::new(WORK), cancel)? {
            InputStep::Ready(context) => return Ok(context.basis()),
            InputStep::Yield => (),
            InputStep::NeedEntityEof => return Err("input-position probe lost EOF".into()),
        }
    }
}

fn refused(mut totals: Totals, error: &dyn std::fmt::Display, offset: u64) -> Totals {
    totals.refusal = Some(Box::new(Refusal {
        reason: error.to_string(),
        cause: format!(
            "{{\"phase\":\"access_units\",\"source\":1,\"input_window_offset\":{offset}}}"
        ),
    }));
    totals
}

fn fold_packet(
    totals: &mut Totals,
    packet: &Packet,
    index: usize,
    position: PacketPosition,
    duration: i64,
    clock: &mut DemuxClock,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut work = WorkBudget::new(WORK);
    let header = match packet.clock() {
        Some(anchor) if anchor.quarantined => return Err("unresolved source clock".into()),
        Some(anchor) => match anchor.pes.event() {
            TraceEvent::Pes(pes) => Some(&pes.header),
            _ => return Err("packet clock has no PES".into()),
        },
        None => None,
    };
    let time = clock
        .project_header(
            packet.track_info().stream().pid,
            packet.epochs(),
            header,
            &mut work,
            cancel,
        )?
        .ok_or("clock projection exhausted its work")?;
    let ticks = |value: Option<i128>| -> Result<i64> {
        Ok(value.map(i64::try_from).transpose()?.unwrap_or(i64::MIN))
    };
    let first = packet.sources().first().ok_or("packet has no source PES")?;
    let TraceEvent::Pes(pes) = first.pes.event() else {
        return Err("packet payload has no PES".into());
    };
    // The parser reports a PES carrier position once, at its first unit. Later
    // units from the same PES have no packet position, even though source byte
    // ranges remain available on every packet.
    let position = if first.payload_range.start == 0 {
        let offset = pes
            .header_source_ranges
            .as_slice()
            .first()
            .ok_or("PES has no header range")?
            .offset;
        position.at(offset)?
    } else {
        -1
    };
    let flags = match packet.access_unit() {
        AccessUnit::Video(unit) => u64::from(unit.random_access),
        AccessUnit::Adts(_) | AccessUnit::Ac3(_) | AccessUnit::IncompleteAudio { .. } => 1,
    };
    for value in [
        index as u64,
        u64::from(pes.pid),
        signed(ticks(time.pts90)?),
        signed(ticks(time.dts90)?),
        signed(duration),
        signed(position),
        flags,
        packet.payload().len() as u64,
    ] {
        fold(&mut totals.digest, value);
    }
    fold_bytes(&mut totals.digest, packet.payload());
    // AV_PKT_DATA_MPEGTS_STREAM_ID, including every unit sharing a PES.
    for value in [1, 19, 1, u64::from(pes.header.stream_id)] {
        fold(&mut totals.digest, value);
    }
    totals.packets += 1;
    totals.bytes += packet.payload().len() as u64;
    Ok(())
}

fn operation(pool: &Budget, request: &Request, cancel: &AtomicBool) -> Result<Totals> {
    let mut totals = Totals::default();
    if request.more_entities {
        return Ok(refused(
            totals,
            &"no single-input reference operation for more than one media entity",
            0,
        ));
    }
    let basis = match position_basis(pool, request, cancel) {
        Ok(basis) => basis,
        Err(error) => return Ok(refused(totals, error.as_ref(), 0)),
    };
    let position = PacketPosition {
        format: request.limits.format,
        basis,
    };
    let mut reader = AnyReader::MpegTs(Reader::new(
        pool,
        SOURCE,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        request.limits,
        WINDOW,
    )?);
    let mut clock = DemuxClock::new(request.limits.program.ok_or("selected program missing")?)?;
    let mut tracks = BudgetVec::<TrackProjection>::new(pool);
    let mut offset = 0;
    loop {
        let step = match reader.step(&mut WorkBudget::new(WORK), cancel) {
            Ok(step) => step,
            Err(error) => return Ok(refused(totals, &error, offset)),
        };
        match step {
            Step::Ready(
                Event::Packet(AnyPacket::MpegTs(packet))
                | Event::Other(AnyOther::MpegTs(
                    ReaderEvent::VideoHeaders(packet) | ReaderEvent::IncompleteAudio(packet),
                )),
            ) => {
                let pid = packet.track_info().id();
                let index = tracks
                    .as_slice()
                    .iter()
                    .position(|value| value.id == pid)
                    .ok_or("packet has no registered track")?;
                let track = &mut tracks.as_mut_slice()[index];
                let (packet, duration) = if is_ac3(&packet) {
                    // The reference parser confirms AC-3 duration from another
                    // frame header. Keep the first packet until that lookahead
                    // or real EOF; a lone frame retains its zero packet duration.
                    track.ac3_duration_known |=
                        track.pending_ac3.is_some() && ac3_header(&packet).is_some();
                    let previous = track.pending_ac3.replace(packet);
                    let Some(previous) = previous else {
                        continue;
                    };
                    let duration = ac3_duration(&previous, track.ac3_duration_known)?;
                    (previous, duration)
                } else {
                    (packet, 0)
                };
                if let Err(error) = fold_packet(
                    &mut totals,
                    &packet,
                    index,
                    position,
                    duration,
                    &mut clock,
                    cancel,
                ) {
                    return Ok(refused(totals, error.as_ref(), offset));
                }
            }
            Step::Ready(Event::Other(AnyOther::MpegTs(ReaderEvent::Source(source)))) => {
                match source.event() {
                    TraceEvent::Pes(pes) => {
                        // The demux interval belongs to source PES order, before
                        // codec-parser lookahead delays individual packet output.
                        clock
                            .project(pes, &mut WorkBudget::new(WORK), cancel)?
                            .ok_or("source clock projection exhausted its work")?;
                    }
                    TraceEvent::Diagnostic(fault) => {
                        totals.refusal = Some(Box::new(Refusal {
                            reason: format!("source diagnostic: {:?}", fault.kind),
                            cause: format!(
                                "{{\"phase\":\"diagnostic\",\"source\":{},\"offset\":{}}}",
                                fault.location.source.0, fault.location.offset,
                            ),
                        }));
                        return Ok(totals);
                    }
                    TraceEvent::ProgramMap {
                        map, active: true, ..
                    } => {
                        for stream in map.streams.as_slice() {
                            let id = stream.id();
                            if !tracks.as_slice().iter().any(|track| track.id == id) {
                                tracks
                                    .try_push(TrackProjection {
                                        id,
                                        pending_ac3: None,
                                        ac3_duration_known: false,
                                    })
                                    .map_err(|error| error.error)?;
                            }
                        }
                    }
                    _ => (),
                }
            }
            Step::Ready(Event::Other(AnyOther::MpegTs(event))) => {
                return Ok(refused(
                    totals,
                    &format!("incomplete access-unit observation: {event:?}"),
                    offset,
                ));
            }
            Step::Ready(_) => return Err("unexpected container event".into()),
            Step::Yield => (),
            Step::NeedInput(input) => {
                let at = usize::try_from(input.offset)?;
                if at > request.bytes.len() {
                    return Err("reader requested bytes beyond the input".into());
                }
                offset = input.offset;
                if at == request.bytes.len() {
                    reader.finish(input.source, input.token)?;
                } else {
                    let end = request.bytes.len().min(at + input.maximum);
                    let bytes = request.bytes.slice(at..end)?;
                    reader.provide(&InputWindow::new(
                        input.source,
                        input.token,
                        input.offset,
                        bytes,
                    )?)?;
                }
            }
            Step::Backpressure(pressure) => {
                return Ok(refused(
                    totals,
                    &Error::Backpressure {
                        requested: pressure.requested,
                        available: pressure.available,
                    },
                    offset,
                ));
            }
            Step::End(end) => {
                if end.source != SOURCE || end.offset != request.bytes.len() as u64 {
                    return Err("reader ended before the physical input".into());
                }
                for (index, track) in tracks.as_mut_slice().iter_mut().enumerate() {
                    if let Some(packet) = track.pending_ac3.take() {
                        let duration = ac3_duration(&packet, track.ac3_duration_known)?;
                        fold_packet(
                            &mut totals,
                            &packet,
                            index,
                            position,
                            duration,
                            &mut clock,
                            cancel,
                        )?;
                    }
                }
                if totals.packets == 0 {
                    return Ok(refused(
                        totals,
                        &"no framed packet was observed",
                        end.offset,
                    ));
                }
                return Ok(totals);
            }
        }
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(
        args,
        PREFIX,
        " SIZE PROGRAM [--dvb] [[--discontinuity] ENTITY]...",
        2..=usize::MAX,
        false,
    )?;
    let [size, program, rest @ ..] = invocation.arguments else {
        return Err("expected packet size and program".into());
    };
    let format = match size.as_str() {
        "188" => PacketFormat::Ts188,
        "192" => PacketFormat::M2ts192,
        _ => return Err("packet size must be 188 or 192".into()),
    };
    let mut limits = ReaderLimits {
        format,
        program: Some(program.parse()?),
        audio_eof_policy: AudioEofPolicy::ReportIncomplete,
        ..ReaderLimits::default()
    };
    let rest = if rest.first().is_some_and(|value| value == "--dvb") {
        limits.descriptor_namespace =
            DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile);
        &rest[1..]
    } else {
        rest
    };
    let pool = Budget::new(256 * 1024 * 1024)?;
    let bytes = invocation.input(MAX_INPUT)?;
    if bytes.is_empty() {
        return Err("input must be a bounded nonempty file".into());
    }
    let request = Request {
        bytes: Bytes::try_from_vec(&pool, bytes).map_err(|error| error.error())?,
        limits,
        more_entities: !rest.is_empty(),
    };
    measure(&invocation, OPERATION, Some(&pool), || {
        operation(
            &pool,
            std::hint::black_box(&request),
            &AtomicBool::new(false),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{Request, operation};
    use std::sync::atomic::AtomicBool;
    use triergon_core::{Budget, Bytes};
    use triergon_mpegts::ReaderLimits;

    #[test]
    fn rejected_pes_does_not_become_successful_empty_completion() {
        for input in [
            &include_bytes!("../../mpegts/tests/data/pes-start-prefix-invalid.ts")[..],
            &include_bytes!("../../mpegts/tests/data/pes-optional-header-overrun.ts")[..],
        ] {
            let pool = Budget::new(1 << 20).unwrap();
            let request = Request {
                bytes: Bytes::copy_from_slice(&pool, input).unwrap(),
                limits: ReaderLimits {
                    program: Some(1),
                    ..ReaderLimits::default()
                },
                more_entities: false,
            };
            let baseline = pool.used();
            let totals = operation(&pool, &request, &AtomicBool::new(false)).unwrap();
            assert_eq!(totals.packets, 0);
            let refusal = totals.refusal.unwrap();
            assert!(refusal.reason.starts_with("source diagnostic:"));
            assert!(refusal.cause.contains("\"phase\":\"diagnostic\""));
            assert_eq!(pool.used(), baseline);
        }
    }
}
