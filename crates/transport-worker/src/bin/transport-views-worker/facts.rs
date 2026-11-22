use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId,
    Step, WorkBudget,
};
use triergon_mpegts::compat::input::{InputBootstrap, InputContext, InputStep};
use triergon_mpegts::compat::pes::{
    DemuxClock, DemuxProfile, DemuxScope, DvbSubtitlesDescriptors, PrivateAudioCarriage,
    PrivateAudioDescriptors, RegisteredCarriage, SourceContext,
};
use triergon_mpegts::psi::{DescriptorNamespace, DvbNamespaceEvidence, ProgramMap, TableStep};
use triergon_mpegts::reference_trace::recovery::{
    DemuxObservation, PatRoute, PatSelection, RecoveryObserver, RejectedPolicy,
};
use triergon_mpegts::stream::{TraceEvent, TransportStream};
use triergon_mpegts::{AssemblyStep, PacketFormat, ParseLimits};

use super::protocol::{Operation, Work};
use super::reader::{explain, row};

const SOURCE: SourceId = SourceId(1);
const WORK: u64 = 64 * 1024;
const PROFILE: DemuxProfile = DemuxProfile::PesPackets;

mod completion;
mod opaque;

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
                Err(error) => {
                    if let Some(trace) = &mut output.trace {
                        writeln!(trace, "# {error}").map_err(explain)?;
                    }
                    row(output.work, &mut output.trace, 8, &[], &[])
                }
            }
        }),
    })
}

struct Output<'work, 'trace> {
    work: &'work mut Work,
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
    cancel: AtomicBool,
}

impl Recovery {
    fn new(program: u16, pid: u16, policy: Policy, pool: &Budget) -> Result<Self, String> {
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
        )
        .map_err(explain)?;
        let clock = DemuxClock::new(program).map_err(explain)?;
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
            cancel: AtomicBool::new(false),
        })
    }

    fn run(
        mut self,
        pool: &Budget,
        bytes: &Bytes,
        output: &mut Output<'_, '_>,
    ) -> Result<(), String> {
        let bound = matches!(self.policy, Policy::Input(_));
        let caps = SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: bound,
        };
        let mut cursor =
            InputCursor::new(pool, SOURCE, caps, (32 * 1024).max(bytes.len())).map_err(explain)?;
        let startup = if let Policy::Input(format) = self.policy {
            Some(position_probe(&mut cursor, bytes, format, &self.cancel).map_err(explain)?)
        } else {
            None
        };
        let mut ordinal = 0;
        loop {
            match self
                .source
                .poll(&mut cursor, &mut WorkBudget::new(WORK), &self.cancel)
                .map_err(explain)?
            {
                Step::Ready(TraceEvent::Finished) => {
                    let observer = self.observer.as_mut().ok_or("no selected PMT")?;
                    observer.end_of_source().map_err(explain)?;
                    if !self.drain(output, bound.then_some(&cursor))? {
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
                    if self.drain(output, bound.then_some(&cursor))? {
                        return Err("recovery observer reached EOF before the source".into());
                    }
                }
                Step::NeedInput(request) => {
                    provide(&mut cursor, bytes, request).map_err(explain)?;
                }
                Step::End(_) => self.source.end_of_source().map_err(explain)?,
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
    ) -> Result<(), String> {
        match event {
            TraceEvent::Pat(table) => {
                let mut selection =
                    PatSelection::new(&table, self.program, pool, context).map_err(explain)?;
                loop {
                    match selection
                        .poll(&mut WorkBudget::new(WORK), &self.cancel)
                        .map_err(explain)?
                    {
                        TableStep::Output(route) => {
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
                    }.map_err(|refused| explain(refused.error))?;
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
                            .map_err(|refused| explain(refused.error))?;
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
                            .map_err(explain)?;
                    }
                } else if let Some(rejected) = fault.rejected {
                    observer
                        .push_rejected(rejected, context)
                        .map_err(|refused| explain(refused.error))?;
                }
            }
            TraceEvent::Timeline { .. } if self.policy == Policy::Selected => {
                self.observer
                    .as_mut()
                    .ok_or("timeline without selected PMT")?
                    .commit_source_timeline(context)
                    .map_err(explain)?;
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
    ) -> Result<(), String> {
        let route = self.route.ok_or("PMT without selected PAT route")?;
        let rebind = self.observer.is_some();
        if route.program() != self.program || route.entity() != SOURCE {
            return Err("selected PAT route belongs to another source or program".into());
        }
        if !rebind {
            let mut observer = self.observer(pool, map, context)?;
            if let Some((startup, input)) = startup {
                observer.bind_input(startup, input).map_err(explain)?;
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
        let mut job = if rebind {
            observer.rebind_drained_selected_pmt(route, map, context)
        } else {
            observer.bind_selected_pmt(route, map, context)
        }
        .map_err(explain)?;
        loop {
            match job
                .poll(&mut WorkBudget::new(WORK), &self.cancel)
                .map_err(explain)?
            {
                TableStep::Output(_) => return Ok(()),
                TableStep::Yield => {}
                TableStep::Done => return Err("PMT binding finished without a result".into()),
            }
        }
    }

    fn observer(
        &self,
        pool: &Budget,
        map: &ProgramMap,
        context: SourceContext,
    ) -> Result<RecoveryObserver, String> {
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
                PROFILE,
                scope,
                RegisteredCarriage::Avc,
                RejectedPolicy::OpcrWithoutPcr,
                pool.clone(),
                limits,
            ),
            Policy::Selected => {
                RecoveryObserver::new_selected_avc_faults(PROFILE, scope, pool.clone(), limits)
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
                )
                .map_err(explain)?;
                RecoveryObserver::new_private_audio(
                    PROFILE,
                    scope,
                    carriage,
                    descriptors,
                    pool.clone(),
                    limits,
                )
            }
            Policy::DvbSubtitles => RecoveryObserver::new_dvb_subtitles(
                PROFILE,
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
        .map_err(explain)
    }

    fn drain(
        &mut self,
        out: &mut Output<'_, '_>,
        input: Option<&InputCursor>,
    ) -> Result<bool, String> {
        let Some(observer) = &mut self.observer else {
            return Ok(false);
        };
        let mut grant = WorkBudget::new(WORK);
        let clock = &mut self.clock;
        let cancel = &self.cancel;
        loop {
            let mut work = WorkBudget::new(WORK);
            let step = if let Some(input) = input {
                observer.poll_with_input(input, &mut work, cancel)
            } else {
                observer.poll(&mut work, cancel)
            };
            match step.map_err(explain)? {
                AssemblyStep::Output(DemuxObservation::Accepted(packet)) => {
                    let time = clock
                        .project_header(
                            packet.pid,
                            packet.source_epochs,
                            packet.time_header(),
                            &mut grant,
                            cancel,
                        )
                        .map_err(explain)?
                        .ok_or("demux clock yielded")?;
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
                    let time = clock
                        .project_encoded_header(
                            packet.pid,
                            &packet.encoded_header,
                            &mut grant,
                            cancel,
                        )
                        .map_err(explain)?
                        .ok_or("encoded clock yielded")?;
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
                    let time = clock
                        .project_diagnostic_header(
                            packet.pid,
                            &packet.encoded_header,
                            &mut grant,
                            cancel,
                        )
                        .map_err(explain)?
                        .ok_or("diagnostic clock yielded")?;
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

impl Output<'_, '_> {
    fn header(&mut self, flags: u8, pts: Option<u64>, data: &[u8]) -> Result<(), String> {
        row(
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
        )
    }
    fn packet<'a>(
        &mut self,
        pid: u16,
        pts: Option<i128>,
        dts: Option<i128>,
        pos: i64,
        flags: u32,
        payload: impl Iterator<Item = &'a [u8]>,
    ) -> Result<(), String> {
        let timestamp =
            |ticks: Option<i128>| ticks.map_or(Ok(i64::MIN), i64::try_from).map_err(explain);
        let bytes: Vec<u8> = payload.flatten().copied().collect();
        row(
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
        )
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
