use triergon_mpegts::compat::pes::{
    DemuxObserver, MalformedInputCompletion, MalformedSourceDisposition, PacketCompletion,
    PesFaultPolicy, RecoveredPes, TransportFaultRecovery,
};
use triergon_mpegts::stream::FaultKind;

use super::*;

pub(super) fn run(
    pool: &Budget,
    bytes: &Bytes,
    program: u16,
    pid: u16,
    control: bool,
    out: &mut Output<'_, '_>,
) -> Result<(), String> {
    let caps = SourceCapabilities {
        length: Some(bytes.len() as u64),
        random_access: false,
    };
    let mut cursor = InputCursor::new(pool, SOURCE, caps, 32 * 1024).map_err(explain)?;
    let limits = ParseLimits::default();
    let cancel = AtomicBool::new(false);
    if control {
        return control_only(&mut cursor, bytes, program, limits, &cancel, out);
    }
    let mut completion =
        MalformedInputCompletion::new(&cursor, PacketFormat::Ts188, limits, program)
            .map_err(explain)?;
    let mut clock = DemuxClock::new(program).map_err(explain)?;
    let mut recovery = None;
    let mut packets = 0;
    loop {
        match completion
            .poll_source(&mut cursor, &mut WorkBudget::new(WORK), &cancel)
            .map_err(explain)?
        {
            Step::NeedInput(request) => provide(&mut cursor, bytes, request).map_err(explain)?,
            Step::Ready(mut step) => {
                let context = step.context();
                let grant = step.required_work().map_err(explain)?;
                match step
                    .observe(recovery.as_mut(), &mut WorkBudget::new(grant), &cancel)
                    .map_err(explain)?
                    .ok_or("malformed completion yielded")?
                {
                    MalformedSourceDisposition::Reported(fact) => {
                        if let Some(trace) = &mut out.trace {
                            writeln!(
                                trace,
                                "# discard {:?} {} {:?}",
                                fact.kind(),
                                fact.source_bytes().len(),
                                fact.source_fault().error
                            )
                            .map_err(explain)?;
                        }
                    }
                    MalformedSourceDisposition::AwaitDiagnostic => {}
                    MalformedSourceDisposition::Forward => match step.event() {
                        TraceEvent::ProgramMap {
                            map, active: true, ..
                        } => {
                            if recovery.is_some() || step.clock_is_quarantined() {
                                return Err(
                                    "selected fault policy requires one active AVC map".into()
                                );
                            }
                            let [stream] = map.streams.as_slice() else {
                                return Err("selected fault policy requires one stream".into());
                            };
                            if stream.pid != pid {
                                return Err("selected PID is absent".into());
                            }
                            let scope = DemuxScope {
                                program,
                                pid,
                                stream_type: stream.stream_type,
                                stream_id: 0xe0,
                                entity: SOURCE,
                                epochs: context.epochs,
                            };
                            let observer = DemuxObserver::new_registered(
                                PROFILE,
                                scope,
                                RegisteredCarriage::Avc,
                                pool.clone(),
                                limits,
                            )
                            .map_err(explain)?;
                            let policy = PesFaultPolicy {
                                timing_headers: true,
                                rejected_clock_fields: true,
                                source_pcr_pid: Some(map.pcr_pid),
                                terminal_transport: true,
                            };
                            let mut observer =
                                TransportFaultRecovery::with_policy(observer, policy)
                                    .map_err(|refused| explain(refused.error))?;
                            observer.bind_source_input(step.input()).map_err(explain)?;
                            recovery = Some(observer);
                        }
                        TraceEvent::Transport(packet) if packet.pid == pid => {
                            recovery
                                .as_mut()
                                .ok_or("carrier precedes selected PMT")?
                                .push(packet.clone(), context)
                                .map_err(|refused| explain(refused.error))?;
                        }
                        TraceEvent::Diagnostic(fault) => {
                            if let Some(observer) = &mut recovery {
                                if observer.awaiting_source_fault() {
                                    observer
                                        .confirm_source_fault(fault, context)
                                        .map_err(explain)?;
                                } else if fault.rejected.is_some()
                                    && matches!(
                                        fault.error,
                                        Error::Invalid(
                                            "PCR reserved bits/extension" | "OPCR without PCR"
                                        )
                                    )
                                {
                                    observer
                                        .push_rejected_fault(fault.clone(), context)
                                        .map_err(|refused| explain(refused.error))?;
                                } else if fault.kind == FaultKind::Transport
                                    && matches!(
                                        fault.error,
                                        Error::Truncated | Error::Invalid("TS sync byte")
                                    )
                                {
                                    step.hold_terminal_transport(observer).map_err(explain)?;
                                }
                            }
                        }
                        TraceEvent::Timeline { .. } => {
                            recovery
                                .as_mut()
                                .ok_or("timeline precedes selected PMT")?
                                .confirm_source_timeline(step.event(), context)
                                .map_err(explain)?;
                        }
                        _ => {}
                    },
                }
                let finished = matches!(step.event(), TraceEvent::Finished);
                drop(step);
                if let Some(observer) = &mut recovery {
                    if finished {
                        if observer.awaiting_terminal_completion() {
                            let witness = completion.source_finished(&cursor).map_err(explain)?;
                            if !observer
                                .confirm_terminal_finished(
                                    &witness,
                                    context,
                                    &mut cursor,
                                    &mut WorkBudget::new(WORK),
                                    &cancel,
                                )
                                .map_err(explain)?
                            {
                                return Err("terminal confirmation yielded".into());
                            }
                            let tail = observer
                                .terminal_confirmation()
                                .ok_or("missing tail confirmation")?;
                            if let Some(trace) = &mut out.trace {
                                writeln!(trace, "# tail end {}", tail.physical_end().offset)
                                    .map_err(explain)?;
                            }
                        }
                        if packets == 0 && completion.discarded_carriers() != 0 {
                            let confirmation = completion
                                .confirm_no_packets(
                                    Some(observer),
                                    &mut cursor,
                                    &mut WorkBudget::new(WORK),
                                    &cancel,
                                )
                                .map_err(explain)?
                                .ok_or("no-packets confirmation yielded")?;
                            if let Some(trace) = &mut out.trace {
                                writeln!(
                                    trace,
                                    "# no packets at {} after {} discarded carriers",
                                    confirmation.physical_end().offset,
                                    confirmation.discarded_carriers()
                                )
                                .map_err(explain)?;
                            }
                        } else if packets == 0 {
                            return Err("empty observation has no completion proof".into());
                        }
                        observer.end_of_source().map_err(explain)?;
                    }
                    packets += drain(observer, &mut clock, &cancel, out, finished)?;
                } else if finished {
                    return Err("missing selected PMT at EOF".into());
                }
                if finished {
                    return Ok(());
                }
            }
            Step::End(_) => {
                let end = completion
                    .seal_physical_end(&mut cursor, &mut WorkBudget::new(WORK), &cancel)
                    .map_err(explain)?
                    .ok_or("physical end yielded")?;
                if let Some(observer) = &mut recovery
                    && observer.awaiting_terminal_completion()
                    && !observer
                        .seal_terminal_end(&end, &mut cursor, &mut WorkBudget::new(WORK), &cancel)
                        .map_err(explain)?
                {
                    return Err("terminal End yielded".into());
                }
                completion.end_of_source().map_err(explain)?;
            }
            Step::Yield => {}
            Step::Backpressure(never) => match never {},
        }
    }
}

fn drain(
    observer: &mut TransportFaultRecovery,
    clock: &mut DemuxClock,
    cancel: &AtomicBool,
    out: &mut Output<'_, '_>,
    finished: bool,
) -> Result<u64, String> {
    let mut count = 0;
    loop {
        let mut grant = WorkBudget::new(WORK);
        match observer.poll(&mut grant, cancel).map_err(explain)? {
            AssemblyStep::Output(RecoveredPes::Accepted(packet)) => {
                let time = clock
                    .project_header(
                        packet.pid,
                        packet.source_epochs,
                        packet.time_header(),
                        &mut grant,
                        cancel,
                    )
                    .map_err(explain)?
                    .ok_or("clock yielded")?;
                if let Some(header) = packet.time_header() {
                    out.header(
                        header.as_bytes()[7] >> 6,
                        header.pts_raw33,
                        header.as_bytes(),
                    )?;
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
            AssemblyStep::Output(RecoveredPes::Faulted(packet)) => {
                let time = clock
                    .project_transport_fault(&packet, &mut grant, cancel)
                    .map_err(explain)?
                    .ok_or("transport fault clock yielded")?;
                let header = &packet.encoded_header;
                out.header(
                    header.as_bytes()[7] >> 6,
                    Some(header.encoded_pts_raw33()),
                    header.as_bytes(),
                )?;
                out.packet(
                    packet.pid,
                    Some(time.pts90),
                    Some(time.dts90),
                    packet.position,
                    packet.flags,
                    packet.payload(),
                )?;
            }
            AssemblyStep::Output(RecoveredPes::PesFault(packet)) => {
                let time = clock
                    .project_pes_fault(&packet, &mut grant, cancel)
                    .map_err(explain)?
                    .ok_or("PES fault clock yielded")?;
                let header = &packet.encoded_header;
                out.header(
                    header.time_flags(),
                    header.encoded_pts_raw33(),
                    header.as_bytes(),
                )?;
                out.packet(
                    packet.pid,
                    time.pts90,
                    time.dts90,
                    packet.position,
                    packet.flags,
                    packet.payload(),
                )?;
            }
            AssemblyStep::Yield => continue,
            AssemblyStep::NeedPayload if !finished => return Ok(count),
            AssemblyStep::Finished if finished => return Ok(count),
            _ => return Err("observer completion disagrees with source".into()),
        }
        count += 1;
    }
}

fn control_only(
    cursor: &mut InputCursor,
    bytes: &Bytes,
    program: u16,
    limits: ParseLimits,
    cancel: &AtomicBool,
    out: &mut Output<'_, '_>,
) -> Result<(), String> {
    let pool = cursor.budget().clone();
    let mut source = TransportStream::new(pool.clone(), PacketFormat::Ts188, limits, program)
        .map_err(explain)?;
    let mut completion =
        PacketCompletion::new_for_format(pool, limits, program, PacketFormat::Ts188)
            .map_err(explain)?;
    let mut ordinal = 0;
    loop {
        match source
            .poll_observed(cursor, &mut WorkBudget::new(WORK), cancel)
            .map_err(explain)?
        {
            Step::Ready(issued) => {
                let context = SourceContext {
                    source_event_ordinal: ordinal,
                    epochs: source.source_epochs(),
                    byte_run_epoch: None,
                };
                ordinal += 1;
                let grant = completion.required_work(issued.event()).map_err(explain)?;
                if !completion
                    .observe_issued(&issued, context, &mut WorkBudget::new(grant), cancel)
                    .map_err(explain)?
                {
                    return Err("control observation yielded".into());
                }
                if matches!(issued.event(), TraceEvent::Finished) {
                    let grant = completion.required_end_work().map_err(explain)?;
                    let confirmation = completion
                        .confirm_no_media(cursor, &mut WorkBudget::new(grant), cancel)
                        .map_err(explain)?
                        .ok_or("no media confirmation yielded")?;
                    if let Some(trace) = &mut out.trace {
                        writeln!(
                            trace,
                            "# no media after source event {}",
                            confirmation.source_finished().source_event_ordinal
                        )
                        .map_err(explain)?;
                    }
                    return Ok(());
                }
            }
            Step::NeedInput(request) => provide(cursor, bytes, request).map_err(explain)?,
            Step::End(_) => {
                let end = source
                    .media_entity_end(cursor, &mut WorkBudget::new(WORK), cancel)
                    .map_err(explain)?
                    .ok_or("physical end yielded")?;
                let grant = completion.required_end_work().map_err(explain)?;
                if !completion
                    .seal_physical_end(&end, cursor, &mut WorkBudget::new(grant), cancel)
                    .map_err(explain)?
                {
                    return Err("control end yielded".into());
                }
                source.end_of_source().map_err(explain)?;
            }
            Step::Yield => {}
            Step::Backpressure(never) => match never {},
        }
    }
}
