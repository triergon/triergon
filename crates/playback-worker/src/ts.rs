//! The FFmpeg-compatible packet view of one MPEG-TS program.
//!
//! The strict source view (`TransportStream`) parses the input; its events drive
//! `compat::pes` observers bound to the one elementary stream of the selected
//! program, and `compat::pes::DemuxClock` places their timestamps. Packets FFmpeg's
//! demuxer still emits for carriers the source view rejects come from
//! `reference_trace::recovery`, the only implementation of that behaviour.
//!
//! Each emitted packet folds its stream index, PID, PTS, DTS, duration, position,
//! flags, payload and side data, the same fields in the same order as the reference
//! worker. A context outside the composed profile (more than one stream, a PSI or
//! clock transition, a second media entity, rejected carriers without a recovery
//! policy) ends the operation with its cause instead of a fold, and so does an
//! operation that finishes without a packet.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_mpegts::compat::pes::{
    DemuxClock, DemuxProfile, DemuxScope, DvbSubtitlesDescriptors, PrivateAudioCarriage,
    PrivateAudioDescriptors, RegisteredCarriage, SourceContext,
};
use triergon_mpegts::psi::{
    DescriptorNamespace, Descriptors, DvbNamespaceEvidence, PatTable, ProgramMap, TableStep,
};
use triergon_mpegts::reference_trace::recovery::{
    DemuxObservation, PatRoute, PatSelection, RecoveryObserver, RejectedPolicy,
};
use triergon_mpegts::{
    AssemblyStep, FaultKind, PacketFormat, ParseLimits, SourceEpochs, TraceEvent, TraceFault,
    TransportPacket, TransportStream,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const OPERATION: &str = "mpegts-pes-packets";
const PROFILE: DemuxProfile = DemuxProfile::PesPackets;
const MAX_INPUT: u64 = 64 * 1024 * 1024;
const WINDOW: usize = 64 * 1024;
/// FFmpeg's `AV_NOPTS_VALUE`, folded for an absent timestamp on both sides.
const NO_TIME: i64 = i64::MIN;
/// FFmpeg's `AV_PKT_DATA_MPEGTS_STREAM_ID` side-data type.
const STREAM_ID_SIDE_DATA: u64 = 19;

#[derive(Default)]
struct Work {
    packets: u64,
    bytes: u64,
    digest: u64,
}

impl Work {
    #[allow(
        clippy::too_many_arguments,
        reason = "one field per folded packet value"
    )]
    fn packet<'a>(
        &mut self,
        pid: u16,
        pts90: Option<i64>,
        dts90: Option<i64>,
        duration: i64,
        position: i64,
        flags: u32,
        stream_id: u8,
        length: usize,
        payload: impl Iterator<Item = &'a [u8]>,
    ) {
        let digest = &mut self.digest;
        // The composed profile has one elementary stream: stream index zero.
        for value in [
            0,
            u64::from(pid),
            signed(pts90.unwrap_or(NO_TIME)),
            signed(dts90.unwrap_or(NO_TIME)),
            signed(duration),
            signed(position),
            u64::from(flags),
            length as u64,
        ] {
            fold(digest, value);
        }
        for span in payload {
            fold_bytes(digest, span);
        }
        // One side-data element: the PES stream ID.
        for value in [1, STREAM_ID_SIDE_DATA, 1, u64::from(stream_id)] {
            fold(digest, value);
        }
        self.packets += 1;
        self.bytes += length as u64;
    }
}

/// A demuxer timestamp as FFmpeg's 64-bit packet field.
fn time64(value: Option<i128>) -> triergon_core::Result<Option<i64>> {
    value
        .map(|value| i64::try_from(value).map_err(|_| Error::Overflow))
        .transpose()
}

/// Poll a table job to its one output.
fn complete<T>(
    mut poll: impl FnMut(&mut WorkBudget) -> triergon_core::Result<TableStep<T>>,
    missing: &'static str,
) -> triergon_core::Result<T> {
    loop {
        match poll(&mut WorkBudget::new(4096))? {
            TableStep::Yield => {}
            TableStep::Output(output) => return Ok(output),
            TableStep::Done => return Err(Error::Invalid(missing)),
        }
    }
}

/// Why the composed operation did not complete.
struct Failure {
    error: Error,
    cause: Cause,
}

/// The source event or boundary a failure is attributed to. Ordinals count the
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
        let phase = match event {
            TraceEvent::Transport(_) => "transport",
            TraceEvent::Continuity { .. } => "continuity",
            TraceEvent::Section(_) => "section",
            TraceEvent::Pat(_) => "pat",
            TraceEvent::ProgramMap { .. } => "pmt",
            TraceEvent::Pes(_) => "pes",
            TraceEvent::Timeline { .. } => "timeline",
            TraceEvent::Discontinuity { .. } => "discontinuity",
            TraceEvent::Diagnostic(_) => "diagnostic",
            TraceEvent::Finished => "finished",
        };
        let mut cause = Self {
            phase,
            ordinal: Some(ordinal),
            source: entity,
            offset: None,
            epochs,
        };
        let range = match event {
            TraceEvent::Section(section) => section.source_ranges().first().copied(),
            TraceEvent::Pat(table) => table
                .source_ranges
                .as_slice()
                .first()
                .map(|item| item.range),
            TraceEvent::ProgramMap { map, .. } => map.source_ranges.as_slice().first().copied(),
            TraceEvent::Pes(pes) => pes.header_source_ranges.as_slice().first().copied(),
            _ => None,
        };
        if let Some(range) = range {
            cause.source = range.entity;
            cause.offset = Some(range.offset);
        }
        match event {
            TraceEvent::Transport(packet) => {
                cause.source = packet.source_id.unwrap_or(entity);
                cause.offset = Some(packet.source_offset);
            }
            TraceEvent::Diagnostic(fault) => {
                cause.source = fault.location.source;
                cause.offset = Some(fault.location.offset);
            }
            _ => {}
        }
        cause
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

    fn context(self) -> std::result::Result<SourceContext, Error> {
        Ok(SourceContext {
            source_event_ordinal: self
                .ordinal
                .ok_or(Error::Invalid("source event ordinal missing"))?,
            epochs: self.epochs,
            byte_run_epoch: None,
        })
    }
}

#[derive(Clone, Copy)]
struct ScopeRecord {
    scope: DemuxScope,
    pmt_pid: u16,
    pcr_pid: u16,
}

/// The FFmpeg-compatible packet view composed over the source events of one
/// program: one observer, its clock and the routing that admits carriers to it.
struct Composed {
    budget: Budget,
    program: u16,
    clock: DemuxClock,
    observer: Option<RecoveryObserver>,
    scope: Option<ScopeRecord>,
    pat: Option<(u16, u16)>,
    pat_route: Option<PatRoute>,
    finished: bool,
    failure: Option<Failure>,
}

impl Composed {
    fn new(budget: Budget, program: u16) -> triergon_core::Result<Self> {
        Ok(Self {
            budget,
            program,
            clock: DemuxClock::new(program)?,
            observer: None,
            scope: None,
            pat: None,
            pat_route: None,
            finished: false,
            failure: None,
        })
    }

    fn fail(&mut self, error: Error, cause: Cause) {
        if self.failure.is_none() {
            self.failure = Some(Failure { error, cause });
            self.observer = None;
        }
    }

    fn observe(
        &mut self,
        event: &TraceEvent,
        cause: Cause,
        quarantined: bool,
        cancel: &AtomicBool,
    ) {
        if self.failure.is_some() {
            return;
        }
        if let Err(error) = self.route(event, cause, quarantined, cancel) {
            self.fail(error, cause);
        }
    }

    fn route(
        &mut self,
        event: &TraceEvent,
        cause: Cause,
        quarantined: bool,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<()> {
        if let Some(record) = self.scope {
            match event {
                // A PMT that retypes the selected AVC stream as HEVC rebinds the
                // drained observer; any other PMT change after selection is outside
                // the profile.
                TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch,
                } if record.scope.stream_type == 0x1b
                    && matches!(map.streams.as_slice(), [stream] if stream.stream_type == 0x24) =>
                {
                    if quarantined
                        || cause.source != record.scope.entity
                        || *configuration_epoch != cause.epochs.configuration
                    {
                        return Err(Error::Unsupported(
                            "selected PMT source/quarantine context unobserved",
                        ));
                    }
                    return self.retype(record, map, cause, cancel);
                }
                TraceEvent::Timeline {
                    program,
                    clock_epoch,
                } => {
                    if record.scope.entity != cause.source
                        || *program != record.scope.program
                        || *clock_epoch != cause.epochs.clock
                    {
                        return Err(Error::Unsupported("native Timeline source mismatch"));
                    }
                    self.observer()?.commit_source_timeline(cause.context()?)?;
                    let mut record = record;
                    record.scope.epochs = cause.epochs;
                    self.scope = Some(record);
                    return Ok(());
                }
                _ if record.scope.entity != cause.source || record.scope.epochs != cause.epochs => {
                    return Err(Error::Unsupported(
                        "native source entity/epoch transition unobserved",
                    ));
                }
                _ if quarantined => {
                    return Err(Error::Unsupported("native source clock quarantined"));
                }
                _ => {}
            }
        }
        match event {
            TraceEvent::Pat(table) => self.route_pat(table, cause, cancel),
            TraceEvent::ProgramMap {
                map,
                active: true,
                configuration_epoch,
            } => {
                if self.scope.is_some() {
                    return Err(Error::Unsupported(
                        "native PMT configuration transition unobserved",
                    ));
                }
                self.select(map, *configuration_epoch, cause, quarantined, cancel)
            }
            TraceEvent::Transport(packet) => self.route_transport(packet, cause),
            TraceEvent::Timeline { .. } | TraceEvent::Discontinuity { .. } => Err(
                Error::Unsupported("native explicit clock/format boundary unobserved"),
            ),
            TraceEvent::Diagnostic(fault)
                if fault.kind == FaultKind::Transport
                    || (fault.kind == FaultKind::Pes
                        && matches!(
                            fault.error,
                            Error::Invalid("PES timestamp markers" | "forbidden PES time flags")
                        )) =>
            {
                self.route_fault(fault, cause)
            }
            TraceEvent::Diagnostic(fault)
                if fault.kind == FaultKind::Program
                    || (fault.kind == FaultKind::Psi && self.scope.is_some()) =>
            {
                Err(Error::Unsupported(
                    "native program/active PSI recovery context unobserved",
                ))
            }
            // Loss, duplicate and truncation diagnostics stay with the source view;
            // the observer sees the real carriers, never the source PES.
            _ => Ok(()),
        }
    }

    fn observer(&mut self) -> triergon_core::Result<&mut RecoveryObserver> {
        self.observer
            .as_mut()
            .ok_or(Error::Invalid("native observer missing"))
    }

    fn pat_route(&self) -> triergon_core::Result<PatRoute> {
        self.pat_route
            .ok_or(Error::Invalid("selected PAT route missing"))
    }

    /// Rebind the drained observer to the HEVC stream a PMT update retypes it as.
    fn retype(
        &mut self,
        record: ScopeRecord,
        map: &ProgramMap,
        cause: Cause,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<()> {
        let route = self.pat_route()?;
        let mut job = self
            .observer()?
            .rebind_drained_selected_pmt(route, map, cause.context()?)?;
        let binding = complete(
            |work| job.poll(work, cancel),
            "selected PMT binding output missing",
        )?;
        self.scope = Some(ScopeRecord {
            scope: binding.scope,
            pcr_pid: binding.pcr_pid,
            ..record
        });
        Ok(())
    }

    /// Route the program through a PAT; a later PAT must keep the same route.
    fn route_pat(
        &mut self,
        table: &PatTable,
        cause: Cause,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<()> {
        let mut search = PatSelection::new(table, self.program, &self.budget, cause.context()?)?;
        let chosen = complete(
            |work| search.poll(work, cancel),
            "selected PAT output missing",
        )?;
        let route = match chosen {
            Some(route) if route.entity() == cause.source => route,
            _ => {
                return Err(Error::Unsupported(
                    "native single selected PAT program not established",
                ));
            }
        };
        let next = (table.header.extension, route.pmt_pid());
        if self.scope.is_some() && self.pat != Some(next) {
            return Err(Error::Unsupported(
                "native PAT routing transition unobserved",
            ));
        }
        self.pat = Some(next);
        self.pat_route = Some(route);
        Ok(())
    }

    /// Select the program's one elementary stream and build its observer.
    fn select(
        &mut self,
        map: &ProgramMap,
        configuration_epoch: u64,
        cause: Cause,
        quarantined: bool,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<()> {
        let Some((_, pmt_pid)) = self.pat else {
            return Err(Error::Unsupported("native PMT without validated PAT"));
        };
        let [stream] = map.streams.as_slice() else {
            return Err(Error::Unsupported(
                "native single-stream PMT context not established",
            ));
        };
        if map.header.extension != self.program
            || !matches!(
                stream.stream_type,
                0x1b | 0x24 | 0x0f | 0x11 | 6 | 0x81 | 0x87
            )
            || configuration_epoch != cause.epochs.configuration
            || quarantined
        {
            return Err(Error::Unsupported(
                "native single-stream PMT context not established",
            ));
        }
        let carriage = match stream.stream_type {
            0x1b => Some(RegisteredCarriage::Avc),
            0x24 => Some(RegisteredCarriage::Hevc),
            0x0f => Some(RegisteredCarriage::AacAdts),
            0x11 => Some(RegisteredCarriage::AacLatm),
            _ => None,
        };
        let descriptors = PrivateAudioDescriptors {
            program: map.program_descriptors.as_ref(),
            elementary: stream.descriptor_bytes.as_ref(),
        };
        let has_tag = |tags: &[u8]| {
            Descriptors::new(descriptors.elementary)
                .any(|entry| entry.is_ok_and(|view| tags.contains(&view.tag)))
        };
        // DVB subtitles need the declared namespace and a subtitling descriptor.
        let dvb_namespace = (stream.stream_type == 6
            && matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_))
            && has_tag(&[0x59]))
        .then_some(map.descriptor_namespace);
        let audio = if carriage.is_none() && dvb_namespace.is_none() {
            // Neither a descriptor namespace nor a codec frame is inferred.
            if stream.stream_type == 6 && !has_tag(&[0x6a, 0x7a]) {
                return Err(Error::Unsupported(
                    "native single-stream PMT context not established",
                ));
            }
            Some(PrivateAudioCarriage::from_observed_pmt(
                stream.stream_type,
                descriptors,
                ParseLimits::default(),
            )?)
        } else {
            None
        };
        let stream_id = match carriage {
            _ if audio.is_some() || dvb_namespace.is_some() => 0xbd,
            Some(RegisteredCarriage::AacAdts | RegisteredCarriage::AacLatm) => 0xc0,
            _ => 0xe0,
        };
        let scope = DemuxScope {
            program: self.program,
            pid: stream.pid,
            stream_type: stream.stream_type,
            stream_id,
            entity: cause.source,
            epochs: cause.epochs,
        };
        let budget = self.budget.clone();
        let limits = ParseLimits::default();
        let mut observer = match (carriage, audio, dvb_namespace) {
            (Some(RegisteredCarriage::Avc), None, None) => {
                RecoveryObserver::new_selected_avc_faults(PROFILE, scope, budget, limits)
            }
            (Some(carriage), None, None) => RecoveryObserver::new(
                PROFILE,
                scope,
                carriage,
                RejectedPolicy::OpcrWithoutPcr,
                budget,
                limits,
            ),
            (None, Some(audio), None) => RecoveryObserver::new_private_audio(
                PROFILE,
                scope,
                audio,
                descriptors,
                budget,
                limits,
            ),
            (None, None, Some(namespace)) => RecoveryObserver::new_dvb_subtitles(
                PROFILE,
                scope,
                namespace,
                DvbSubtitlesDescriptors {
                    program: descriptors.program,
                    elementary: descriptors.elementary,
                },
                budget,
                limits,
            ),
            _ => Err(Error::Invalid("native carriage selection")),
        }?;
        if matches!(scope.stream_type, 0x1b | 0x24) {
            let route = self.pat_route()?;
            let mut job = observer.bind_selected_pmt(route, map, cause.context()?)?;
            complete(
                |work| job.poll(work, cancel),
                "initial selected PMT output missing",
            )?;
        }
        self.observer = Some(observer);
        self.scope = Some(ScopeRecord {
            scope,
            pmt_pid,
            pcr_pid: map.pcr_pid,
        });
        Ok(())
    }

    /// Admit the selected PID's carriers; any other media PID is outside the profile.
    fn route_transport(
        &mut self,
        packet: &TransportPacket,
        cause: Cause,
    ) -> triergon_core::Result<()> {
        let Some(record) = self.scope else {
            if packet.pid != 0
                && packet.pid != 0x1fff
                && self.pat.is_none_or(|(_, pid)| pid != packet.pid)
            {
                return Err(Error::Unsupported(
                    "native unrouted transport before validated scope",
                ));
            }
            return Ok(());
        };
        if packet.pid == record.scope.pid {
            let context = cause.context()?;
            self.observer()?
                .push_accepted(packet.clone(), context)
                .map_err(|rejected| rejected.error)?;
        } else if packet.pid == record.pcr_pid && packet.discontinuity_indicator() {
            return Err(Error::Unsupported(
                "native selected PCR clock boundary unobserved",
            ));
        } else if packet.pid == record.pcr_pid && packet.payload.is_some() {
            return Err(Error::Unsupported(
                "native unrouted PCR-PID payload context unobserved",
            ));
        } else if packet.pid != 0
            && packet.pid != 0x1fff
            && packet.pid != record.pmt_pid
            && packet.pid != record.pcr_pid
        {
            return Err(Error::Unsupported(
                "native additional unrouted transport unobserved",
            ));
        }
        Ok(())
    }

    /// Hand a transport or PES-time fault on the selected stream to the observer.
    fn route_fault(&mut self, fault: &TraceFault, cause: Cause) -> triergon_core::Result<()> {
        let (Some(_), Some(observer)) = (cause.ordinal, self.observer.as_mut()) else {
            return Err(Error::Unsupported(
                "native diagnostic without selected observer/ordinal",
            ));
        };
        let context = cause.context()?;
        if fault.error == Error::Invalid("PCR reserved bits/extension") && fault.rejected.is_some()
        {
            observer
                .push_diagnostic_rejection(fault.clone(), context)
                .map_err(|rejected| rejected.error)?;
        } else if let Some(rejection) = fault.rejected.as_ref() {
            if rejection.error() != fault.error
                || rejection.source_id() != Some(fault.location.source)
                || rejection.source_offset() != fault.location.offset
            {
                return Err(Error::Invalid(
                    "rejected carrier/source diagnostic mismatch",
                ));
            }
            observer
                .push_rejected(rejection.clone(), context)
                .map_err(|rejected| rejected.error)?;
        } else if fault.pid == Some(observer.scope().pid)
            && matches!(
                fault.error,
                Error::Invalid(
                    "TS transport error indicator"
                        | "PES timestamp markers"
                        | "forbidden PES time flags"
                ) | Error::Unsupported("scrambled TS payload")
            )
        {
            observer.confirm_source_fault(fault, context)?;
        } else {
            return Err(Error::Unsupported(
                "native raw transport unavailable after source rejection",
            ));
        }
        Ok(())
    }

    /// Fold every packet the observer has ready.
    fn drain(&mut self, work: &mut Work, cause: Cause, cancel: &AtomicBool) {
        while self.failure.is_none() {
            let Some(observer) = self.observer.as_mut() else {
                return;
            };
            let folded = match observer.poll(&mut WorkBudget::new(4096), cancel) {
                Ok(AssemblyStep::Output(observation)) => self.fold(observation, work, cancel),
                Ok(AssemblyStep::Yield) => Ok(()),
                Ok(AssemblyStep::NeedPayload) => return,
                Ok(AssemblyStep::Finished) => {
                    self.finished = true;
                    return;
                }
                Err(error) => Err(error),
            };
            if let Err(error) = folded {
                self.fail(error, cause);
            }
        }
    }

    fn fold(
        &mut self,
        observation: DemuxObservation,
        work: &mut Work,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<()> {
        let grant = || WorkBudget::new(1);
        match observation {
            DemuxObservation::Accepted(packet) => {
                let time = self
                    .clock
                    .project_header(
                        packet.pid,
                        packet.source_epochs,
                        packet.time_header(),
                        &mut grant(),
                        cancel,
                    )?
                    .ok_or(Error::Invalid("native clock fixed grant yielded"))?;
                work.packet(
                    packet.pid,
                    time64(time.pts90)?,
                    time64(time.dts90)?,
                    packet.duration90_raw,
                    packet.position,
                    packet.flags,
                    packet.stream_id,
                    packet.payload_len,
                    packet.payload(),
                );
            }
            DemuxObservation::FromRejectedCarrier(packet) => {
                let time = self
                    .clock
                    .project_encoded_header(
                        packet.pid,
                        &packet.encoded_header,
                        &mut grant(),
                        cancel,
                    )?
                    .ok_or(Error::Invalid("native encoded clock fixed grant yielded"))?;
                work.packet(
                    packet.pid,
                    time64(Some(time.pts90))?,
                    time64(Some(time.dts90))?,
                    packet.duration90_raw,
                    packet.position,
                    packet.flags,
                    packet.stream_id,
                    packet.payload_len,
                    packet.payload(),
                );
            }
            DemuxObservation::FromSelectedDiagnostic(packet) => {
                let time = self
                    .clock
                    .project_diagnostic_header(
                        packet.pid,
                        &packet.encoded_header,
                        &mut grant(),
                        cancel,
                    )?
                    .ok_or(Error::Invalid(
                        "native diagnostic clock fixed grant yielded",
                    ))?;
                work.packet(
                    packet.pid,
                    time64(time.pts90)?,
                    time64(time.dts90)?,
                    packet.duration90_raw,
                    packet.position,
                    packet.flags,
                    packet.stream_id,
                    packet.payload_len,
                    packet.payload(),
                );
            }
        }
        Ok(())
    }

    fn logical_end(&mut self, work: &mut Work, cause: Cause, cancel: &AtomicBool) {
        if self.failure.is_some() {
            return;
        }
        let ended = self
            .observer
            .as_mut()
            .ok_or(Error::Unsupported(
                "native scope not established at logical EOF",
            ))
            .and_then(RecoveryObserver::end_of_source);
        if let Err(error) = ended {
            self.fail(error, cause);
            return;
        }
        self.drain(work, cause, cancel);
        if self.failure.is_none() && !self.finished {
            self.fail(Error::Invalid("native logical EOF did not finish"), cause);
        }
    }
}

/// The parsed request: packet format, program, descriptor namespace, the first
/// media entity and whether more follow. The composed view refuses at the first
/// entity boundary, so neither the later entities nor whether one follows a
/// discontinuity change the outcome.
struct Request {
    format: PacketFormat,
    program: u16,
    namespace: DescriptorNamespace,
    input: Bytes,
    more_entities: bool,
}

/// The fold of one operation, or why it ended without one.
struct Outcome {
    work: Work,
    failure: Option<Failure>,
}

fn operation(pool: &Budget, request: &Request, cancel: &AtomicBool) -> Result<Outcome> {
    let mut work = Work::default();
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
    let mut composed = Composed::new(pool.clone(), request.program)?;
    let mut ordinal = 0_u64;
    let mut logical_eof = None;
    loop {
        let step = match demux.poll(&mut input, &mut WorkBudget::new(4096), cancel) {
            Ok(step) => step,
            Err(error) => {
                let cause = Cause::boundary(
                    "source_poll_error",
                    input.source(),
                    input.position(),
                    demux.source_epochs(),
                );
                composed.fail(error, cause);
                break;
            }
        };
        match step {
            Step::Ready(event) => {
                let cause = Cause::event(&event, ordinal, input.source(), demux.source_epochs());
                let quarantined = demux.clock_is_quarantined();
                composed.observe(&event, cause, quarantined, cancel);
                ordinal += 1;
                composed.drain(&mut work, cause, cancel);
                if matches!(event, TraceEvent::Finished) {
                    match logical_eof {
                        Some(cause) => composed.logical_end(&mut work, cause, cancel),
                        None => composed.fail(
                            Error::Invalid("source finished without explicit logical EOF"),
                            cause,
                        ),
                    }
                    break;
                }
                if composed.failure.is_some() {
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
                if request.more_entities {
                    // The composed view has no transaction across media entities; a
                    // second segment or a discontinuity is outside its profile.
                    composed.fail(
                        Error::Unsupported("native entity boundary context unobserved"),
                        Cause::boundary(
                            "entity_end_not_logical_eof",
                            location.source,
                            location.offset,
                            demux.source_epochs(),
                        ),
                    );
                    break;
                }
                logical_eof = Some(Cause::boundary(
                    "logical_eof",
                    location.source,
                    location.offset,
                    demux.source_epochs(),
                ));
                demux.end_of_source()?;
            }
        }
    }
    let failure = composed.failure.take().or_else(|| {
        (work.packets == 0).then(|| Failure {
            error: Error::Unsupported("empty observation is not acceptance"),
            cause: Cause::boundary(
                "logical_eof",
                input.source(),
                input.position(),
                demux.source_epochs(),
            ),
        })
    });
    Ok(Outcome { work, failure })
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

/// `ts-fold FILE ARGS...`, `ts-bench FILE ITERATIONS ARGS...` or `ts-memory FILE
/// ARGS...`, with ARGS `SIZE PROGRAM [--dvb] [[--discontinuity] ENTITY]...`.
pub fn run(args: &[String]) -> Result<()> {
    let usage = "usage: playback-worker ts-fold FILE SIZE PROGRAM [OPTIONS] | ts-bench FILE N SIZE PROGRAM [OPTIONS] | ts-memory FILE SIZE PROGRAM [OPTIONS]";
    let (mode, path, iterations, arguments) = match args {
        [mode, path, rest @ ..] if mode == "ts-fold" || mode == "ts-memory" => {
            (mode, path, 1, rest)
        }
        [mode, path, count, rest @ ..] if mode == "ts-bench" => {
            (mode, path, count.parse::<u64>()?, rest)
        }
        _ => return Err(usage.into()),
    };
    if !(1..=1_000_000).contains(&iterations) {
        return Err(usage.into());
    }
    let memory = mode == "ts-memory";
    let pool = Budget::new(256 * 1024 * 1024)?;
    let request = parse(&pool, Path::new(path), arguments)?;
    let retained = pool.used();
    let cancel = AtomicBool::new(false);
    let mut out = io::stdout().lock();
    if memory {
        out.write_all(b"TRIERGON_BEGIN_V1\n")?;
        out.flush()?;
    }
    let start = (mode == "ts-bench").then(Instant::now);
    let mut total = Work::default();
    let mut failure = None;
    for _ in 0..iterations {
        let outcome = operation(&pool, std::hint::black_box(&request), &cancel)?;
        fold(&mut total.digest, outcome.work.digest);
        total.packets += outcome.work.packets;
        total.bytes += outcome.work.bytes;
        if outcome.failure.is_some() {
            failure = outcome.failure;
            break;
        }
        if pool.used() != retained {
            return Err("operation retained charged storage".into());
        }
    }
    let elapsed = start.map_or(0, |begin| begin.elapsed().as_nanos());
    if memory {
        out.write_all(b"TRIERGON_END_V1\n")?;
        out.flush()?;
    }
    let mut line = format!(
        "{{\"schema\":1,\"operation\":\"{OPERATION}\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":{elapsed},\"eof\":{}",
        total.packets,
        total.bytes,
        total.digest,
        failure.is_none()
    );
    if let Some(Failure { error, cause }) = failure {
        line.push_str(",\"refusal\":");
        triergon_harness_protocol::push_quoted(&mut line, &error.to_string());
        write!(
            line,
            ",\"cause\":{{\"phase\":\"{}\",\"source_event_ordinal\":{},\"source\":{},\"offset\":{},\"clock_epoch\":{},\"configuration_epoch\":{}}}",
            cause.phase,
            cause.ordinal.map_or("null".into(), |n| n.to_string()),
            cause.source.0,
            cause.offset.map_or("null".into(), |n| n.to_string()),
            cause.epochs.clock,
            cause.epochs.configuration,
        )?;
    }
    line.push('}');
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Outcome, operation, parse};
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use triergon_core::Budget;

    fn run(file: &str, arguments: &[&str]) -> Outcome {
        let pool = Budget::new(64 * 1024 * 1024).unwrap();
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mpegts/tests/data")
            .join(file);
        let arguments: Vec<String> = arguments.iter().map(|&a| a.to_owned()).collect();
        let idle = pool.used();
        let request = parse(&pool, &path, &arguments).unwrap();
        let outcome = operation(&pool, &request, &AtomicBool::new(false)).unwrap();
        drop(request);
        assert_eq!(
            pool.used(),
            idle,
            "{file}: charged storage outlived the operation"
        );
        outcome
    }

    #[test]
    fn one_packet_folds_as_the_recorded_reference() {
        // The fold harness/features/mpegts-pes/corpus.json records for this input.
        let outcome = run("ts188-basic.ts", &["188", "1"]);
        assert!(outcome.failure.is_none());
        let work = outcome.work;
        assert_eq!(
            (work.packets, work.bytes, work.digest),
            (1, 27, 7_910_521_612_812_314_295)
        );
    }

    #[test]
    fn contexts_outside_the_profile_end_with_their_cause() {
        for (file, arguments, refusal) in [
            (
                "hls-contiguous-part-a.ts",
                &["188", "1", "hls-contiguous-part-b.ts"][..],
                "unsupported: native entity boundary context unobserved",
            ),
            (
                "psi-crc-invalid.ts",
                &["188", "1"][..],
                "unsupported: native scope not established at logical EOF",
            ),
            (
                "pmt-opaque-language-descriptors.ts",
                &["188", "1"][..],
                "unsupported: empty observation is not acceptance",
            ),
        ] {
            let failure = run(file, arguments).failure.expect(file);
            assert_eq!(failure.error.to_string(), refusal, "{file}");
        }
    }
}
