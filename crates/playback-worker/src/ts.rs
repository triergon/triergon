//! The FFmpeg-compatible packets of one MPEG-TS program through the public API.
//!
//! The strict source view (`TransportStream`) parses the input and selects the
//! program. Its first active PMT binds a `compat::pes::DemuxObserver` to the
//! program's one elementary stream; the transport packets of that PID feed the
//! observer, and `compat::pes::DemuxClock` places each packet's timestamps.
//!
//! The crate has no published program-level reader yet, so this runner does no
//! routing of its own. Anything that would need one ends the operation with its
//! cause instead of a fold: a program with more than one stream or a carriage the
//! observer has no constructor for, a later PMT, a clock or format boundary, a
//! quarantined clock, a transport, PES-time, program or active PSI fault, a second
//! media entity, and an operation that finishes without a packet.
//!
//! Each packet folds its stream index, PID, PTS, DTS, duration, position, flags,
//! payload and side data, the same fields in the same order as the reference
//! worker (`harness/reference_ts.h`).

use crate::runner::{Invocation, MAX_INPUT, Refusal, Result, Totals, measure};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_mpegts::compat::pes::{
    DemuxClock, DemuxObserver, DemuxPacket, DemuxProfile, DemuxScope, DvbSubtitlesDescriptors,
    PrivateAudioCarriage, PrivateAudioDescriptors, RegisteredCarriage, SourceContext,
};
use triergon_mpegts::psi::{DescriptorNamespace, Descriptors, DvbNamespaceEvidence, ProgramMap};
use triergon_mpegts::{
    AssemblyStep, FaultKind, PacketFormat, ParseLimits, SourceEpochs, SourceRange, TraceEvent,
    TransportStream,
};

pub const PREFIX: &str = "ts-";
const OPERATION: &str = "mpegts-pes-packets";
const PROFILE: DemuxProfile = DemuxProfile::PesPackets;
const WINDOW: usize = 64 * 1024;
const WORK: u64 = 4096;
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
) -> Result<()> {
    let digest = &mut totals.digest;
    // One elementary stream: stream index zero.
    for value in [
        0,
        u64::from(packet.pid),
        signed(pts90.unwrap_or(NO_TIME)),
        signed(dts90.unwrap_or(NO_TIME)),
        signed(packet.duration90_raw),
        signed(packet.position),
        u64::from(packet.flags),
        packet.payload_len as u64,
    ] {
        fold(digest, value);
    }
    let mut seen = 0;
    for span in packet.payload() {
        fold_bytes(digest, span);
        seen += span.len();
    }
    if seen != packet.payload_len {
        return Err("packet payload does not match its length".into());
    }
    // One side-data element: the PES stream ID.
    for value in [1, STREAM_ID_SIDE_DATA, 1, u64::from(packet.stream_id)] {
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
    pool: &Budget,
    program: u16,
    map: &ProgramMap,
    cause: Cause,
) -> triergon_core::Result<DemuxObserver> {
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
        entity: cause.source,
        epochs: cause.epochs,
    };
    let (budget, limits) = (pool.clone(), ParseLimits::default());
    if let Some(carriage) = registered {
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
    }
}

/// The observer, its clock and the folded packets of one operation.
struct Session {
    clock: DemuxClock,
    observer: Option<DemuxObserver>,
    finished: bool,
    totals: Totals,
}

impl Session {
    /// Route one source event: the first active PMT binds the observer, the
    /// selected PID's transport packets feed it, and every context the observer
    /// alone cannot take is refused.
    fn observe(
        &mut self,
        pool: &Budget,
        program: u16,
        event: &TraceEvent,
        cause: Cause,
        quarantined: bool,
    ) -> triergon_core::Result<()> {
        let unrouted = || Error::Unsupported(UNROUTED);
        if let Some(observer) = &self.observer {
            let scope = observer.scope();
            if scope.entity != cause.source || scope.epochs != cause.epochs || quarantined {
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
                self.observer = Some(observer(pool, program, map, cause)?);
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

    /// Fold every packet the observer has ready.
    fn drain(&mut self, cancel: &AtomicBool) -> Result<()> {
        let Some(observer) = self.observer.as_mut() else {
            return Ok(());
        };
        loop {
            match observer.poll(&mut WorkBudget::new(WORK), cancel)? {
                AssemblyStep::Output(packet) => {
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

/// The parsed request: packet format, program, descriptor namespace, the first
/// media entity and whether more follow. The observer refuses at the first entity
/// boundary, so the later entities do not change the outcome.
struct Request {
    format: PacketFormat,
    program: u16,
    namespace: DescriptorNamespace,
    input: Bytes,
    more_entities: bool,
}

/// One operation: its fold, or the refusal that ended it.
fn operation(pool: &Budget, request: &Request, cancel: &AtomicBool) -> Result<Totals> {
    let raw = &request.input;
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
        ParseLimits::default(),
        request.program,
        request.namespace,
    )?;
    let mut session = Session {
        clock: DemuxClock::new(request.program)?,
        observer: None,
        finished: false,
        totals: Totals::default(),
    };
    let mut ordinal = 0_u64;
    let mut logical_eof = None;
    let refused = |mut session: Session, error: &dyn std::fmt::Display, cause: Cause| {
        session.totals.refusal = Some(Box::new(cause.refusal(error)));
        Ok(session.totals)
    };
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
                return refused(session, &error, cause);
            }
        };
        match step {
            Step::Ready(event) => {
                let cause = Cause::event(&event, ordinal, input.source(), demux.source_epochs());
                ordinal += 1;
                let routed = session.observe(
                    pool,
                    request.program,
                    &event,
                    cause,
                    demux.clock_is_quarantined(),
                );
                if let Err(error) = routed {
                    return refused(session, &error, cause);
                }
                if let Err(error) = session.drain(cancel) {
                    return refused(session, &error, cause);
                }
                if matches!(event, TraceEvent::Finished) {
                    let Some(eof) = logical_eof else {
                        let error = Error::Invalid("source finished without logical EOF");
                        return refused(session, &error, cause);
                    };
                    if let Err(error) = session.logical_end(cancel) {
                        return refused(session, &error, eof);
                    }
                    break;
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
            Step::End(location) => {
                let boundary = |phase| {
                    Cause::boundary(
                        phase,
                        location.source,
                        location.offset,
                        demux.source_epochs(),
                    )
                };
                if request.more_entities {
                    let error = Error::Unsupported(UNROUTED);
                    return refused(session, &error, boundary("entity_end_not_logical_eof"));
                }
                logical_eof = Some(boundary("logical_eof"));
                demux.end_of_source()?;
            }
        }
    }
    if session.totals.packets == 0 {
        let error = Error::Unsupported("empty observation is not acceptance");
        let cause = Cause::boundary(
            "logical_eof",
            input.source(),
            input.position(),
            demux.source_epochs(),
        );
        return refused(session, &error, cause);
    }
    Ok(session.totals)
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
    let mut namespace = DescriptorNamespace::Unspecified;
    let mut files = vec![path.to_path_buf()];
    let mut rest = rest.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--dvb" if files.len() == 1 => {
                namespace =
                    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile);
            }
            "--discontinuity" => files.push(entity(path, rest.next().ok_or(usage)?)?),
            name => files.push(entity(path, name)?),
        }
    }
    for path in &files {
        let size = path.metadata()?.len();
        if size == 0 || size > MAX_INPUT {
            return Err("input must be a bounded nonempty file".into());
        }
    }
    let input = Bytes::try_from_vec(pool, std::fs::read(path)?).map_err(|error| error.error())?;
    Ok(Request {
        format,
        program,
        namespace,
        input,
        more_entities: files.len() > 1,
    })
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
    use super::{operation, parse};
    use crate::runner::Totals;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use triergon_core::Budget;

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
    fn contexts_outside_the_observer_end_with_their_cause() {
        for (file, arguments, reason) in [
            (
                "hls-contiguous-part-a.ts",
                &["188", "1", "hls-contiguous-part-b.ts"][..],
                "unsupported: needs a program-level reader",
            ),
            (
                "psi-crc-invalid.ts",
                &["188", "1"][..],
                "unsupported: needs a program-level reader",
            ),
            (
                "pmt-opaque-language-descriptors.ts",
                &["188", "1"][..],
                "unsupported: empty observation is not acceptance",
            ),
        ] {
            let refusal = run(file, arguments).refusal.expect(file);
            assert_eq!(refusal.reason, reason, "{file}");
        }
    }
}
