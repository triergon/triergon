//! Transport access units and observations through the public source reader.
//!
//! The separate demux-clock policy places source PES anchors for comparison. It
//! never changes the source packet's timestamps or supplies a clock to later AUs.
use crate::runner::{Invocation, Refusal, Result, Totals, measure};
use crate::ts::{au_recovery, confirm_control_only_entity, fold_payload};
use crate::ts_input::{self, FIRST_SOURCE, Request};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{ContainerReader, Event, PacketInfo, TrackId, TrackInfo};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputWindow, Rounding, SourceCapabilities, SourceId,
    SourceLocation, Step, TimeBase, WorkBudget,
};
use triergon_framing::Ac3Family;
use triergon_harness_protocol::digest::signed;
use triergon_mpegts::compat::adts::{PacketPolicy, PesPolicy};
use triergon_mpegts::compat::annex_b::{ContinuityEvent, ContinuityReader, ContinuityResidue};
use triergon_mpegts::compat::input::{InputBootstrap, InputStep};
use triergon_mpegts::compat::latm;
use triergon_mpegts::compat::pes::{DemuxClock, UnframedAudioPolicy};
use triergon_mpegts::reference_trace::input::PositionBasis;
use triergon_mpegts::{
    AccessUnit, AudioEofPolicy, DvbSubtitleTailPolicy, EntityTransition, FaultKind,
    IncompleteVideo, Packet, PacketClock, PacketFormat, PayloadSource, ReaderEvent, ReaderLimits,
    SourceEpochs, TraceEvent, UnframedAudio,
};

mod malformed;

pub const PREFIX: &str = "ts-au-";
const OPERATION: &str = "mpegts-access-unit-packets";
const WINDOW: usize = 64 * 1024;
const WORK: u64 = 4096;

enum SourceProof {
    ControlOnly,
    ReportedMalformed(malformed::Diagnostic),
    OpaqueRecovery(au_recovery::Trigger),
}

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
        // Unfinished private audio keeps the fragment projection.
        AccessUnit::IncompleteAudio { .. } => {
            matches!(packet.track_info().stream().stream_type, 0x06 | 0x81 | 0x87)
        }
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
    totals.refusal.get_or_insert_with(|| {
        Box::new(Refusal {
            reason: error.to_string(),
            cause: format!(
                "{{\"phase\":\"access_units\",\"source\":{},\"input_window_offset\":{}}}",
                at.source.0, at.offset,
            ),
        })
    });
    totals
}

enum Payload<'a> {
    Framed {
        sources: &'a [PayloadSource],
        bytes: &'a [u8],
        pes_position: bool,
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
            AccessUnit::Adts(_)
            | AccessUnit::AdtsFragment(_)
            | AccessUnit::Ac3(_)
            | AccessUnit::DvbSubtitle(_)
            | AccessUnit::IncompleteAudio { .. } => 1,
        };
        Self {
            pid: packet.track_info().stream().pid,
            clock: packet.clock(),
            epochs: packet.epochs(),
            payload: Payload::Framed {
                sources: packet.sources(),
                bytes: packet.payload(),
                // DVB subtitle bytes start after the two-byte PES wrapper.
                pes_position: matches!(packet.access_unit(), AccessUnit::DvbSubtitle(_)),
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
                pes_position: false,
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

impl<'a> From<&'a UnframedAudio> for PacketProjection<'a> {
    fn from(audio: &'a UnframedAudio) -> Self {
        Self {
            pid: audio.track_info().stream().pid,
            clock: audio.clock(),
            epochs: audio.epochs(),
            payload: Payload::Framed {
                sources: audio.sources(),
                bytes: audio.bytes().as_ref(),
                pes_position: false,
            },
            flags: 1,
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
    let header = match (&packet.payload, packet.clock) {
        (Payload::PreservedPes(packet), _) if packet.clock_is_quarantined() => {
            return Err("unresolved source clock".into());
        }
        (Payload::PreservedPes(packet), _) => Some(&packet.pes().header),
        (_, Some(anchor)) if anchor.quarantined => return Err("unresolved source clock".into()),
        (_, Some(anchor)) => Some(
            &anchor
                .pes
                .event()
                .pes_source()
                .ok_or("packet clock has no PES")?
                .header,
        ),
        (_, None) => None,
    };
    let time = clock
        .project_header(packet.pid, packet.epochs, header, &mut work, cancel)?
        .ok_or("clock projection exhausted its work")?;
    let ticks = |value: Option<i128>| -> Result<i64> {
        Ok(value.map(i64::try_from).transpose()?.unwrap_or(i64::MIN))
    };
    let (pes, at_start, bytes) = match packet.payload {
        Payload::Framed {
            sources,
            bytes,
            pes_position,
        } => {
            let first = sources.first().ok_or("packet has no source PES")?;
            let pes = first
                .pes
                .event()
                .pes_source()
                .ok_or("packet payload has no PES")?;
            (
                pes,
                first.payload_range.start == 0 || pes_position,
                bytes.len(),
            )
        }
        Payload::PreservedPes(packet) => {
            let pes = packet.pes();
            (pes, true, pes.payload_len)
        }
    };
    let position = if at_start {
        let range = pes
            .header_source_ranges
            .as_slice()
            .first()
            .ok_or("PES has no header range")?;
        position.at(range.entity, range.offset)?
    } else {
        -1
    };
    let fields = [
        index as u64,
        u64::from(pes.pid),
        signed(ticks(time.pts90)?),
        signed(ticks(time.dts90)?),
        signed(duration),
        signed(position),
        packet.flags,
        bytes as u64,
    ];
    let stream_id = pes.header.stream_id;
    match packet.payload {
        Payload::Framed { bytes, .. } => {
            fold_payload(totals, fields, stream_id, std::iter::once(bytes))
        }
        Payload::PreservedPes(_) => fold_payload(totals, fields, stream_id, pes.payload()),
    }
}

fn diagnostic_refusal(fault: &triergon_mpegts::TraceFault) -> Refusal {
    Refusal {
        reason: format!("source diagnostic: {:?}", fault.kind),
        cause: format!(
            "{{\"phase\":\"diagnostic\",\"source\":{},\"offset\":{}}}",
            fault.location.source.0, fault.location.offset,
        ),
    }
}

fn fold_continuity(residue: &ContinuityResidue, position: PacketPosition<'_>) -> Result<Totals> {
    let first = residue
        .chunks()
        .first()
        .ok_or("unframed video has no carrier input")?;
    let time = residue.clock();
    let ticks = |value: Option<i128>| -> Result<u64> {
        Ok(signed(
            value.map(i64::try_from).transpose()?.unwrap_or(i64::MIN),
        ))
    };
    let mut totals = Totals::default();
    fold_payload(
        &mut totals,
        [
            0,
            u64::from(first.pid),
            ticks(time.pts90)?,
            ticks(time.dts90)?,
            0,
            signed(position.at(first.source_origin.source, first.source_origin.offset)?),
            0,
            residue.payload_len() as u64,
        ],
        first.stream_id,
        residue.payload(),
    )?;
    Ok(totals)
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
        unframed_audio_policy: UnframedAudioPolicy::ReportAtEof,
        adts_packet_policy: PacketPolicy::ReportFragments,
        adts_pes_policy: PesPolicy::ReportUnbounded,
        latm_packet_policy: latm::PacketPolicy::PreservePes,
        dvb_subtitle_tail_policy: DvbSubtitleTailPolicy::ReportCompletePrefix,
        ..ReaderLimits::default()
    };
    let mut reader = ContinuityReader::new(
        pool,
        FIRST_SOURCE,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        limits,
        WINDOW,
    )?;
    let mut clock = DemuxClock::new(request.program)?;
    let mut tracks = BudgetVec::<TrackProjection>::new(pool);
    let mut source = FIRST_SOURCE;
    let mut current = &request.input;
    let mut following = request.following.as_slice().iter();
    let single_entity = request.following.is_empty();
    let mut deferred_continuity = None;
    let mut recovered = None;
    let mut observed_media = false;
    let (proof, reader_refusal) = loop {
        let step = match reader.step(&mut WorkBudget::new(WORK), cancel) {
            Ok(step) => step,
            Err(error) => {
                return Ok(deferred_continuity.unwrap_or_else(|| refused(totals, &error, at)));
            }
        };
        match step {
            Step::Ready(
                Event::Packet(packet)
                | Event::Other(ContinuityEvent::Source(
                    ReaderEvent::VideoHeaders(packet)
                    | ReaderEvent::IncompleteAudio(packet)
                    | ReaderEvent::DvbSubtitlePrefix(packet)
                    | ReaderEvent::AdtsFragment(packet)
                    | ReaderEvent::RecoveredAdts(packet),
                )),
            ) => {
                if let Some(original) = deferred_continuity {
                    return Ok(original);
                }
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
                    // The reader already used lookahead to seal the E-AC-3 group.
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
            Step::Ready(Event::Other(ContinuityEvent::Source(
                event @ (ReaderEvent::IncompleteVideo(_) | ReaderEvent::UnframedAudio(_)),
            ))) => {
                observed_media = true;
                let (id, projection) = match &event {
                    ReaderEvent::IncompleteVideo(video) => (video.track_info().id(), video.into()),
                    ReaderEvent::UnframedAudio(audio) => (audio.track_info().id(), audio.into()),
                    _ => unreachable!("unframed observation"),
                };
                let index = tracks
                    .as_slice()
                    .iter()
                    .position(|value| value.id == id)
                    .ok_or("unframed observation has no registered track")?;
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
            Step::Ready(Event::Other(ContinuityEvent::Source(ReaderEvent::LatmPes(packet)))) => {
                observed_media = true;
                let id = packet.track_info().id();
                let index = tracks
                    .as_slice()
                    .iter()
                    .position(|value| value.id == id)
                    .ok_or("LATM packet has no registered track")?;
                if let Err(error) = fold_packet(
                    &mut totals,
                    (&packet).into(),
                    index,
                    position,
                    0,
                    &mut clock,
                    cancel,
                ) {
                    return Ok(
                        deferred_continuity.unwrap_or_else(|| refused(totals, error.as_ref(), at))
                    );
                }
            }
            Step::Ready(Event::Other(ContinuityEvent::Pending(event))) => {
                if deferred_continuity.is_none() {
                    let mut original = std::mem::take(&mut totals);
                    if let ReaderEvent::Source(source) = &event
                        && let TraceEvent::Diagnostic(fault) = source.event()
                    {
                        original
                            .refusal
                            .get_or_insert_with(|| Box::new(diagnostic_refusal(fault)));
                    } else {
                        original = refused(
                            original,
                            &format!("incomplete access-unit observation: {event:?}"),
                            at,
                        );
                    }
                    deferred_continuity = Some(original);
                }
            }
            Step::Ready(Event::Other(ContinuityEvent::Residue(residue))) => {
                recovered = Some(fold_continuity(&residue, position)?);
            }
            Step::Ready(Event::Other(ContinuityEvent::Source(
                ReaderEvent::Transport(_) | ReaderEvent::Continuity { .. },
            ))) => (),
            Step::Ready(Event::Other(ContinuityEvent::Source(ReaderEvent::Source(source)))) => {
                if let Some(pes) = source.event().pes_source() {
                    observed_media = true;
                    // Place the demux interval in source PES order before
                    // codec-parser lookahead delays individual packet output.
                    clock
                        .project(pes, &mut WorkBudget::new(WORK), cancel)?
                        .ok_or("source clock projection exhausted its work")?;
                    continue;
                }
                match source.event() {
                    TraceEvent::Diagnostic(fault) => {
                        let startup_pointer = single_entity
                            && !observed_media
                            && tracks.is_empty()
                            && totals.refusal.is_none()
                            && fault.kind == FaultKind::Psi
                            && fault.error == Error::Invalid("PSI pointer");
                        if let Some(original) = deferred_continuity {
                            return Ok(original);
                        }
                        totals
                            .refusal
                            .get_or_insert_with(|| Box::new(diagnostic_refusal(fault)));
                        // The section assembler discarded the invalid pointer.
                        // A later clean map and a framed packet can still prove
                        // the start of the selected program.
                        if startup_pointer {
                            continue;
                        }
                        if single_entity
                            && request.format == PacketFormat::Ts188
                            && totals.packets == 0
                            && let Some(trigger) = au_recovery::Trigger::from_fault(fault)
                        {
                            break (SourceProof::OpaqueRecovery(trigger), totals);
                        }
                        if single_entity && !observed_media {
                            match fault.kind {
                                FaultKind::Psi => break (SourceProof::ControlOnly, totals),
                                FaultKind::Transport | FaultKind::Pes
                                    if request.format == PacketFormat::Ts188
                                        && matches!(fault.error, Error::Invalid(_)) =>
                                {
                                    break (SourceProof::ReportedMalformed(fault.into()), totals);
                                }
                                _ => {}
                            }
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
            Step::Ready(Event::Other(ContinuityEvent::Source(ReaderEvent::Interrupted {
                cause,
                ..
            }))) if matches!(cause.event(), TraceEvent::Discontinuity { .. }) => (),
            Step::Ready(Event::Other(ContinuityEvent::Source(event))) => {
                if let Some(original) = deferred_continuity {
                    return Ok(original);
                }
                let recovery = match &event {
                    ReaderEvent::Interrupted { cause, .. }
                        if single_entity
                            && request.format == PacketFormat::Ts188
                            && totals.packets == 0 =>
                    {
                        match cause.event() {
                            TraceEvent::Diagnostic(fault) => {
                                au_recovery::Trigger::from_fault(fault)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                let refusal = refused(
                    totals,
                    &format!("incomplete access-unit observation: {event:?}"),
                    at,
                );
                if let Some(trigger) = recovery {
                    break (SourceProof::OpaqueRecovery(trigger), refusal);
                }
                return Ok(refusal);
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
                        let transition = if next.discontinuity {
                            EntityTransition::Discontinuity
                        } else {
                            EntityTransition::Contiguous
                        };
                        reader.finish_entity(
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
                return Ok(deferred_continuity.unwrap_or_else(|| {
                    refused(
                        totals,
                        &Error::Backpressure {
                            requested: pressure.requested,
                            available: pressure.available,
                        },
                        at,
                    )
                }));
            }
            Step::End(end) => {
                if end.source != source
                    || end.offset != current.len() as u64
                    || following.len() != 0
                {
                    return Err("reader ended before the physical input".into());
                }
                if let Some(recovered) = recovered {
                    return Ok(recovered);
                }
                if let Some(original) = deferred_continuity {
                    return Ok(original);
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
                    if single_entity && !observed_media {
                        break (SourceProof::ControlOnly, refusal);
                    }
                    return Ok(refusal);
                }
                // The only deferred fault is a pointer discarded before track
                // admission. Reaching physical EOF with framed media clears it.
                totals.refusal = None;
                return Ok(totals);
            }
        }
    };

    // The compatibility proof examines the whole original entity. Release the
    // completed reader attempt before reserving storage for the replay.
    drop(reader);
    drop(tracks);
    Ok(prove_source(pool, request, proof, cancel)?.unwrap_or(reader_refusal))
}

#[cold]
#[inline(never)]
fn prove_source(
    pool: &Budget,
    request: &Request,
    proof: SourceProof,
    cancel: &AtomicBool,
) -> Result<Option<Totals>> {
    match proof {
        SourceProof::ControlOnly => {
            Ok(confirm_control_only_entity(pool, request, cancel)?.map(|_| Totals::default()))
        }
        SourceProof::ReportedMalformed(diagnostic) => {
            Ok(malformed::confirm(pool, request, diagnostic, cancel)?.map(|_| Totals::default()))
        }
        SourceProof::OpaqueRecovery(trigger) => {
            au_recovery::project(pool, request, trigger, cancel)
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
    use super::{Request, Totals, operation, ts_input};
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use triergon_core::{Budget, BudgetVec, Bytes};
    use triergon_mpegts::PacketFormat;
    use triergon_mpegts::psi::DescriptorNamespace;

    const MALFORMED_ENTITIES: [&[u8]; 4] = [
        include_bytes!("../../mpegts/tests/data/transport-afc-reserved.ts"),
        include_bytes!("../../mpegts/tests/data/transport-adaptation-overrun.ts"),
        include_bytes!("../../mpegts/tests/data/pes-optional-header-overrun.ts"),
        include_bytes!("../../mpegts/tests/data/pes-start-prefix-invalid.ts"),
    ];

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
    fn control_only_entity_completes_after_a_psi_diagnostic_or_empty_end() {
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
    fn control_only_completion_requires_one_entity() {
        let pool = Budget::new(1 << 20).unwrap();
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../mpegts/tests/data/psi-crc-invalid.ts");
        let request = ts_input::parse(
            &pool,
            &path,
            &[
                "188".into(),
                "1".into(),
                "pat-network-pid-not-track.ts".into(),
            ],
        )
        .unwrap();
        let totals = operation(&pool, &request, &AtomicBool::new(false)).unwrap();
        assert_eq!(totals.packets, 0);
        assert_eq!(totals.refusal.unwrap().reason, "source diagnostic: Psi");
    }

    #[test]
    fn reported_malformed_entity_completes_without_a_packet() {
        for input in MALFORMED_ENTITIES {
            let totals = fold_single(input);
            assert!(totals.refusal.is_none());
            assert_eq!((totals.packets, totals.bytes, totals.digest), (0, 0, 0));
        }
    }

    #[test]
    fn later_valid_pes_prevents_reported_malformed_completion() {
        let valid = include_bytes!("../../mpegts/tests/data/ts188-basic.ts");
        for input in MALFORMED_ENTITIES {
            let mut raw = input.to_vec();
            let mut packet = valid[376..564].to_vec();
            packet[3] = packet[3] & 0xf0 | 1;
            raw.extend_from_slice(&packet);
            let totals = fold_single(&raw);
            assert_eq!(totals.packets, 0);
            let refusal = totals.refusal.unwrap();
            assert!(refusal.reason.starts_with("source diagnostic:"));
            assert!(refusal.cause.contains("\"phase\":\"diagnostic\""));
        }
    }

    #[test]
    fn opaque_continuity_residue_keeps_copy_and_timeline_order_through_eof() {
        let copy = fold_single(include_bytes!("../../mpegts/tests/data/cc-third-copy.ts"));
        assert!(copy.refusal.is_none());
        assert_eq!(
            (copy.packets, copy.bytes, copy.digest),
            (1, 81, 10_013_462_900_046_159_905)
        );
        let timeline = fold_single(include_bytes!(
            "../../mpegts/tests/data/pcr-timeline-discontinuity.ts"
        ));
        assert!(timeline.refusal.is_none());
        assert_eq!(
            (timeline.packets, timeline.bytes, timeline.digest),
            (1, 30, 1_602_270_805_323_916_750)
        );
    }

    #[test]
    fn start_code_across_compatibility_carriers_blocks_the_residue() {
        let mut input = include_bytes!("../../mpegts/tests/data/cc-one-duplicate.ts").to_vec();
        // Each payload alone is opaque. A start code straddles their
        // compatibility join even though the strict stream deduplicates it.
        for end in [564, 752] {
            input[end - 27] = 1;
            input[end - 2..end].fill(0);
        }
        let totals = fold_single(&input);
        assert!(totals.refusal.is_none());
        assert_eq!((totals.packets, totals.bytes), (1, 27));
    }

    #[test]
    fn annex_b_prefix_with_a_recoverable_diagnostic_stays_refused() {
        let mut raw = include_bytes!("../../mpegts/tests/data/pes-timestamp-marker.ts").to_vec();
        let start = raw.len() - 4;
        raw[start..].copy_from_slice(&[0, 0, 1, 0x65]);
        let totals = fold_single(&raw);
        assert_eq!(totals.packets, 0);
        assert_eq!(totals.refusal.unwrap().reason, "source diagnostic: Pes");
    }

    #[test]
    fn recovered_opaque_finite_continuation_uses_every_payload_byte() {
        let input =
            include_bytes!("../../../harness/fixtures/mpegts/finite-continuation-body-401.ts");
        let totals = fold_single(input);
        assert!(totals.refusal.is_none());
        assert_eq!((totals.packets, totals.bytes), (1, 401));
        assert_eq!(totals.digest, 16_203_249_262_521_375_477);

        let mut raw = input.to_vec();
        // Put a start code across the retained PES and its headerless continuation.
        let next_carrier = 4 * 188;
        let continuation = next_carrier + 5 + usize::from(raw[next_carrier + 4]);
        raw[next_carrier - 2..next_carrier].fill(0);
        raw[continuation..continuation + 2].copy_from_slice(&[1, 0x65]);
        let totals = fold_single(&raw);
        assert_eq!(totals.packets, 0);
        assert_eq!(totals.refusal.unwrap().reason, "source diagnostic: Pes");
    }
}
