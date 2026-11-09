//! Complete transport units through the consumer facade and public source facts.
//!
//! The separate demux-clock policy places source PES anchors for comparison. It
//! never changes the source packet's timestamps or supplies a clock to later AUs.
mod control_only;

use crate::runner::{Invocation, Refusal, Result, Totals, measure};
use crate::ts_input::{self, FIRST_SOURCE, Request};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon::{
    AnyOther, AnyPacket, AnyReader, ContainerReader, Event, PacketInfo, Step, TrackId, TrackInfo,
};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputWindow, Rounding, SourceCapabilities, SourceId,
    SourceLocation, TimeBase, WorkBudget,
};
use triergon_framing::Ac3Family;
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_mpegts::compat::annex_b::VideoEofPolicy;
use triergon_mpegts::compat::input::{InputBootstrap, InputStep};
use triergon_mpegts::compat::latm;
use triergon_mpegts::compat::pes::DemuxClock;
use triergon_mpegts::reference_trace::input::PositionBasis;
use triergon_mpegts::{
    AccessUnit, AudioEofPolicy, EntityTransition, FaultKind, IncompleteVideo, Packet, PacketClock,
    PacketFormat, PayloadSource, Reader, ReaderEvent, ReaderLimits, SourceEpochs, TraceEvent,
};

pub const PREFIX: &str = "ts-au-";
const OPERATION: &str = "mpegts-access-unit-packets";
const WINDOW: usize = 64 * 1024;
const WORK: u64 = 4096;

struct TrackProjection {
    id: TrackId,
    pending_ac3: Option<Packet>,
    ac3_duration_known: bool,
}

#[derive(Clone, Copy)]
struct PacketPosition<'a> {
    request: &'a Request,
    basis: PositionBasis,
}

impl PacketPosition<'_> {
    fn at(self, entity: SourceId, offset: u64) -> Result<i64> {
        let carrier = match self.request.format {
            PacketFormat::Ts188 => offset / 188 * 188,
            PacketFormat::M2ts192 => offset / 192 * 192,
        };
        let prefix = u64::from(
            self.request.format == PacketFormat::M2ts192 && self.basis == PositionBasis::SyncStart,
        ) * 4;
        let base = self
            .request
            .position_base(entity)
            .ok_or(Error::SourceMismatch)?;
        let offset = base
            .checked_add(carrier)
            .and_then(|at| at.checked_add(prefix))
            .ok_or(Error::Overflow)?;
        Ok(i64::try_from(offset)?)
    }
}

fn is_ac3(packet: &Packet) -> bool {
    match packet.access_unit() {
        AccessUnit::Ac3(_) => true,
        // Incomplete private audio retains the DVB transport tag.
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
    if request.format == PacketFormat::Ts188 {
        return Ok(PositionBasis::CarrierStart);
    }
    let following = request
        .following
        .as_slice()
        .iter()
        .take_while(|entity| !entity.discontinuity);
    let length = request.input.len()
        + following
            .clone()
            .map(|entity| entity.input.len())
            .sum::<usize>();
    let finite = length <= InputBootstrap::MAX_BORROWED_BYTES;
    let count = if finite {
        length
    } else {
        InputBootstrap::M2TS_PREFIX_BYTES
    };
    let bytes = if count <= request.input.len() {
        request.input.slice(0..count)?
    } else {
        let mut prefix = BudgetVec::new(pool);
        prefix.try_reserve_exact(count)?;
        prefix.try_extend_from_slice(request.input.as_ref())?;
        for entity in following {
            let take = (count - prefix.len()).min(entity.input.len());
            prefix.try_extend_from_slice(&entity.input.as_ref()[..take])?;
            if prefix.len() == count {
                break;
            }
        }
        prefix.freeze().map_err(|error| error.error)?
    };
    let mut input = InputCursor::new(
        pool,
        FIRST_SOURCE,
        SourceCapabilities {
            length: Some(length as u64),
            random_access: false,
        },
        count,
    )?;
    let need = input.request();
    let window = InputWindow::new(need.source, need.token, 0, bytes)?;
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

fn refused(mut totals: Totals, error: &dyn std::fmt::Display, at: SourceLocation) -> Totals {
    totals.refusal = Some(Box::new(Refusal {
        reason: error.to_string(),
        cause: format!(
            "{{\"phase\":\"access_units\",\"source\":{},\"input_window_offset\":{}}}",
            at.source.0, at.offset,
        ),
    }));
    totals
}

#[derive(Clone, Copy)]
enum Payload<'a> {
    Framed {
        sources: &'a [PayloadSource],
        bytes: &'a [u8],
    },
    PreservedPes(&'a latm::PesPacket),
}

struct PacketProjection<'a> {
    pid: u16,
    clock: Option<&'a PacketClock>,
    epochs: SourceEpochs,
    payload: Payload<'a>,
    flags: u64,
}

impl<'a> From<&'a Packet> for PacketProjection<'a> {
    fn from(packet: &'a Packet) -> Self {
        let flags = match packet.access_unit() {
            AccessUnit::Video(unit) => u64::from(unit.random_access),
            AccessUnit::Loas(_) => 0,
            AccessUnit::Adts(_) | AccessUnit::Ac3(_) | AccessUnit::IncompleteAudio { .. } => 1,
        };
        Self {
            pid: packet.track_info().stream().pid,
            clock: packet.clock(),
            epochs: packet.epochs(),
            payload: Payload::Framed {
                sources: packet.sources(),
                bytes: packet.payload(),
            },
            flags,
        }
    }
}

impl<'a> From<&'a IncompleteVideo> for PacketProjection<'a> {
    fn from(video: &'a IncompleteVideo) -> Self {
        Self {
            pid: video.track_info().stream().pid,
            clock: video.clock(),
            epochs: video.epochs(),
            payload: Payload::Framed {
                sources: video.sources(),
                bytes: video.bytes().as_ref(),
            },
            flags: 0,
        }
    }
}

impl<'a> From<&'a latm::PesPacket> for PacketProjection<'a> {
    fn from(packet: &'a latm::PesPacket) -> Self {
        let pes = packet.pes();
        Self {
            pid: pes.pid,
            clock: None,
            epochs: pes.epochs,
            payload: Payload::PreservedPes(packet),
            flags: 0,
        }
    }
}

fn fold_packet(
    totals: &mut Totals,
    packet: PacketProjection<'_>,
    index: usize,
    position: PacketPosition<'_>,
    duration: i64,
    clock: &mut DemuxClock,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut work = WorkBudget::new(WORK);
    let header = match (packet.payload, packet.clock) {
        (Payload::PreservedPes(packet), _) if packet.clock_is_quarantined() => {
            return Err("unresolved source clock".into());
        }
        (Payload::PreservedPes(packet), _) => Some(&packet.pes().header),
        (_, Some(anchor)) if anchor.quarantined => return Err("unresolved source clock".into()),
        (_, Some(anchor)) => match anchor.pes.event() {
            TraceEvent::Pes(pes) => Some(&pes.header),
            _ => return Err("packet clock has no PES".into()),
        },
        (_, None) => None,
    };
    let time = clock
        .project_header(packet.pid, packet.epochs, header, &mut work, cancel)?
        .ok_or("clock projection exhausted its work")?;
    let ticks = |value: Option<i128>| -> Result<i64> {
        Ok(value.map(i64::try_from).transpose()?.unwrap_or(i64::MIN))
    };
    let (pes, at_pes_start, length) = match packet.payload {
        Payload::Framed { sources, bytes } => {
            let first = sources.first().ok_or("packet has no source PES")?;
            let TraceEvent::Pes(pes) = first.pes.event() else {
                return Err("packet payload has no PES".into());
            };
            (pes, first.payload_range.start == 0, bytes.len())
        }
        Payload::PreservedPes(packet) => (packet.pes(), true, packet.pes().payload_len),
    };
    // The parser reports a PES carrier position once, at its first unit. Later
    // units from the same PES have no packet position, even though source byte
    // ranges remain available on every packet.
    let position = if at_pes_start {
        let range = pes
            .header_source_ranges
            .as_slice()
            .first()
            .ok_or("PES has no header range")?;
        position.at(range.entity, range.offset)?
    } else {
        -1
    };
    for value in [
        index as u64,
        u64::from(pes.pid),
        signed(ticks(time.pts90)?),
        signed(ticks(time.dts90)?),
        signed(duration),
        signed(position),
        packet.flags,
        length as u64,
    ] {
        fold(&mut totals.digest, value);
    }
    match packet.payload {
        Payload::Framed { bytes, .. } => fold_bytes(&mut totals.digest, bytes),
        Payload::PreservedPes(_) => {
            let mut seen = 0;
            for span in pes.payload() {
                fold_bytes(&mut totals.digest, span);
                seen += span.len();
            }
            if seen != length {
                return Err("packet payload does not match its length".into());
            }
        }
    }
    // AV_PKT_DATA_MPEGTS_STREAM_ID, including every unit sharing a PES.
    for value in [1, 19, 1, u64::from(pes.header.stream_id)] {
        fold(&mut totals.digest, value);
    }
    totals.packets += 1;
    totals.bytes += length as u64;
    Ok(())
}

fn operation(pool: &Budget, request: &Request, cancel: &AtomicBool) -> Result<Totals> {
    let mut totals = Totals::default();
    let mut at = SourceLocation {
        source: FIRST_SOURCE,
        offset: 0,
    };
    let basis = match position_basis(pool, request, cancel) {
        Ok(basis) => basis,
        Err(error) => return Ok(refused(totals, error.as_ref(), at)),
    };
    let position = PacketPosition { request, basis };
    let limits = ReaderLimits {
        format: request.format,
        program: Some(request.program),
        descriptor_namespace: request.namespace,
        audio_eof_policy: AudioEofPolicy::ReportIncomplete,
        video_eof_policy: VideoEofPolicy::ReportUnframed,
        latm_packet_policy: latm::PacketPolicy::PreservePes,
        ..ReaderLimits::default()
    };
    let mut reader = AnyReader::MpegTs(Reader::new(
        pool,
        FIRST_SOURCE,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        limits,
        WINDOW,
    )?);
    let mut clock = DemuxClock::new(request.program)?;
    let mut tracks = BudgetVec::<TrackProjection>::new(pool);
    let mut source = FIRST_SOURCE;
    let mut current = &request.input;
    let mut following = request.following.as_slice().iter();
    let mut observed_media = false;
    let refusal = loop {
        let step = match reader.step(&mut WorkBudget::new(WORK), cancel) {
            Ok(step) => step,
            Err(error) => return Ok(refused(totals, &error, at)),
        };
        match step {
            Step::Ready(
                Event::Packet(AnyPacket::MpegTs(packet))
                | Event::Other(AnyOther::MpegTs(
                    ReaderEvent::VideoHeaders(packet) | ReaderEvent::IncompleteAudio(packet),
                )),
            ) => {
                observed_media = true;
                let pid = packet.track_info().id();
                let index = tracks
                    .as_slice()
                    .iter()
                    .position(|value| value.id == pid)
                    .ok_or("packet has no registered track")?;
                let track = &mut tracks.as_mut_slice()[index];
                let (packet, duration) = if matches!(
                    packet.access_unit(),
                    AccessUnit::Ac3(unit) if unit.facts().header.family == Ac3Family::Eac3
                ) {
                    // The reader has already used lookahead to seal the group.
                    let duration = ac3_duration(&packet, true)?;
                    (packet, duration)
                } else if is_ac3(&packet) {
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
                    (&packet).into(),
                    index,
                    position,
                    duration,
                    &mut clock,
                    cancel,
                ) {
                    return Ok(refused(totals, error.as_ref(), at));
                }
            }
            Step::Ready(Event::Other(AnyOther::MpegTs(
                event @ (ReaderEvent::IncompleteVideo(_) | ReaderEvent::LatmPes(_)),
            ))) => {
                observed_media = true;
                let (id, projection) = match &event {
                    ReaderEvent::IncompleteVideo(video) => (video.track_info().id(), video.into()),
                    ReaderEvent::LatmPes(packet) => (packet.track_info().id(), packet.into()),
                    _ => unreachable!("expected an opaque packet observation"),
                };
                let index = tracks
                    .as_slice()
                    .iter()
                    .position(|value| value.id == id)
                    .ok_or("opaque packet has no registered track")?;
                if let Err(error) = fold_packet(
                    &mut totals,
                    projection,
                    index,
                    position,
                    0,
                    &mut clock,
                    cancel,
                ) {
                    return Ok(refused(totals, error.as_ref(), at));
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
                        if fault.kind == FaultKind::Psi && !observed_media {
                            break totals;
                        }
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
                    TraceEvent::Discontinuity { .. } => {
                        // The parser drops the frame still waiting for lookahead.
                        for track in tracks.as_mut_slice() {
                            track.pending_ac3 = None;
                            track.ac3_duration_known = false;
                        }
                    }
                    _ => (),
                }
            }
            Step::Ready(Event::Other(AnyOther::MpegTs(ReaderEvent::Interrupted {
                cause, ..
            }))) if matches!(cause.event(), TraceEvent::Discontinuity { .. }) => (),
            Step::Ready(Event::Other(AnyOther::MpegTs(event))) => {
                return Ok(refused(
                    totals,
                    &format!("incomplete access-unit observation: {event:?}"),
                    at,
                ));
            }
            Step::Ready(_) => return Err("unexpected container event".into()),
            Step::Yield => (),
            Step::NeedInput(input) => {
                if input.source != source {
                    return Err(Error::SourceMismatch.into());
                }
                at = SourceLocation {
                    source,
                    offset: input.offset,
                };
                let start = usize::try_from(input.offset)?;
                if start > current.len() {
                    return Err("reader requested bytes beyond the input".into());
                }
                if start == current.len() {
                    if let Some(next) = following.next() {
                        let AnyReader::MpegTs(transport) = &mut reader else {
                            return Err("unexpected container reader".into());
                        };
                        let transition = if next.discontinuity {
                            EntityTransition::Discontinuity
                        } else {
                            EntityTransition::Contiguous
                        };
                        transport.finish_entity(
                            input.source,
                            input.token,
                            next.source,
                            ts_input::capabilities(&next.input),
                            transition,
                        )?;
                        source = next.source;
                        current = &next.input;
                    } else {
                        reader.finish(input.source, input.token)?;
                    }
                } else {
                    let end = current.len().min(start + input.maximum);
                    let bytes = current.slice(start..end)?;
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
                    at,
                ));
            }
            Step::End(end) => {
                if end.source != source
                    || end.offset != current.len() as u64
                    || following.len() != 0
                {
                    return Err("reader ended before the physical input".into());
                }
                for (index, track) in tracks.as_mut_slice().iter_mut().enumerate() {
                    if let Some(packet) = track.pending_ac3.take() {
                        let duration = ac3_duration(&packet, track.ac3_duration_known)?;
                        fold_packet(
                            &mut totals,
                            (&packet).into(),
                            index,
                            position,
                            duration,
                            &mut clock,
                            cancel,
                        )?;
                    }
                }
                if totals.packets == 0 {
                    let refusal = refused(totals, &"no framed packet was observed", end);
                    if !observed_media {
                        break refusal;
                    }
                    return Ok(refusal);
                }
                return Ok(totals);
            }
        }
    };
    // Release retained packets before replaying the original input.
    drop(reader);
    drop(tracks);
    match control_only::confirm(pool, request, cancel) {
        Ok(true) => Ok(Totals::default()),
        Ok(false) | Err(Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated) => {
            Ok(refusal)
        }
        Err(error) => Err(error.into()),
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
    let pool = Budget::new(256 * 1024 * 1024)?;
    let request = ts_input::parse(&pool, Path::new(invocation.path), invocation.arguments)?;
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
    use super::{Request, Totals, operation};
    use std::sync::atomic::AtomicBool;
    use triergon_core::{Budget, BudgetVec, Bytes};
    use triergon_mpegts::PacketFormat;
    use triergon_mpegts::psi::DescriptorNamespace;

    fn fold_single(input: &[u8]) -> Totals {
        let pool = Budget::new(1 << 20).unwrap();
        let request = Request {
            input: Bytes::copy_from_slice(&pool, input).unwrap(),
            format: PacketFormat::Ts188,
            program: 1,
            namespace: DescriptorNamespace::Unspecified,
            following: BudgetVec::new(&pool),
        };
        let baseline = pool.used();
        let totals = operation(&pool, &request, &AtomicBool::new(false)).unwrap();
        assert_eq!(pool.used(), baseline);
        totals
    }

    #[test]
    fn control_only_entity_completes_after_psi_diagnostic_or_empty_end() {
        for input in [
            &include_bytes!("../../mpegts/tests/data/psi-crc-invalid.ts")[..],
            &include_bytes!("../../mpegts/tests/data/pat-network-pid-not-track.ts")[..],
        ] {
            let totals = fold_single(input);
            assert!(totals.refusal.is_none());
            assert_eq!((totals.packets, totals.bytes, totals.digest), (0, 0, 0));
        }
    }

    #[test]
    fn media_after_a_psi_diagnostic_prevents_control_only_completion() {
        let totals = fold_single(include_bytes!(
            "../../mpegts/tests/data/packet-completion-v1/crc-control-then-valid-pes.ts"
        ));
        assert_eq!(totals.packets, 0);
        assert_eq!(totals.refusal.unwrap().reason, "source diagnostic: Psi");
    }

    #[test]
    fn rejected_pes_does_not_become_successful_empty_completion() {
        for input in [
            &include_bytes!("../../mpegts/tests/data/pes-start-prefix-invalid.ts")[..],
            &include_bytes!("../../mpegts/tests/data/pes-optional-header-overrun.ts")[..],
        ] {
            let totals = fold_single(input);
            assert_eq!(totals.packets, 0);
            let refusal = totals.refusal.unwrap();
            assert!(refusal.reason.starts_with("source diagnostic:"));
            assert!(refusal.cause.contains("\"phase\":\"diagnostic\""));
        }
    }
}
