use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId,
    Step, WorkBudget,
};
use triergon_mpegts::compat::input::{InputBootstrap, InputContext, InputStep};
use triergon_mpegts::compat::pes::{
    ClockOrigin, DemuxClock, DemuxScope, DvbSubtitlesDescriptors, PrivateAudioCarriage,
    PrivateAudioDescriptors, RegisteredCarriage, SourceContext,
};
use triergon_mpegts::psi::{DescriptorNamespace, DvbNamespaceEvidence, ProgramMap, TableStep};
use triergon_mpegts::reference_trace::recovery::{
    DemuxObservation, PatRoute, PatSelection, RecoveryObserver, RejectedPolicy,
};
use triergon_mpegts::stream::{TraceEvent, TransportStream};
use triergon_mpegts::{AssemblyStep, PacketFormat, ParseLimits};

use super::reader::{Stop, explain, refusal_detail, row};
use triergon_harness_runner::{Operation, Totals};

const SOURCE: SourceId = SourceId(1);
const WORK: u64 = 64 * 1024;

mod completion;
mod opaque;
mod recovery;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    Opcr,
    Selected,
    PrivateAudio,
    DvbSubtitles,
    Faults,
    Control,
    Input(PacketFormat),
    Opaque,
}

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    // A registered refusal case asks the worker to name its typed unsupported detail.
    let (report_refusal, arguments) = match arguments.split_last() {
        Some((last, rest)) if last == "--report-refusal" => (true, rest),
        _ => (false, arguments),
    };
    let [program, pid, policy] = arguments else {
        return Err("expected PROGRAM PID opcr|selected|audio|dvb".into());
    };
    let program = program
        .to_str()
        .ok_or("invalid program")?
        .parse()
        .map_err(explain)?;
    let pid = pid
        .to_str()
        .ok_or("invalid PID")?
        .parse()
        .map_err(explain)?;
    let policy = match (prefix, policy.to_str()) {
        ("ts-recovery-", Some("opcr")) => Policy::Opcr,
        ("ts-recovery-", Some("selected")) => Policy::Selected,
        ("ts-recovery-", Some("audio")) => Policy::PrivateAudio,
        ("ts-recovery-", Some("dvb")) => Policy::DvbSubtitles,
        ("ts-facts-", Some("faults")) => Policy::Faults,
        ("ts-facts-", Some("control")) => Policy::Control,
        ("ts-facts-", Some("input188")) => Policy::Input(PacketFormat::Ts188),
        ("ts-facts-", Some("input192")) => Policy::Input(PacketFormat::M2ts192),
        ("ts-facts-", Some("opaque")) => Policy::Opaque,
        _ => return Err("unknown recovery policy".into()),
    };
    let pool = Budget::new(128 * 1024 * 1024).map_err(explain)?;
    let bytes = Bytes::copy_from_slice(&pool, input).map_err(explain)?;
    Ok(Operation {
        name: if prefix == "ts-facts-" {
            "ts-fault-facts"
        } else {
            "ts-recovery-trace"
        },
        run: Box::new(move |work, trace| {
            let mut output = Output { work, trace };
            let operation = match policy {
                Policy::Faults | Policy::Control => completion::run(
                    &pool,
                    &bytes,
                    program,
                    pid,
                    policy == Policy::Control,
                    &mut output,
                ),
                Policy::Opaque => opaque::run(&pool, &bytes, program, pid, &mut output),
                _ => Recovery::new(program, pid, policy, &pool)
                    .and_then(|operation| operation.run(&pool, &bytes, &mut output)),
            };
            match operation {
                Ok(()) => row(output.work, &mut output.trace, 7, &[], &[]),
                Err(Stop::Refused(error)) => {
                    if let Some(trace) = &mut output.trace {
                        writeln!(trace, "# {error}").map_err(explain)?;
                    }
                    if report_refusal {
                        output.work.unsupported = Some(refusal_detail(&error));
                    }
                    row(output.work, &mut output.trace, 8, &[], &[])
                }
                Err(Stop::Failed(failure)) => Err(failure),
            }
        }),
    })
}

struct Output<'work, 'trace> {
    work: &'work mut Totals,
    trace: Option<&'trace mut dyn Write>,
}

struct Recovery {
    program: u16,
    pid: u16,
    policy: Policy,
    namespace: DescriptorNamespace,
    observer: Option<RecoveryObserver>,
    route: Option<PatRoute>,
    source: TransportStream,
    clock: DemuxClock,
    first_clock: Option<u64>,
    cancel: AtomicBool,
}

impl Recovery {
    fn new(program: u16, pid: u16, policy: Policy, pool: &Budget) -> Result<Self, Stop> {
        let namespace = if matches!(policy, Policy::PrivateAudio | Policy::DvbSubtitles) {
            DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile)
        } else {
            DescriptorNamespace::Unspecified
        };
        let format = match policy {
            Policy::Input(format) => format,
            _ => PacketFormat::Ts188,
        };
        let source = TransportStream::new_with_namespace(
            pool.clone(),
            format,
            ParseLimits::default(),
            program,
            namespace,
        )?;
        let clock = DemuxClock::new(program)?;
        if clock.selected_program() != program {
            return Err("clock selected a different program".into());
        }
        Ok(Self {
            program,
            pid,
            policy,
            namespace,
            observer: None,
            route: None,
            source,
            clock,
            first_clock: None,
            cancel: AtomicBool::new(false),
        })
    }

    fn run(
        mut self,
        pool: &Budget,
        bytes: &Bytes,
        output: &mut Output<'_, '_>,
    ) -> Result<(), Stop> {
        let bound = matches!(self.policy, Policy::Input(_));
        let caps = SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: bound,
        };
        let mut cursor = InputCursor::new(pool, SOURCE, caps, (32 * 1024).max(bytes.len()))?;
        let startup = if let Policy::Input(format) = self.policy {
            Some(position_probe(&mut cursor, bytes, format, &self.cancel)?)
        } else {
            None
        };
        let mut ordinal = 0;
        loop {
            match self
                .source
                .poll(&mut cursor, &mut WorkBudget::new(WORK), &self.cancel)?
            {
                Step::Ready(TraceEvent::Finished) => {
                    let observer = self.observer.as_mut().ok_or("no selected PMT")?;
                    observer.end_of_source()?;
                    if !self.drain(output, bound.then_some(&cursor), bytes)? {
                        return Err("recovery observer requested input at logical EOF".into());
                    }
                    return Ok(());
                }
                Step::Ready(event) => {
                    let context = SourceContext {
                        source_event_ordinal: ordinal,
                        epochs: self.source.source_epochs(),
                        byte_run_epoch: None,
                    };
                    ordinal += 1;
                    self.feed(
                        pool,
                        event,
                        context,
                        startup.as_ref().map(|startup| (startup, &cursor)),
                    )?;
                    if self.drain(output, bound.then_some(&cursor), bytes)? {
                        return Err("recovery observer reached EOF before the source".into());
                    }
                }
                Step::NeedInput(request) => {
                    provide(&mut cursor, bytes, request)?;
                }
                Step::End(_) => self.source.end_of_source()?,
                Step::Yield => {}
                Step::Backpressure(never) => match never {},
            }
        }
    }

    fn feed(
        &mut self,
        pool: &Budget,
        event: TraceEvent,
        context: SourceContext,
        startup: Option<(&InputContext, &InputCursor)>,
    ) -> Result<(), Stop> {
        match event {
            TraceEvent::Pat(table) => {
                recovery::check_pat_selection(&table, pool, context)?;
                let mut selection =
                    PatSelection::new(&table, self.program, pool, context)?;
                recovery::expect_pending(|work, cancel| selection.poll(work, cancel))?;
                let expected = table
                    .programs
                    .as_slice()
                    .iter()
                    .find(|entry| entry.program_number == self.program);
                loop {
                    match selection
                        .poll(&mut WorkBudget::new(WORK), &self.cancel)
                        ?
                    {
                        TableStep::Output(route) => {
                            recovery::check_pat(&table, expected, route)?;
                            if !matches!(
                                selection.poll(&mut WorkBudget::new(WORK), &self.cancel),
                                Ok(TableStep::Done)
                            ) {
                                return Err("PAT selection published a route twice".into());
                            }
                            self.route = route;
                            break;
                        }
                        TableStep::Yield => {}
                        TableStep::Done => return Err("PAT finished without a result".into()),
                    }
                }
            }
            TraceEvent::ProgramMap {
                map, active: true, ..
            } => self.bind(pool, &map, context, startup)?,
            TraceEvent::Transport(packet) if packet.pid == self.pid => {
                if let Some(observer) = &mut self.observer {
                    if let Some((_, cursor)) = startup {
                        observer.push_accepted_with_input(packet, context, cursor)
                    } else {
                        observer.push_accepted(packet, context)
                    }.map_err(|refused| Stop::Refused(refused.error))?;
                }
            }
            TraceEvent::Diagnostic(fault)
                if fault.pid == Some(self.pid)
                    || fault.rejected.as_ref().is_some_and(|carrier| {
                        let bytes = carrier.bytes().as_ref();
                        matches!(bytes, [0x47, high, low, ..] if u16::from_be_bytes([*high & 31, *low]) == self.pid)
                    }) =>
            {
                let Some(observer) = &mut self.observer else {
                    return Ok(());
                };
                if self.policy == Policy::Selected {
                    if fault.rejected.is_some() {
                        observer
                            .push_diagnostic_rejection(fault, context)
                            .map_err(|refused| Stop::Refused(refused.error))?;
                    } else if matches!(
                        fault.error,
                        Error::Invalid(
                            "TS transport error indicator"
                                | "PES timestamp markers"
                                | "forbidden PES time flags"
                        ) | Error::Unsupported("scrambled TS payload")
                    ) {
                        observer
                            .confirm_source_fault(&fault, context)
                            ?;
                    }
                } else if let Some(rejected) = fault.rejected {
                    observer
                        .push_rejected(rejected, context)
                        .map_err(|refused| Stop::Refused(refused.error))?;
                }
            }
            TraceEvent::Timeline { .. } if self.policy == Policy::Selected => {
                self.observer
                    .as_mut()
                    .ok_or("timeline without selected PMT")?
                    .commit_source_timeline(context)
                    ?;
            }
            _ => {}
        }
        Ok(())
    }

    fn bind(
        &mut self,
        pool: &Budget,
        map: &ProgramMap,
        context: SourceContext,
        startup: Option<(&InputContext, &InputCursor)>,
    ) -> Result<(), Stop> {
        let route = self.route.ok_or("PMT without selected PAT route")?;
        let rebind = self.observer.is_some();
        if route.program() != self.program || route.entity() != SOURCE {
            return Err("selected PAT route belongs to another source or program".into());
        }
        if !rebind {
            let mut observer = self.observer(pool, map, context)?;
            if let Some((startup, input)) = startup {
                observer.bind_input(startup, input)?;
            }
            self.observer = Some(observer);
        }
        if matches!(self.policy, Policy::PrivateAudio | Policy::DvbSubtitles) {
            return if rebind {
                Err("private carriage changed its PMT".into())
            } else {
                Ok(())
            };
        }
        let observer = self.observer.as_mut().ok_or("no recovery observer")?;
        let old_scope = observer.scope();
        let mut job = if rebind {
            observer.rebind_drained_selected_pmt(route, map, context)
        } else {
            observer.bind_selected_pmt(route, map, context)
        }?;
        recovery::expect_pending(|work, cancel| job.poll(work, cancel))?;
        if job.current_scope() != old_scope {
            return Err("PMT job changed the active scope before commit".into());
        }
        let binding = loop {
            match job.poll(&mut WorkBudget::new(WORK), &self.cancel)? {
                TableStep::Output(binding) => break binding,
                TableStep::Yield => {}
                TableStep::Done => return Err("PMT binding finished without a result".into()),
            }
        };
        if !matches!(
            job.poll(&mut WorkBudget::new(WORK), &self.cancel),
            Ok(TableStep::Done)
        ) {
            return Err("PMT job published a binding twice".into());
        }
        if binding.version != (map.source[5] >> 1) & 31
            || binding.pcr_pid != u16::from_be_bytes([map.source[8] & 31, map.source[9]])
            || binding.scope != observer.scope()
            || binding.scope.epochs != context.epochs
            || binding.scope.entity != SOURCE
            || binding.scope.program != self.program
            || binding.scope.pid != self.pid
            || map
                .streams
                .as_slice()
                .first()
                .map(|stream| stream.stream_type)
                != Some(binding.scope.stream_type)
            || binding.context != context
            || observer.pmt_binding() != Some(binding)
        {
            return Err("PMT job committed a different scope or source context".into());
        }
        Ok(())
    }

    fn observer(
        &self,
        pool: &Budget,
        map: &ProgramMap,
        context: SourceContext,
    ) -> Result<RecoveryObserver, Stop> {
        let [stream] = map.streams.as_slice() else {
            return Err("recovery requires one selected PMT stream".into());
        };
        if stream.pid != self.pid {
            return Err("PMT stream is not the requested PID".into());
        }
        let scope = DemuxScope {
            program: self.program,
            pid: stream.pid,
            stream_type: stream.stream_type,
            stream_id: if matches!(self.policy, Policy::PrivateAudio | Policy::DvbSubtitles) {
                0xbd
            } else {
                0xe0
            },
            entity: SOURCE,
            epochs: context.epochs,
        };
        let program = map.program_descriptors.as_ref();
        let elementary = stream.descriptor_bytes.as_ref();
        let limits = ParseLimits::default();
        match self.policy {
            Policy::Opcr | Policy::Input(_) => RecoveryObserver::new(
                scope,
                RegisteredCarriage::Avc,
                RejectedPolicy::OpcrWithoutPcr,
                pool.clone(),
                limits,
            ),
            Policy::Selected => {
                RecoveryObserver::new_selected_avc_faults(scope, pool.clone(), limits)
            }
            Policy::PrivateAudio => {
                let descriptors = PrivateAudioDescriptors {
                    program,
                    elementary,
                };
                let carriage = PrivateAudioCarriage::from_observed_pmt(
                    stream.stream_type,
                    descriptors,
                    limits,
                )?;
                RecoveryObserver::new_private_audio(
                    scope,
                    carriage,
                    descriptors,
                    pool.clone(),
                    limits,
                )
            }
            Policy::DvbSubtitles => RecoveryObserver::new_dvb_subtitles(
                scope,
                self.namespace,
                DvbSubtitlesDescriptors {
                    program,
                    elementary,
                },
                pool.clone(),
                limits,
            ),
            Policy::Faults | Policy::Control | Policy::Opaque => {
                return Err("expected a recovery trace policy".into());
            }
        }
        .map_err(Stop::Refused)
    }

    fn drain(
        &mut self,
        out: &mut Output<'_, '_>,
        input: Option<&InputCursor>,
        source: &[u8],
    ) -> Result<bool, Stop> {
        let Some(observer) = &mut self.observer else {
            return Ok(false);
        };
        let mut grant = WorkBudget::new(WORK);
        let clock = &mut self.clock;
        let first = &mut self.first_clock;
        let cancel = &self.cancel;
        let program = self.program;
        loop {
            let mut work = WorkBudget::new(WORK);
            let step = if let Some(input) = input {
                observer.poll_with_input(input, &mut work, cancel)
            } else {
                observer.poll(&mut work, cancel)
            };
            match step? {
                AssemblyStep::Output(DemuxObservation::Accepted(packet)) => {
                    let time = clock
                        .project_header(
                            packet.pid,
                            packet.source_epochs,
                            packet.time_header(),
                            &mut grant,
                            cancel,
                        )?
                        .ok_or("demux clock yielded")?;
                    *first = first.or(time.source_dts_raw33).or(time.source_pts_raw33);
                    out.packet(
                        packet.pid,
                        time.pts90,
                        time.dts90,
                        packet.position,
                        packet.flags,
                        packet.payload(),
                    )?;
                }
                AssemblyStep::Output(DemuxObservation::FromRejectedCarrier(packet)) => {
                    recovery::check_rejected(source, &packet, program)?;
                    let time = checked_projection(packet.pid, &mut grant, cancel, |pid, w, c| {
                        clock.project_encoded_header(pid, &packet.encoded_header, w, c)
                    })?;
                    let raw = packet
                        .encoded_header
                        .as_bytes()
                        .get(9..14)
                        .and_then(wire_timestamp)
                        .ok_or("missing encoded PTS")?;
                    let expected = clock_origin(first, Some(raw)).ok_or("missing clock origin")?;
                    if time.encoded_pts_raw33 != raw
                        || time.encoded_dts_raw33.is_some()
                        || time.origin != expected
                        || time.pts90 != lift(expected, raw)
                        || time.dts90 != time.pts90
                    {
                        return Err("encoded header clock differs from the source bits".into());
                    }
                    out.packet(
                        packet.pid,
                        Some(time.pts90),
                        Some(time.dts90),
                        packet.position,
                        packet.flags,
                        packet.payload(),
                    )?;
                }
                AssemblyStep::Output(DemuxObservation::FromSelectedDiagnostic(packet)) => {
                    recovery::check_diagnostic(source, &packet, program)?;
                    let time = checked_projection(packet.pid, &mut grant, cancel, |pid, w, c| {
                        clock.project_diagnostic_header(pid, &packet.encoded_header, w, c)
                    })?;
                    let header = &packet.encoded_header;
                    let bytes = header.as_bytes();
                    let raw = if bytes.get(7).is_some_and(|byte| byte >> 6 == 2) {
                        bytes.get(9..14).and_then(wire_timestamp)
                    } else {
                        None
                    };
                    let expected = clock_origin(first, raw);
                    if header.encoded_pts_raw33() != raw
                        || time.encoded_pts_raw33 != raw
                        || time.encoded_dts_raw33.is_some()
                        || time.origin != expected
                        || time.pts90 != expected.zip(raw).map(|(origin, raw)| lift(origin, raw))
                        || time.dts90 != time.pts90
                    {
                        return Err("diagnostic header clock differs from the source bits".into());
                    }
                    out.packet(
                        packet.pid,
                        time.pts90,
                        time.dts90,
                        packet.position,
                        packet.flags,
                        packet.payload(),
                    )?;
                }
                AssemblyStep::Yield => {}
                AssemblyStep::NeedPayload => return Ok(false),
                AssemblyStep::Finished => return Ok(true),
            }
        }
    }
}

fn checked_projection<T>(
    pid: u16,
    grant: &mut WorkBudget,
    cancel: &AtomicBool,
    mut project: impl FnMut(u16, &mut WorkBudget, &AtomicBool) -> triergon_core::Result<Option<T>>,
) -> Result<T, Stop> {
    if !matches!(project(pid, &mut WorkBudget::new(0), cancel), Ok(None))
        || !matches!(
            project(pid, &mut WorkBudget::new(1), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        )
        || !matches!(
            project(0x1fff, &mut WorkBudget::new(1), cancel),
            Err(Error::Invalid("assembly PID"))
        )
    {
        return Err("header clock changed a work, cancellation or PID refusal".into());
    }
    project(pid, grant, cancel)?.ok_or("header clock yielded with a full grant".into())
}

pub(super) fn wire_timestamp(stamp: &[u8]) -> Option<u64> {
    let [a, b, c, d, e] = *stamp else {
        return None;
    };
    let high = u64::from(a >> 1 & 7);
    let middle = u64::from(u16::from_be_bytes([b, c]) >> 1);
    let low = u64::from(u16::from_be_bytes([d, e]) >> 1);
    Some((high << 30) | (middle << 15) | low)
}

const DEMUX_PERIOD: i128 = 1 << 33;

fn clock_origin(first: &mut Option<u64>, next: Option<u64>) -> Option<ClockOrigin> {
    *first = first.or(next);
    first.map(|raw| {
        let start = i128::from(raw) - 5_400_000;
        let start = if start + 10_800_000 >= DEMUX_PERIOD {
            start - DEMUX_PERIOD
        } else {
            start.max(0)
        };
        ClockOrigin {
            first_reference_raw90: raw,
            interval_origin90: start,
        }
    })
}

fn lift(origin: ClockOrigin, raw: u64) -> i128 {
    let raw = i128::from(raw);
    let start = origin.interval_origin90;
    if raw < start {
        raw + DEMUX_PERIOD
    } else if raw >= start + DEMUX_PERIOD {
        raw - DEMUX_PERIOD
    } else {
        raw
    }
}

impl Output<'_, '_> {
    fn header(&mut self, flags: u8, pts: Option<u64>, data: &[u8]) -> Result<(), Stop> {
        Ok(row(
            self.work,
            &mut self.trace,
            3,
            &[
                u64::from(flags),
                u64::from(pts.is_some()),
                pts.unwrap_or(0),
                data.len() as u64,
            ],
            data,
        )?)
    }
    fn packet<'a>(
        &mut self,
        pid: u16,
        pts: Option<i128>,
        dts: Option<i128>,
        pos: i64,
        flags: u32,
        payload: impl Iterator<Item = &'a [u8]>,
    ) -> Result<(), Stop> {
        let timestamp =
            |ticks: Option<i128>| ticks.map_or(Ok(i64::MIN), i64::try_from).map_err(explain);
        let bytes: Vec<u8> = payload.flatten().copied().collect();
        Ok(row(
            self.work,
            &mut self.trace,
            1,
            &[
                u64::from(pid),
                timestamp(pts)? as u64,
                timestamp(dts)? as u64,
                pos as u64,
                u64::from(flags & 2 != 0),
                bytes.len() as u64,
            ],
            &bytes,
        )?)
    }
}

fn provide(
    cursor: &mut InputCursor,
    bytes: &Bytes,
    request: InputRequest,
) -> triergon_core::Result<()> {
    let start = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
    if start == bytes.len() {
        cursor.finish(request.source, request.token)
    } else {
        let end = bytes.len().min(start.saturating_add(request.maximum));
        cursor.provide(&InputWindow::new(
            request.source,
            request.token,
            request.offset,
            bytes.slice(start..end)?,
        )?)
    }
}

fn position_probe(
    input: &mut InputCursor,
    bytes: &Bytes,
    format: PacketFormat,
    cancel: &AtomicBool,
) -> triergon_core::Result<InputContext> {
    let request = input.request();
    let window = InputWindow::new(request.source, request.token, 0, bytes.clone())?;
    input.provide(&window)?;
    let mut bootstrap = InputBootstrap::new(input, &window, format)?;
    bootstrap.observe_entity_eof(input, window.source, &window.token, bytes.len() as u64)?;
    loop {
        match bootstrap.poll(input, &mut WorkBudget::new(WORK), cancel)? {
            InputStep::Ready(context) => return Ok(context),
            InputStep::Yield => {}
            InputStep::NeedEntityEof => {
                return Err(Error::Invalid("position probe lost input EOF"));
            }
        }
    }
}
