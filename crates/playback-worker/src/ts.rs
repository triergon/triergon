//! The FFmpeg-compatible packets of one MPEG-TS program through the public API.
//!
//! The strict source view (`TransportStream`) parses the input and selects the
//! program. Its first active PMT binds a `compat::pes::DemuxObserver` to the
//! program's one elementary stream; the transport packets of that PID feed the
//! observer through `TransportFaultRecovery`, and `compat::pes::DemuxClock`
//! places each packet's timestamps. Faulted extraction retains the actual source
//! diagnostic and never becomes a validated source PES.
//!
//! Ordinary entity boundaries preserve PES assembly and the demux clock. A
//! declared discontinuity requires drained assembly and a fresh source map
//! for the same stream. Other program, clock, format and recovery
//! contexts outside this scope stop with a refusal. A single-entity
//! program-policy refusal with no emitted packets
//! can instead complete when `PacketCompletion` proves every physical carrier is
//! control-only and the source reached actual EOF. The proof runs inside the
//! operation and never changes source validation.
//! Selected finite AVC can acknowledge a real source PCR Timeline or exclude one
//! unusable final carrier after physical EOF and source Finished confirmation.
//!
//! Each packet folds its stream index, PID, PTS, DTS, duration, position, flags,
//! payload and side data, the same fields in the same order as the reference
//! worker (`harness/reference_ts.h`).

use crate::runner::{Invocation, MAX_INPUT, Refusal, Result, Totals, measure};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceId, Step, WorkBudget,
};
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_mpegts::compat::pes::{
    DemuxClock, DemuxObserver, DemuxPacket, DemuxProfile, DemuxScope, DvbSubtitlesDescriptors,
    MalformedInputCompletion, MalformedSourceDisposition, NoMediaConfirmation, PacketCompletion,
    PesFaultPolicy, PrivateAudioCarriage, PrivateAudioDescriptors, RegisteredCarriage,
    SourceContext, TransportFaultRecovery, TransportObservation,
};
use triergon_mpegts::psi::{DescriptorNamespace, Descriptors, DvbNamespaceEvidence, ProgramMap};
use triergon_mpegts::{
    AssemblyStep, FaultKind, MediaEntityEnd, ObservedTrace, PacketFormat, ParseLimits,
    SourceEpochs, SourceRange, TraceEvent, TransportStream,
};

pub const PREFIX: &str = "ts-";
const OPERATION: &str = "mpegts-pes-packets";
const PROFILE: DemuxProfile = DemuxProfile::PesPackets;
const WINDOW: usize = 64 * 1024;
const WORK: u64 = 4096;
/// Matches the entity count limit of the public reference adapter.
const MAX_ENTITIES: usize = 64;
/// FFmpeg's `AV_NOPTS_VALUE`, folded for an absent timestamp on both sides.
const NO_TIME: i64 = i64::MIN;
/// FFmpeg's `AV_PKT_DATA_MPEGTS_STREAM_ID` side-data type.
const STREAM_ID_SIDE_DATA: u64 = 19;
/// What every refusal of a context outside the observer's profile names.
const UNROUTED: &str = "needs a program-level reader";

/// Fold one packet with its demuxer timestamps in 90 kHz units.
fn fold_packet(
    totals: &mut Totals,
    packet: &DemuxPacket,
    pts90: Option<i64>,
    dts90: Option<i64>,
    position: i64,
) -> Result<()> {
    // One elementary stream: stream index zero.
    let fields = [
        0,
        u64::from(packet.pid),
        signed(pts90.unwrap_or(NO_TIME)),
        signed(dts90.unwrap_or(NO_TIME)),
        signed(packet.duration90_raw),
        signed(position),
        u64::from(packet.flags),
        packet.payload_len as u64,
    ];
    fold_payload(totals, fields, packet.stream_id, packet.payload())
}

fn fold_payload<'a>(
    totals: &mut Totals,
    fields: [u64; 8],
    stream_id: u8,
    payload: impl Iterator<Item = &'a [u8]>,
) -> Result<()> {
    let digest = &mut totals.digest;
    for value in fields {
        fold(digest, value);
    }
    let mut seen = 0;
    for span in payload {
        fold_bytes(digest, span);
        seen += span.len();
    }
    if seen as u64 != fields[7] {
        return Err("packet payload does not match its length".into());
    }
    // One side-data element: the PES stream ID.
    for value in [1, STREAM_ID_SIDE_DATA, 1, u64::from(stream_id)] {
        fold(digest, value);
    }
    totals.packets += 1;
    totals.bytes += seen as u64;
    Ok(())
}

/// A demuxer timestamp as FFmpeg's 64-bit packet field.
fn time64(value: Option<i128>) -> triergon_core::Result<Option<i64>> {
    value
        .map(|value| i64::try_from(value).map_err(|_| Error::Overflow))
        .transpose()
}

/// The source event or boundary a refusal is attributed to. Ordinals count the
/// source view's events from zero.
#[derive(Clone, Copy)]
struct Cause {
    phase: &'static str,
    ordinal: Option<u64>,
    source: SourceId,
    offset: Option<u64>,
    epochs: SourceEpochs,
}

impl Cause {
    fn event(event: &TraceEvent, ordinal: u64, entity: SourceId, epochs: SourceEpochs) -> Self {
        let range = |range: Option<&SourceRange>| range.map(|range| (range.entity, range.offset));
        let (phase, located) = match event {
            TraceEvent::Transport(packet) => (
                "transport",
                Some((packet.source_id.unwrap_or(entity), packet.source_offset)),
            ),
            TraceEvent::Continuity { .. } => ("continuity", None),
            TraceEvent::Section(section) => ("section", range(section.source_ranges().first())),
            TraceEvent::Pat(table) => (
                "pat",
                range(
                    table
                        .source_ranges
                        .as_slice()
                        .first()
                        .map(|item| &item.range),
                ),
            ),
            TraceEvent::ProgramMap { map, .. } => {
                ("pmt", range(map.source_ranges.as_slice().first()))
            }
            TraceEvent::Pes(pes) => ("pes", range(pes.header_source_ranges.as_slice().first())),
            TraceEvent::Timeline { .. } => ("timeline", None),
            TraceEvent::Discontinuity { .. } => ("discontinuity", None),
            TraceEvent::Diagnostic(fault) => (
                "diagnostic",
                Some((fault.location.source, fault.location.offset)),
            ),
            TraceEvent::Finished => ("finished", None),
        };
        Self {
            phase,
            ordinal: Some(ordinal),
            source: located.map_or(entity, |(source, _)| source),
            offset: located.map(|(_, offset)| offset),
            epochs,
        }
    }

    fn boundary(phase: &'static str, source: SourceId, offset: u64, epochs: SourceEpochs) -> Self {
        Self {
            phase,
            ordinal: None,
            source,
            offset: Some(offset),
            epochs,
        }
    }

    fn context(self) -> triergon_core::Result<SourceContext> {
        Ok(SourceContext {
            source_event_ordinal: self
                .ordinal
                .ok_or(Error::Invalid("source event ordinal missing"))?,
            epochs: self.epochs,
            byte_run_epoch: None,
        })
    }

    fn refusal(self, error: &dyn std::fmt::Display) -> Refusal {
        let null = |value: Option<u64>| value.map_or("null".into(), |value| value.to_string());
        Refusal {
            reason: error.to_string(),
            cause: format!(
                "{{\"phase\":\"{}\",\"source_event_ordinal\":{},\"source\":{},\"offset\":{},\"clock_epoch\":{},\"configuration_epoch\":{}}}",
                self.phase,
                null(self.ordinal),
                self.source.0,
                null(self.offset),
                self.epochs.clock,
                self.epochs.configuration,
            ),
        }
    }
}

/// The observer of the program's one elementary stream, built from its PMT with the
/// constructor its carriage selects.
fn observer(
    input: &InputCursor,
    program: u16,
    map: &ProgramMap,
    cause: Cause,
) -> triergon_core::Result<TransportFaultRecovery> {
    let pool = input.budget();
    let unrouted = Error::Unsupported(UNROUTED);
    let [stream] = map.streams.as_slice() else {
        return Err(unrouted);
    };
    if map.header.extension != program {
        return Err(unrouted);
    }
    let descriptors = PrivateAudioDescriptors {
        program: map.program_descriptors.as_ref(),
        elementary: stream.descriptor_bytes.as_ref(),
    };
    let has_tag = |tags: &[u8]| {
        Descriptors::new(descriptors.elementary)
            .any(|entry| entry.is_ok_and(|view| tags.contains(&view.tag)))
    };
    let registered = match stream.stream_type {
        0x1b => Some(RegisteredCarriage::Avc),
        0x24 => Some(RegisteredCarriage::Hevc),
        0x0f => Some(RegisteredCarriage::AacAdts),
        0x11 => Some(RegisteredCarriage::AacLatm),
        _ => None,
    };
    // DVB subtitles need the declared namespace and a subtitling descriptor; AC-3
    // and E-AC-3 a DVB tag or their ATSC stream type. Nothing else is inferred.
    let dvb_subtitles = stream.stream_type == 6
        && matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_))
        && has_tag(&[0x59]);
    let private_audio = matches!(stream.stream_type, 0x81 | 0x87)
        || (stream.stream_type == 6 && !dvb_subtitles && has_tag(&[0x6a, 0x7a]));
    let stream_id = match registered {
        Some(RegisteredCarriage::AacAdts | RegisteredCarriage::AacLatm) => 0xc0,
        Some(_) => 0xe0,
        None => 0xbd,
    };
    let scope = DemuxScope {
        program,
        pid: stream.pid,
        stream_type: stream.stream_type,
        stream_id,
        entity: input.source(),
        epochs: cause.epochs,
    };
    let (budget, limits) = (pool.clone(), ParseLimits::default());
    let observer = if let Some(carriage) = registered {
        DemuxObserver::new_registered(PROFILE, scope, carriage, budget, limits)
    } else if dvb_subtitles {
        DemuxObserver::new_dvb_subtitles(
            PROFILE,
            scope,
            map.descriptor_namespace,
            DvbSubtitlesDescriptors {
                program: descriptors.program,
                elementary: descriptors.elementary,
            },
            budget,
            limits,
        )
    } else if private_audio {
        let carriage =
            PrivateAudioCarriage::from_observed_pmt(stream.stream_type, descriptors, limits)?;
        DemuxObserver::new_private_audio(PROFILE, scope, carriage, descriptors, budget, limits)
    } else {
        Err(unrouted)
    }?;
    let mut recovery = TransportFaultRecovery::with_policy(
        observer,
        PesFaultPolicy {
            timing_headers: true,
            rejected_clock_fields: true,
            source_pcr_pid: Some(map.pcr_pid),
            terminal_transport: true,
        },
    )
    .map_err(|rejected| rejected.error)?;
    recovery.bind_source_input(input)?;
    Ok(recovery)
}

/// The observer, its clock and the folded packets of one operation.
struct Session {
    clock: DemuxClock,
    observer: Option<TransportFaultRecovery>,
    route: Option<DemuxScope>,
    discontinuity: Option<SourceEpochs>,
    entity_base: i64,
    observer_base: i64,
    finished: bool,
    totals: Totals,
}

impl Session {
    fn new(program: u16) -> Result<Self> {
        Ok(Self {
            clock: DemuxClock::new(program)?,
            observer: None,
            route: None,
            discontinuity: None,
            entity_base: 0,
            observer_base: 0,
            finished: false,
            totals: Totals::default(),
        })
    }

    fn refusal(mut self, error: &dyn std::fmt::Display, cause: Cause) -> Totals {
        self.totals.refusal = Some(Box::new(cause.refusal(error)));
        self.totals
    }

    /// Route one source event: the first active PMT binds the observer, the
    /// selected PID's transport packets feed it, and every context the observer
    /// alone cannot take is refused.
    fn observe(
        &mut self,
        input: &InputCursor,
        program: u16,
        event: &TraceEvent,
        cause: Cause,
        quarantined: bool,
    ) -> triergon_core::Result<()> {
        let unrouted = || Error::Unsupported(UNROUTED);
        let boundary_error = |error| match error {
            Error::Unsupported(_) => unrouted(),
            error => error,
        };
        if let Some(expected) = self.discontinuity {
            if !matches!(event, TraceEvent::Discontinuity { epochs } if *epochs == expected)
                || input.position() != 0
            {
                return Err(Error::Invalid("source discontinuity not observed"));
            }
            self.discontinuity = None;
            return Ok(());
        }
        if let Some(observer) = &self.observer {
            let scope = observer.scope();
            // A spanning PES or PSI event may report its origin in an older
            // entity. The observer binds the current physical input.
            if scope.entity != input.source() || quarantined {
                return Err(unrouted());
            }
            if matches!(event, TraceEvent::Timeline { .. }) {
                return self
                    .observer
                    .as_mut()
                    .ok_or_else(unrouted)?
                    .confirm_source_timeline(event, cause.context()?)
                    .map_err(boundary_error);
            }
            if scope.epochs != cause.epochs
                || (observer.awaiting_terminal_completion()
                    && !matches!(event, TraceEvent::Finished))
            {
                return Err(unrouted());
            }
        }
        match event {
            TraceEvent::ProgramMap {
                map, active: true, ..
            } => {
                if self.observer.is_some() || quarantined {
                    return Err(unrouted());
                }
                let observer = observer(input, program, map, cause)?;
                let scope = observer.scope();
                if self.route.is_some_and(|old| {
                    (old.pid, old.stream_type, old.stream_id)
                        != (scope.pid, scope.stream_type, scope.stream_id)
                }) {
                    return Err(unrouted());
                }
                self.route = Some(scope);
                self.observer_base = self.entity_base;
                self.observer = Some(observer);
            }
            TraceEvent::Transport(packet) => {
                if let Some(observer) = self.observer.as_mut()
                    && packet.pid == observer.scope().pid
                {
                    observer
                        .push(packet.clone(), cause.context()?)
                        .map_err(|rejected| rejected.error)?;
                }
            }
            TraceEvent::Timeline { .. } | TraceEvent::Discontinuity { .. } => {
                return Err(unrouted());
            }
            TraceEvent::Diagnostic(fault)
                if self
                    .observer
                    .as_ref()
                    .is_some_and(TransportFaultRecovery::awaiting_source_fault) =>
            {
                self.observer
                    .as_mut()
                    .ok_or_else(unrouted)?
                    .confirm_source_fault(fault, cause.context()?)?;
            }
            TraceEvent::Diagnostic(fault)
                if fault.rejected.is_some()
                    && matches!(
                        fault.error,
                        Error::Invalid("PCR reserved bits/extension" | "OPCR without PCR")
                    ) =>
            {
                self.observer
                    .as_mut()
                    .ok_or_else(unrouted)?
                    .push_rejected_fault(fault.clone(), cause.context()?)
                    .map_err(|rejected| rejected.error)?;
            }
            TraceEvent::Diagnostic(fault)
                if fault.kind == FaultKind::Transport
                    && matches!(
                        fault.error,
                        Error::Truncated | Error::Invalid("TS sync byte")
                    ) =>
            {
                self.observer
                    .as_mut()
                    .ok_or_else(unrouted)?
                    .hold_terminal_transport(fault, cause.context()?)
                    .map_err(boundary_error)?;
            }
            TraceEvent::Diagnostic(fault)
                if matches!(fault.kind, FaultKind::Transport | FaultKind::Program)
                    || (fault.kind == FaultKind::Pes
                        && matches!(
                            fault.error,
                            Error::Invalid("PES timestamp markers" | "forbidden PES time flags")
                        ))
                    || (fault.kind == FaultKind::Psi && self.observer.is_some()) =>
            {
                return Err(unrouted());
            }
            // Loss, duplicate and truncation diagnostics stay with the source view;
            // the observer sees the real carriers, never the source PES.
            _ => {}
        }
        Ok(())
    }

    fn drain_after_event(&mut self, event: &TraceEvent, cancel: &AtomicBool) -> Result<()> {
        // Continuity and strict PES events leave the drained observer unchanged.
        if !matches!(event, TraceEvent::Continuity { .. } | TraceEvent::Pes(_)) {
            return self.drain(cancel);
        }
        if self.observer.is_some() && cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled.into());
        }
        Ok(())
    }

    /// Fold every packet the observer has ready.
    fn drain(&mut self, cancel: &AtomicBool) -> Result<()> {
        let Some(observer) = self.observer.as_mut() else {
            return Ok(());
        };
        // Observer positions are relative to its first entity. The reference
        // AVIO sees sequence positions even after an observer is replaced.
        let base = self.observer_base;
        let position = |offset| base.checked_add(offset).ok_or(Error::Overflow);
        loop {
            match observer.poll(&mut WorkBudget::new(WORK), cancel)? {
                AssemblyStep::Output(TransportObservation::Accepted(packet)) => {
                    let time = self
                        .clock
                        .project_header(
                            packet.pid,
                            packet.source_epochs,
                            packet.time_header(),
                            &mut WorkBudget::new(1),
                            cancel,
                        )?
                        .ok_or("clock yielded on a fixed grant")?;
                    fold_packet(
                        &mut self.totals,
                        &packet,
                        time64(time.pts90)?,
                        time64(time.dts90)?,
                        position(packet.position)?,
                    )?;
                }
                AssemblyStep::Output(TransportObservation::Faulted(packet)) => {
                    let time = self
                        .clock
                        .project_transport_fault(&packet, &mut WorkBudget::new(1), cancel)?
                        .ok_or("clock yielded on a fixed grant")?;
                    fold_payload(
                        &mut self.totals,
                        [
                            0,
                            u64::from(packet.pid),
                            signed(i64::try_from(time.pts90)?),
                            signed(i64::try_from(time.dts90)?),
                            signed(packet.duration90_raw),
                            signed(position(packet.position)?),
                            u64::from(packet.flags),
                            packet.payload_len as u64,
                        ],
                        packet.stream_id,
                        packet.payload(),
                    )?;
                }
                AssemblyStep::Output(TransportObservation::PesFault(packet)) => {
                    let time = self
                        .clock
                        .project_pes_fault(&packet, &mut WorkBudget::new(1), cancel)?
                        .ok_or("clock yielded on a fixed grant")?;
                    fold_payload(
                        &mut self.totals,
                        [
                            0,
                            u64::from(packet.pid),
                            signed(time64(time.pts90)?.unwrap_or(NO_TIME)),
                            signed(time64(time.dts90)?.unwrap_or(NO_TIME)),
                            signed(packet.duration90_raw),
                            signed(position(packet.position)?),
                            u64::from(packet.flags),
                            packet.payload_len as u64,
                        ],
                        packet.stream_id,
                        packet.payload(),
                    )?;
                }
                AssemblyStep::Yield => {}
                AssemblyStep::NeedPayload => return Ok(()),
                AssemblyStep::Finished => {
                    self.finished = true;
                    return Ok(());
                }
            }
        }
    }

    fn continue_entity(
        &mut self,
        demux: &mut TransportStream,
        end: &MediaEntityEnd,
        input: &mut InputCursor,
        next: &Entity,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if demux.clock_is_quarantined() {
            return Err(Error::Unsupported(UNROUTED));
        }
        if next.discontinuity
            && self
                .observer
                .as_ref()
                .is_some_and(|observer| !observer.discard_idle())
        {
            return Err(Error::Unsupported(
                "discontinuity requires a drained PES observer",
            ));
        }
        let base = self
            .entity_base
            .checked_add(i64::try_from(end.location().offset).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        input.continue_source(next.source, capabilities(&next.input))?;
        if let Some(observer) = self.observer.as_mut() {
            if !observer.continue_media_entity(end, input, &mut WorkBudget::new(WORK), cancel)? {
                return Err(Error::Invalid(
                    "media entity continuation yielded on fixed grant",
                ));
            }
        } else {
            end.validate_continuation(input)?;
        }
        if next.discontinuity {
            demux.discontinuity()?;
            self.observer = None;
            self.discontinuity = Some(demux.source_epochs());
        }
        self.entity_base = base;
        Ok(())
    }

    fn seal_terminal_end(
        &mut self,
        end: &MediaEntityEnd,
        input: &mut InputCursor,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if let Some(observer) = self.observer.as_mut()
            && observer.awaiting_terminal_completion()
            && !observer.seal_terminal_end(end, input, &mut WorkBudget::new(WORK), cancel)?
        {
            return Err("terminal End yielded on fixed grant".into());
        }
        Ok(())
    }

    fn confirm_terminal_finished(
        &mut self,
        cause: Cause,
        input: &mut InputCursor,
        cancel: &AtomicBool,
        witness: impl FnOnce(&InputCursor) -> triergon_core::Result<ObservedTrace<()>>,
    ) -> triergon_core::Result<bool> {
        if let Some(observer) = self.observer.as_mut()
            && observer.awaiting_terminal_completion()
        {
            let finished = witness(input)?;
            return observer.confirm_terminal_finished(
                &finished,
                cause.context()?,
                input,
                &mut WorkBudget::new(WORK),
                cancel,
            );
        }
        Ok(true)
    }

    fn logical_end(&mut self, cancel: &AtomicBool) -> Result<()> {
        self.observer
            .as_mut()
            .ok_or(Error::Unsupported(UNROUTED))?
            .end_of_source()?;
        self.drain(cancel)?;
        if !self.finished {
            return Err("logical EOF did not finish the observer".into());
        }
        Ok(())
    }
}

struct Entity {
    source: SourceId,
    input: Bytes,
    discontinuity: bool,
}

fn capabilities(input: &Bytes) -> SourceCapabilities {
    SourceCapabilities {
        length: Some(input.len() as u64),
        random_access: false,
    }
}

/// A preloaded sequence of actual media entities, all charged to one pool.
struct Request {
    format: PacketFormat,
    program: u16,
    namespace: DescriptorNamespace,
    input: Bytes,
    following: BudgetVec<Entity>,
}

struct PacketAttempt {
    totals: Totals,
    try_control_completion: bool,
}

/// Keep the packet path's refusal unless a separate physical-source proof
/// confirms this entire single entity contained no media.
fn operation(pool: &Budget, request: &Request, cancel: &AtomicBool) -> Result<Totals> {
    if !request.following.is_empty() {
        return sequence_operation(pool, request, cancel);
    }
    let attempt = packet_operation(pool, request, cancel)?;
    if attempt.try_control_completion
        && attempt.totals.packets == 0
        && control_completion(pool, request, cancel)?.is_some()
    {
        return Ok(Totals::default());
    }
    Ok(attempt.totals)
}

/// One packet attempt: its fold, or the typed refusal that ended it.
fn packet_operation(
    pool: &Budget,
    request: &Request,
    cancel: &AtomicBool,
) -> Result<PacketAttempt> {
    let raw = &request.input;
    let mut input = InputCursor::new(pool, SourceId(1), capabilities(raw), WINDOW)?;
    let mut malformed = MalformedInputCompletion::new_with_namespace(
        &input,
        request.format,
        ParseLimits::default(),
        request.program,
        request.namespace,
    )?;
    let mut session = Session::new(request.program)?;
    let mut logical_eof = None;
    let mut no_packets = None;
    let refused =
        |session: Session, error: &dyn std::fmt::Display, cause: Cause, try_control_completion| {
            Ok(PacketAttempt {
                totals: session.refusal(error, cause),
                try_control_completion,
            })
        };
    loop {
        let step = match malformed.poll_source(&mut input, &mut WorkBudget::new(WORK), cancel) {
            Ok(step) => step,
            Err(error) => {
                let cause = Cause::boundary(
                    "source_poll_error",
                    input.source(),
                    input.position(),
                    malformed.source_epochs(),
                );
                return refused(session, &error, cause, false);
            }
        };
        match step {
            Step::Ready(mut observed) => {
                let context = observed.context();
                let cause = Cause::event(
                    observed.event(),
                    context.source_event_ordinal,
                    observed.input().source(),
                    context.epochs,
                );
                let grant = observed.required_work()?;
                let disposition = observed.observe(
                    session.observer.as_mut(),
                    &mut WorkBudget::new(grant),
                    cancel,
                );
                let routed = match disposition {
                    Ok(Some(MalformedSourceDisposition::Forward)) => session.observe(
                        observed.input(),
                        request.program,
                        observed.event(),
                        cause,
                        observed.clock_is_quarantined(),
                    ),
                    Ok(Some(
                        MalformedSourceDisposition::AwaitDiagnostic
                        | MalformedSourceDisposition::Reported(_),
                    )) => Ok(()),
                    Ok(None) => {
                        return Err("malformed completion yielded on its required grant".into());
                    }
                    Err(error) => Err(error),
                };
                if let Err(error) = routed {
                    let eligible = error == Error::Unsupported(UNROUTED);
                    return refused(session, &error, cause, eligible);
                }
                if let Err(error) = session.drain_after_event(observed.event(), cancel) {
                    return refused(session, &error, cause, false);
                }
                if matches!(observed.event(), TraceEvent::Finished) {
                    drop(observed);
                    let Some(eof) = logical_eof else {
                        let error = Error::Invalid("source finished without logical EOF");
                        return refused(session, &error, cause, false);
                    };
                    match session.confirm_terminal_finished(cause, &mut input, cancel, |input| {
                        malformed.source_finished(input)
                    }) {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err("terminal confirmation yielded on fixed grant".into());
                        }
                        Err(error) => return refused(session, &error, cause, false),
                    }
                    if malformed.discarded_carriers() != 0 && session.totals.packets == 0 {
                        match malformed.confirm_no_packets(
                            session.observer.as_mut(),
                            &mut input,
                            &mut WorkBudget::new(WORK),
                            cancel,
                        ) {
                            Ok(Some(confirmation)) => no_packets = Some(confirmation),
                            Ok(None) => {
                                return Err(
                                    "malformed completion terminal yielded on fixed grant".into()
                                );
                            }
                            Err(error) => return refused(session, &error, cause, false),
                        }
                    }
                    if let Err(error) = session.logical_end(cancel) {
                        let eligible = matches!(
                            error.downcast_ref::<Error>(),
                            Some(Error::Unsupported(UNROUTED))
                        );
                        return refused(session, &error, eof, eligible);
                    }
                    break;
                }
            }
            Step::Yield => {}
            Step::NeedInput(request) => provide_input(&mut input, raw, request)?,
            Step::End(location) => {
                let epochs = malformed.source_epochs();
                let boundary =
                    |phase| Cause::boundary(phase, location.source, location.offset, epochs);
                let physical_end = malformed
                    .seal_physical_end(&mut input, &mut WorkBudget::new(WORK), cancel)?
                    .ok_or("source End yielded on fixed grant")?;
                session.seal_terminal_end(&physical_end, &mut input, cancel)?;
                logical_eof = Some(boundary("logical_eof"));
                malformed.end_of_source()?;
            }
        }
    }
    if session.totals.packets == 0 && no_packets.is_none() {
        let error = Error::Unsupported("empty observation is not acceptance");
        let cause = Cause::boundary(
            "logical_eof",
            input.source(),
            input.position(),
            malformed.source_epochs(),
        );
        return refused(session, &error, cause, true);
    }
    Ok(PacketAttempt {
        totals: session.totals,
        try_control_completion: false,
    })
}

fn sequence_operation(pool: &Budget, request: &Request, cancel: &AtomicBool) -> Result<Totals> {
    let mut raw = &request.input;
    let mut following = request.following.as_slice().iter();
    let mut input = InputCursor::new(pool, SourceId(1), capabilities(raw), WINDOW)?;
    let mut demux = TransportStream::new_with_namespace(
        pool.clone(),
        request.format,
        ParseLimits::default(),
        request.program,
        request.namespace,
    )?;
    let mut session = Session::new(request.program)?;
    let mut ordinal = 0_u64;
    let mut logical_eof = None;
    loop {
        let step = match demux.poll(&mut input, &mut WorkBudget::new(WORK), cancel) {
            Ok(step) => step,
            Err(error) => {
                let cause = Cause::boundary(
                    "source_poll_error",
                    input.source(),
                    input.position(),
                    demux.source_epochs(),
                );
                return Ok(session.refusal(&error, cause));
            }
        };
        match step {
            Step::Ready(event) => {
                let cause = Cause::event(&event, ordinal, input.source(), demux.source_epochs());
                ordinal += 1;
                if let Err(error) = session.observe(
                    &input,
                    request.program,
                    &event,
                    cause,
                    demux.clock_is_quarantined(),
                ) {
                    return Ok(session.refusal(&error, cause));
                }
                if let Err(error) = session.drain_after_event(&event, cancel) {
                    return Ok(session.refusal(&error, cause));
                }
                if matches!(event, TraceEvent::Finished) {
                    let Some(eof) = logical_eof else {
                        let error = Error::Invalid("source finished without logical EOF");
                        return Ok(session.refusal(&error, cause));
                    };
                    match session.confirm_terminal_finished(cause, &mut input, cancel, |input| {
                        demux.source_finished(input)
                    }) {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err("terminal confirmation yielded on fixed grant".into());
                        }
                        Err(error) => return Ok(session.refusal(&error, cause)),
                    }
                    if let Err(error) = session.logical_end(cancel) {
                        return Ok(session.refusal(&error, eof));
                    }
                    if session.totals.packets == 0 {
                        let error = Error::Unsupported("empty observation is not acceptance");
                        return Ok(session.refusal(&error, eof));
                    }
                    return Ok(session.totals);
                }
            }
            Step::Yield => {}
            Step::NeedInput(request) => provide_input(&mut input, raw, request)?,
            Step::End(location) => {
                let physical_end = demux
                    .media_entity_end(&mut input, &mut WorkBudget::new(WORK), cancel)?
                    .ok_or("source End yielded on fixed grant")?;
                if let Some(next) = following.next() {
                    let cause = Cause::boundary(
                        "media_entity_continuation",
                        location.source,
                        location.offset,
                        physical_end.epochs(),
                    );
                    if let Err(error) =
                        session.continue_entity(&mut demux, &physical_end, &mut input, next, cancel)
                    {
                        return Ok(session.refusal(&error, cause));
                    }
                    raw = &next.input;
                    continue;
                }
                session.seal_terminal_end(&physical_end, &mut input, cancel)?;
                logical_eof = Some(Cause::boundary(
                    "logical_eof",
                    location.source,
                    location.offset,
                    physical_end.epochs(),
                ));
                demux.end_of_source()?;
            }
        }
    }
}

fn provide_input(input: &mut InputCursor, raw: &Bytes, request: InputRequest) -> Result<()> {
    let offset = usize::try_from(request.offset)?;
    let end = raw.len().min(offset.saturating_add(request.maximum));
    if offset < end {
        input.provide_owned(InputWindow::new(
            request.source,
            request.token,
            request.offset,
            raw.slice(offset..end)?,
        )?)?;
    } else {
        input.finish(request.source, request.token)?;
    }
    Ok(())
}

/// A bounded replay is needed because a program-policy refusal may precede
/// physical EOF. Only the completion policy's confirmation can replace that refusal;
/// an empty observer, parse error or advertised input length cannot do so.
fn control_completion(
    pool: &Budget,
    request: &Request,
    cancel: &AtomicBool,
) -> Result<Option<NoMediaConfirmation>> {
    let raw = &request.input;
    let limits = ParseLimits::default();
    let mut input = InputCursor::new(
        pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        WINDOW,
    )?;
    let mut demux = TransportStream::new_with_namespace(
        pool.clone(),
        request.format,
        limits,
        request.program,
        request.namespace,
    )?;
    let mut completion =
        PacketCompletion::new_for_format(pool.clone(), limits, request.program, request.format)?;
    let mut ordinal = 0_u64;
    loop {
        match demux.poll_observed(&mut input, &mut WorkBudget::new(WORK), cancel)? {
            Step::Ready(event) => {
                let context = SourceContext {
                    source_event_ordinal: ordinal,
                    epochs: demux.source_epochs(),
                    byte_run_epoch: None,
                };
                let work = completion.required_work(event.event())?;
                match completion.observe_issued(&event, context, &mut WorkBudget::new(work), cancel)
                {
                    Ok(true) => {}
                    Ok(false) => return Err("completion yielded on its required grant".into()),
                    Err(Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated) => {
                        return Ok(None);
                    }
                    Err(error) => return Err(error.into()),
                }
                ordinal = ordinal.checked_add(1).ok_or(Error::Overflow)?;
                if matches!(event.event(), TraceEvent::Finished) {
                    let work = completion.required_end_work()?;
                    return match completion.confirm_no_media(
                        &mut input,
                        &mut WorkBudget::new(work),
                        cancel,
                    ) {
                        Ok(Some(confirmation)) => Ok(Some(confirmation)),
                        Ok(None) => Err("completion yielded on its required terminal grant".into()),
                        Err(Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated) => {
                            Ok(None)
                        }
                        Err(error) => Err(error.into()),
                    };
                }
            }
            Step::Yield => {}
            Step::NeedInput(request) => {
                let offset = usize::try_from(request.offset)?;
                let end = raw.len().min(offset.saturating_add(request.maximum));
                if offset < end {
                    input.provide_owned(InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        raw.slice(offset..end)?,
                    )?)?;
                } else {
                    input.finish(request.source, request.token)?;
                }
            }
            Step::End(_) => {
                let end = demux
                    .media_entity_end(&mut input, &mut WorkBudget::new(WORK), cancel)?
                    .ok_or("physical end yielded on a fixed grant")?;
                let work = completion.required_end_work()?;
                if !completion.seal_physical_end(
                    &end,
                    &mut input,
                    &mut WorkBudget::new(work),
                    cancel,
                )? {
                    return Err("completion yielded on its required end grant".into());
                }
                demux.end_of_source()?;
            }
        }
    }
}

fn parse(pool: &Budget, path: &Path, arguments: &[String]) -> Result<Request> {
    let usage = "expected SIZE PROGRAM [--dvb] [[--discontinuity] ENTITY]...";
    let [size, program, rest @ ..] = arguments else {
        return Err(usage.into());
    };
    let format = match size.as_str() {
        "188" => PacketFormat::Ts188,
        "192" => PacketFormat::M2ts192,
        _ => return Err(usage.into()),
    };
    let program: u16 = program.parse()?;
    let (namespace, rest) = match rest {
        [option, rest @ ..] if option == "--dvb" => (
            DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile),
            rest,
        ),
        _ => (DescriptorNamespace::Unspecified, rest),
    };
    let input = load_entity(pool, path, MAX_INPUT)?;
    let mut remaining = MAX_INPUT - input.len() as u64;
    let mut following = BudgetVec::new(pool);
    let mut rest = rest.iter();
    while let Some(argument) = rest.next() {
        if following.len() == MAX_ENTITIES - 1 {
            return Err(Error::Policy("TS media entity count").into());
        }
        let discontinuity = argument == "--discontinuity";
        let name = if discontinuity {
            rest.next().ok_or(usage)?
        } else {
            argument
        };
        let bytes = load_entity(pool, &entity(path, name)?, remaining)?;
        remaining -= bytes.len() as u64;
        let source = SourceId(2 + following.len() as u64);
        following
            .try_push(Entity {
                source,
                input: bytes,
                discontinuity,
            })
            .map_err(|error| error.error)?;
    }
    Ok(Request {
        format,
        program,
        namespace,
        input,
        following,
    })
}

fn load_entity(pool: &Budget, path: &Path, remaining: u64) -> Result<Bytes> {
    let metadata = path.metadata()?;
    let size = metadata.len();
    if !metadata.is_file() || size == 0 || size > remaining {
        return Err("input must be a bounded nonempty file".into());
    }
    let mut file = std::fs::File::open(path)?;
    let mut input = BudgetVec::try_filled(pool, 0, usize::try_from(size)?)?;
    file.read_exact(input.as_mut_slice())?;
    if file.read(&mut [0])? != 0 {
        return Err("input grew beyond its declared extent".into());
    }
    Ok(input.freeze().map_err(|error| error.error)?)
}

/// A further media entity, named relative to the first input's directory.
fn entity(first: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.starts_with('-') || name.contains(['/', '\\']) || name == ".." {
        return Err(format!("entity must be a file name next to the input: {name}").into());
    }
    Ok(first.with_file_name(name))
}

/// `ts-fold FILE ARGS`, `ts-bench FILE ITERATIONS ARGS` or `ts-memory FILE ARGS`,
/// with ARGS `SIZE PROGRAM [--dvb] [[--discontinuity] ENTITY]...`.
pub fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(
        args,
        PREFIX,
        " SIZE PROGRAM [--dvb] [[--discontinuity] ENTITY]...",
        2..=usize::MAX,
        false,
    )?;
    let pool = Budget::new(256 * 1024 * 1024)?;
    let request = parse(&pool, Path::new(invocation.path), invocation.arguments)?;
    let cancel = AtomicBool::new(false);
    measure(&invocation, OPERATION, Some(&pool), || {
        operation(&pool, std::hint::black_box(&request), &cancel)
    })
}

#[cfg(test)]
mod tests {
    use super::{Request, control_completion, operation, parse};
    use crate::runner::Totals;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use triergon_core::{Budget, BudgetVec, Bytes, Error};
    use triergon_mpegts::{PacketFormat, psi::DescriptorNamespace};

    fn run(file: &str, arguments: &[&str]) -> Totals {
        let pool = Budget::new(64 * 1024 * 1024).unwrap();
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mpegts/tests/data")
            .join(file);
        let arguments: Vec<String> = arguments.iter().map(|&a| a.to_owned()).collect();
        let idle = pool.used();
        let request = parse(&pool, &path, &arguments).unwrap();
        let totals = operation(&pool, &request, &AtomicBool::new(false)).unwrap();
        drop(request);
        assert_eq!(
            pool.used(),
            idle,
            "{file}: charged storage outlived the operation"
        );
        totals
    }

    #[test]
    fn one_packet_folds_as_the_recorded_reference() {
        // The fold harness/features/mpegts-pes/corpus.json records for this input.
        let totals = run("ts188-basic.ts", &["188", "1"]);
        assert!(totals.refusal.is_none());
        assert_eq!(
            (totals.packets, totals.bytes, totals.digest),
            (1, 27, 7_910_521_612_812_314_295)
        );
    }

    #[test]
    fn source_confirmed_clock_and_terminal_boundaries_keep_complete_packets() {
        for (file, packets, bytes, digest) in [
            (
                "pcr-timeline-discontinuity.ts",
                2,
                30,
                6_911_604_658_298_429_283,
            ),
            (
                "transport-partial-final-packet.ts",
                1,
                27,
                7_910_521_612_812_314_295,
            ),
            ("transport-sync-loss.ts", 1, 27, 7_910_521_612_812_314_295),
        ] {
            let totals = run(file, &["188", "1"]);
            assert!(
                totals.refusal.is_none(),
                "{file}: {:?}",
                totals.refusal.as_ref().map(|refusal| &refusal.reason)
            );
            assert_eq!(
                (totals.packets, totals.bytes, totals.digest),
                (packets, bytes, digest),
                "{file}"
            );
        }
    }

    #[test]
    fn source_clock_quarantine_is_not_a_confirmed_timeline() {
        let bytes =
            include_bytes!("../../mpegts/tests/data/selected-faults-v1/pcr-discontinuity-three.ts");
        let mut data = bytes.to_vec();
        // Keep the real first Timeline, then a carrier without PCR clears DI.
        // A second DI/PCR before two new samples makes the source quarantine it.
        data[752 + 5] = 0;
        data[752 + 6..752 + 12].fill(0xff);
        let mut next = bytes[564..752].to_vec();
        next[3] = (next[3] & 0xf0) | 3;
        data.extend_from_slice(&next);
        let pool = Budget::new(1 << 20).unwrap();
        let request = Request {
            format: PacketFormat::Ts188,
            program: 1,
            namespace: DescriptorNamespace::Unspecified,
            input: Bytes::copy_from_slice(&pool, &data).unwrap(),
            following: BudgetVec::new(&pool),
        };
        let totals = operation(&pool, &request, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            totals
                .refusal
                .expect("source quarantine must remain a refusal")
                .reason,
            "unsupported: needs a program-level reader"
        );
    }

    #[test]
    fn media_entities_fold_as_the_recorded_reference() {
        for (file, arguments, expected) in [
            (
                "hls-contiguous-part-a.ts",
                &["188", "1", "hls-contiguous-part-b.ts"][..],
                (1, 512, 15_463_357_684_512_389_673),
            ),
            (
                "hls-map-init.ts",
                &["188", "1", "hls-map-media.ts"][..],
                (1, 27, 7_910_521_612_812_314_295),
            ),
            (
                "ts188-basic.ts",
                &["188", "1", "--discontinuity", "hls-new-timeline.ts"][..],
                (2, 54, 2_150_793_539_127_356_716),
            ),
        ] {
            let totals = run(file, arguments);
            assert!(
                totals.refusal.is_none(),
                "{file}: {:?}",
                totals.refusal.as_ref().map(|refusal| &refusal.reason)
            );
            assert_eq!(
                (totals.packets, totals.bytes, totals.digest),
                expected,
                "{file}"
            );
        }
    }

    #[test]
    fn discontinuity_with_unpublished_pes_is_a_refusal() {
        let totals = run(
            "hls-contiguous-part-a.ts",
            &["188", "1", "--discontinuity", "hls-new-timeline.ts"],
        );
        let refusal = totals
            .refusal
            .expect("unfinished PES needs a boundary policy");
        assert_eq!(
            refusal.reason,
            "unsupported: discontinuity requires a drained PES observer"
        );
        assert_eq!(totals.packets, 0);
    }

    #[test]
    fn control_only_inputs_finish_with_a_real_physical_confirmation() {
        for file in [
            "psi-crc-invalid.ts",
            "psi-pointer-overrun.ts",
            "psi-length-over-policy.ts",
            "psi-pending-not-current.ts",
            "pat-atomic-two-sections.ts",
            "pat-incomplete-version.ts",
            "pat-network-pid-not-track.ts",
            "psi-pointer-finishes-old-starts-new.ts",
            "psi-two-sections-one-payload.ts",
            "pmt-version-wrap-retypes-pid.ts",
            "pmt-opaque-language-descriptors.ts",
            "pmt-descriptor-overrun.ts",
            "pmt-multi-section-forbidden.ts",
            "psi-stuffing-tail-invalid.ts",
            "psi-truncated-eof.ts",
        ] {
            let totals = run(file, &["188", "1"]);
            assert!(totals.refusal.is_none(), "{file}");
            assert_eq!(
                (totals.packets, totals.bytes, totals.digest),
                (0, 0, 0),
                "{file}"
            );
        }
    }

    #[test]
    fn reported_malformed_input_reaches_eof_with_no_packets() {
        for file in [
            "transport-afc-reserved.ts",
            "transport-adaptation-overrun.ts",
            "pes-optional-header-overrun.ts",
            "pes-start-prefix-invalid.ts",
        ] {
            let totals = run(file, &["188", "1"]);
            assert!(
                totals.refusal.is_none(),
                "{file}: {:?}",
                totals.refusal.as_ref().map(|r| &r.reason)
            );
            assert_eq!(
                (totals.packets, totals.bytes, totals.digest),
                (0, 0, 0),
                "{file}"
            );
        }
    }

    #[test]
    fn unfinished_and_unexplained_input_stay_refused() {
        for file in [
            "packet-completion-v1/physical-truncated-tail.ts",
            "packet-completion-v1/pmt-control-truncated-section.ts",
            "packet-completion-v1/unknown-pid-continuation-after-control.ts",
        ] {
            assert!(run(file, &["188", "1"]).refusal.is_some(), "{file}");
        }
    }

    #[test]
    fn completion_cancellation_and_storage_pressure_remain_errors() {
        let pool = Budget::new(64 * 1024 * 1024).unwrap();
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../mpegts/tests/data/psi-crc-invalid.ts");
        let request = parse(&pool, &path, &["188".into(), "1".into()]).unwrap();
        let idle = pool.used();
        let error = control_completion(&pool, &request, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(error.downcast_ref::<Error>(), Some(&Error::Cancelled));
        assert_eq!(pool.used(), idle);
        let pressure = pool.reserve(pool.available()).unwrap();
        let error = control_completion(&pool, &request, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Error>(),
            Some(Error::Backpressure { .. })
        ));
        drop(pressure);
        assert_eq!(pool.used(), idle);
    }
}
