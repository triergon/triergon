use triergon_mpegts::compat::pes::{
    DemuxObserver, MalformedInputCompletion, MalformedInputFact, MalformedInputKind,
    MalformedSourceDisposition, PacketCompletion, PesFaultPolicy, RecoveredPes,
    TransportFaultRecovery,
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
) -> Result<(), Stop> {
    let caps = SourceCapabilities {
        length: Some(bytes.len() as u64),
        random_access: false,
    };
    let mut cursor = InputCursor::new(pool, SOURCE, caps, 32 * 1024)?;
    let limits = ParseLimits::default();
    let cancel = AtomicBool::new(false);
    if control {
        return control_only(&mut cursor, bytes, program, limits, &cancel, out);
    }
    let mut completion =
        MalformedInputCompletion::new(&cursor, PacketFormat::Ts188, limits, program)?;
    let mut clock = DemuxClock::new(program)?;
    let mut recovery = None;
    let mut packets = 0;
    let mut discards = 0;
    loop {
        match completion.poll_source(&mut cursor, &mut WorkBudget::new(WORK), &cancel)? {
            Step::NeedInput(request) => provide(&mut cursor, bytes, request)?,
            Step::Ready(mut step) => {
                let context = step.context();
                let grant = step.required_work()?;
                match step
                    .observe(recovery.as_mut(), &mut WorkBudget::new(grant), &cancel)?
                    .ok_or("malformed completion yielded")?
                {
                    MalformedSourceDisposition::Reported(fact) => {
                        check_discard(&fact, bytes)?;
                        discards += 1;
                        if let Some(trace) = &mut out.trace {
                            writeln!(
                                trace,
                                "# discard {:?} {} {:?}",
                                fact.kind(),
                                fact.source_bytes().len(),
                                fact.source_fault().error
                            )?;
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
                                scope,
                                RegisteredCarriage::Avc,
                                pool.clone(),
                                limits,
                            )?;
                            let policy = PesFaultPolicy {
                                timing_headers: true,
                                rejected_clock_fields: true,
                                source_pcr_pid: Some(map.pcr_pid),
                                terminal_transport: true,
                            };
                            let mut observer =
                                TransportFaultRecovery::with_policy(observer, policy)
                                    .map_err(|refused| Stop::Refused(refused.error))?;
                            observer.bind_source_input(step.input())?;
                            recovery = Some(observer);
                        }
                        // A carrier of the PID before the selected PMT has no
                        // route: it reaches no observer, and the input's completion
                        // must account for it.
                        TraceEvent::Transport(packet) if packet.pid == pid => {
                            if let Some(observer) = recovery.as_mut() {
                                observer
                                    .push(packet.clone(), context)
                                    .map_err(|refused| Stop::Refused(refused.error))?;
                            }
                        }
                        TraceEvent::Diagnostic(fault) => {
                            if let Some(observer) = &mut recovery {
                                if observer.awaiting_source_fault() {
                                    observer.confirm_source_fault(fault, context)?;
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
                                        .map_err(|refused| Stop::Refused(refused.error))?;
                                } else if fault.kind == FaultKind::Transport
                                    && matches!(
                                        fault.error,
                                        Error::Truncated | Error::Invalid("TS sync byte")
                                    )
                                {
                                    step.hold_terminal_transport(observer)?;
                                }
                            }
                        }
                        TraceEvent::Timeline { .. } => {
                            recovery
                                .as_mut()
                                .ok_or("timeline precedes selected PMT")?
                                .confirm_source_timeline(step.event(), context)?;
                        }
                        _ => {}
                    },
                }
                let finished = matches!(step.event(), TraceEvent::Finished);
                drop(step);
                if let Some(observer) = &mut recovery {
                    if finished {
                        if observer.awaiting_terminal_completion() {
                            let witness = completion.source_finished(&cursor)?;
                            if !observer.confirm_terminal_finished(
                                &witness,
                                context,
                                &mut cursor,
                                &mut WorkBudget::new(WORK),
                                &cancel,
                            )? {
                                return Err("terminal confirmation yielded".into());
                            }
                            let tail = observer
                                .terminal_confirmation()
                                .ok_or("missing tail confirmation")?;
                            let end = tail.physical_end();
                            let fault = tail.source_fault();
                            if end.source != SOURCE
                                || end.offset != bytes.len() as u64
                                || fault.location.source != SOURCE
                                || fault.location.offset >= end.offset
                                || fault.kind != FaultKind::Transport
                                || tail.source_finished() != context
                                || tail.diagnostic_context().source_event_ordinal
                                    >= context.source_event_ordinal
                                || !matches!(
                                    fault.error,
                                    Error::Truncated | Error::Invalid("TS sync byte")
                                )
                            {
                                return Err(
                                    "tail confirmation differs from the physical input".into()
                                );
                            }
                            if let Some(trace) = &mut out.trace {
                                writeln!(trace, "# tail end {}", tail.physical_end().offset)?;
                            }
                        }
                        if packets == 0 && completion.discarded_carriers() != 0 {
                            let confirmation = completion
                                .confirm_no_packets(
                                    Some(observer),
                                    &mut cursor,
                                    &mut WorkBudget::new(WORK),
                                    &cancel,
                                )?
                                .ok_or("no-packets confirmation yielded")?;
                            let end = confirmation.physical_end();
                            if end.source != SOURCE
                                || end.offset != bytes.len() as u64
                                || confirmation.physical_carriers() != (bytes.len() / 188) as u64
                                || confirmation.discarded_carriers() != discards
                                || confirmation.source_finished() != context
                            {
                                return Err(
                                    "no-packets confirmation differs from the checked input".into(),
                                );
                            }
                            if let Some(trace) = &mut out.trace {
                                writeln!(
                                    trace,
                                    "# no packets at {} after {} discarded carriers",
                                    confirmation.physical_end().offset,
                                    confirmation.discarded_carriers()
                                )?;
                            }
                        }
                        observer.end_of_source()?;
                    }
                    packets += drain(observer, &mut clock, &cancel, out, finished)?;
                    if finished && packets == 0 && completion.discarded_carriers() == 0 {
                        return Err("empty observation has no completion proof".into());
                    }
                } else if finished {
                    // Without a selected PMT the input publishes no packet only
                    // when its completion proves that nothing was left unrouted.
                    let confirmation = completion
                        .confirm_no_packets(None, &mut cursor, &mut WorkBudget::new(WORK), &cancel)?
                        .ok_or("no-packets confirmation yielded")?;
                    if confirmation.physical_end().offset != bytes.len() as u64
                        || confirmation.source_finished() != context
                    {
                        return Err("no-packets confirmation differs from the checked input".into());
                    }
                }
                if finished {
                    return Ok(());
                }
            }
            Step::End(_) => {
                let end = completion
                    .seal_physical_end(&mut cursor, &mut WorkBudget::new(WORK), &cancel)?
                    .ok_or("physical end yielded")?;
                if let Some(observer) = &mut recovery
                    && observer.awaiting_terminal_completion()
                    && !observer.seal_terminal_end(
                        &end,
                        &mut cursor,
                        &mut WorkBudget::new(WORK),
                        &cancel,
                    )?
                {
                    return Err("terminal End yielded".into());
                }
                completion.end_of_source()?;
            }
            Step::Yield => {}
            Step::Backpressure(never) => match never {},
        }
    }
}

fn check_discard(fact: &MalformedInputFact, input: &Bytes) -> Result<(), Stop> {
    let fault = fact.source_fault();
    let transport = fact.accepted_transport();
    let start =
        usize::try_from(transport.map_or(fault.location.offset, |packet| packet.source_offset))?;
    let carrier = fact.source_bytes().as_ref();
    if fault.location.source != SOURCE
        || carrier.len() != 188
        || input.get(start..start.saturating_add(carrier.len())) != Some(carrier)
        || fact.input_context().source_event_ordinal
            > fact.diagnostic_context().source_event_ordinal
    {
        return Err("discarded carrier differs from the original input".into());
    }
    let control = (carrier[3] >> 4) & 3;
    let adaptation_length = usize::from(carrier[4]);
    let transport_fault = if control == 0 {
        Some(MalformedInputKind::ReservedAdaptationControl)
    } else if (control == 2 && adaptation_length != 183)
        || (control == 3 && adaptation_length >= 183)
    {
        Some(MalformedInputKind::AdaptationLengthOverrun)
    } else {
        None
    };
    let malformed = if let Some(kind) = transport_fault {
        (transport.is_none() && fault.kind == FaultKind::Transport).then_some(kind)
    } else if transport.is_some() && fault.kind == FaultKind::Pes {
        let offset = if control == 3 {
            5 + adaptation_length
        } else {
            4
        };
        let payload = &carrier[offset..];
        if !payload.starts_with(&[0, 0, 1]) {
            Some(MalformedInputKind::PesStartCode)
        } else if let [_, _, _, _, high, low, _, _, header_length, ..] = *payload {
            let pes_length = u16::from_be_bytes([high, low]);
            (pes_length != 0 && u16::from(header_length) + 3 > pes_length)
                .then_some(MalformedInputKind::PesHeaderLength)
        } else {
            None
        }
    } else {
        None
    };
    if malformed != Some(fact.kind()) {
        return Err("malformed carrier classification disagrees with its wire bytes".into());
    }
    Ok(())
}

fn drain(
    observer: &mut TransportFaultRecovery,
    clock: &mut DemuxClock,
    cancel: &AtomicBool,
    out: &mut Output<'_, '_>,
    finished: bool,
) -> Result<u64, Stop> {
    let mut count = 0;
    loop {
        let mut grant = WorkBudget::new(WORK);
        match observer.poll(&mut grant, cancel)? {
            AssemblyStep::Output(RecoveredPes::Accepted(packet)) => {
                let time = clock
                    .project_header(
                        packet.pid,
                        packet.source_epochs,
                        packet.time_header(),
                        &mut grant,
                        cancel,
                    )?
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
                    .project_transport_fault(&packet, &mut grant, cancel)?
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
                    .project_pes_fault(&packet, &mut grant, cancel)?
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
) -> Result<(), Stop> {
    let pool = cursor.budget().clone();
    let mut source = TransportStream::new(pool.clone(), PacketFormat::Ts188, limits, program)?;
    let mut completion =
        PacketCompletion::new_for_format(pool, limits, program, PacketFormat::Ts188)?;
    let mut ordinal = 0;
    loop {
        match source.poll_observed(cursor, &mut WorkBudget::new(WORK), cancel)? {
            Step::Ready(issued) => {
                let context = SourceContext {
                    source_event_ordinal: ordinal,
                    epochs: source.source_epochs(),
                    byte_run_epoch: None,
                };
                ordinal += 1;
                let grant = completion.required_work(issued.event())?;
                if !completion.observe_issued(
                    &issued,
                    context,
                    &mut WorkBudget::new(grant),
                    cancel,
                )? {
                    return Err("control observation yielded".into());
                }
                if matches!(issued.event(), TraceEvent::Finished) {
                    let grant = completion.required_end_work()?;
                    let confirmation = completion
                        .confirm_no_media(cursor, &mut WorkBudget::new(grant), cancel)?
                        .ok_or("no media confirmation yielded")?;
                    if confirmation.source() != SOURCE
                        || confirmation.source_bytes() != bytes.len() as u64
                        || confirmation.physical_carriers() != bytes.len() / 188
                        || confirmation.source_finished() != context
                    {
                        return Err(
                            "control-only confirmation differs from the checked input".into()
                        );
                    }
                    if let Some(trace) = &mut out.trace {
                        writeln!(
                            trace,
                            "# no media after source event {}",
                            confirmation.source_finished().source_event_ordinal
                        )?;
                    }
                    return Ok(());
                }
            }
            Step::NeedInput(request) => provide(cursor, bytes, request)?,
            Step::End(_) => {
                let end = source
                    .media_entity_end(cursor, &mut WorkBudget::new(WORK), cancel)?
                    .ok_or("physical end yielded")?;
                let grant = completion.required_end_work()?;
                if !completion.seal_physical_end(
                    &end,
                    cursor,
                    &mut WorkBudget::new(grant),
                    cancel,
                )? {
                    return Err("control end yielded".into());
                }
                source.end_of_source()?;
            }
            Step::Yield => {}
            Step::Backpressure(never) => match never {},
        }
    }
}
