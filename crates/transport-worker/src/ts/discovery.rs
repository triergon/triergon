//! Opt-in discovery and replay of registered TS188 program streams.
use super::*;
use triergon_core::SourceCapabilities;
use triergon_mpegts::compat::{input::Ts188Startup, pes::ReadyContinuation};
use triergon_mpegts::{
    ContinuityOutcome, FeedContinuation, TransportPacket, psi::DescriptorNamespace,
};

struct Track {
    observer: DemuxObserver,
    started: bool,
    next_counter: Option<u8>,
}

impl Track {
    fn check_counter(&self, packet: &TransportPacket) -> triergon_core::Result<()> {
        if packet.payload.is_some()
            && self
                .next_counter
                .is_some_and(|next| next != packet.continuity_counter)
        {
            return Err(Error::Invalid("discovery continuity fault"));
        }
        Ok(())
    }

    fn record_payload(&mut self, counter: u8) {
        self.next_counter = Some((counter + 1) & 15);
    }
}

fn drain(
    track: &mut Track,
    index: usize,
    clock: &mut DemuxClock,
    totals: &mut Totals,
    cancel: &AtomicBool,
) -> Result<()> {
    loop {
        match track.observer.poll(&mut WorkBudget::new(WORK), cancel)? {
            AssemblyStep::Output(packet) => {
                let time = clock
                    .project_header(
                        packet.pid,
                        packet.source_epochs,
                        packet.time_header(),
                        &mut WorkBudget::new(1),
                        cancel,
                    )?
                    .ok_or("clock yielded")?;
                fold_payload(
                    totals,
                    [
                        index as u64,
                        u64::from(packet.pid),
                        signed(time64(time.pts90)?.unwrap_or(NO_TIME)),
                        signed(time64(time.dts90)?.unwrap_or(NO_TIME)),
                        signed(packet.duration90_raw),
                        signed(packet.position),
                        u64::from(packet.flags),
                        packet.payload_len as u64,
                    ],
                    packet.stream_id,
                    packet.payload(),
                )?;
                track.observer.reuse_payload_spans(packet.spans);
                track
                    .observer
                    .reuse_header_ranges(packet.header_source_ranges);
            }
            AssemblyStep::Yield => {}
            AssemblyStep::NeedPayload | AssemblyStep::Finished => return Ok(()),
        }
    }
}

pub(super) fn operation(pool: &Budget, request: &Request, cancel: &AtomicBool) -> Result<Totals> {
    let raw = &request.input;
    let mut input = InputCursor::new(
        pool,
        FIRST_SOURCE,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        WINDOW,
    )?;
    let mut startup = Ts188Startup::new(&input)?;
    let offset = loop {
        match startup.poll(&mut input, &mut WorkBudget::new(WORK), cancel)? {
            Step::Ready(offset) => break offset,
            Step::NeedInput(r) => provide_input(&mut input, raw, r)?,
            Step::Yield => {}
            _ => return Err("unexpected startup boundary".into()),
        }
    };
    let mut source = TransportStream::new_single_program_feed(
        pool.clone(),
        ParseLimits::default(),
        DescriptorNamespace::Unspecified,
    )?;
    let map = loop {
        match source.poll(&mut input, &mut WorkBudget::new(WORK), cancel)? {
            Step::Ready(TraceEvent::ProgramMap {
                map, active: true, ..
            }) if map.header.extension == request.program => break map,
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(r) => provide_input(&mut input, raw, r)?,
            _ => return Err("requested program has no active PMT".into()),
        }
    };
    let [video, audio] = map.streams.as_slice() else {
        return Err("discovery requires two registered tracks".into());
    };
    if video.stream_type != 0x24 || audio.stream_type != 0x0f {
        return Err("discovery requires HEVC followed by AAC".into());
    }
    let epochs = SourceEpochs::default();
    let bind = |pid, stream_type, stream_id, carriage| -> Result<Track> {
        let scope = DemuxScope {
            program: request.program,
            pid,
            stream_type,
            stream_id,
            entity: FIRST_SOURCE,
            epochs,
        };
        let observer = DemuxObserver::new_registered(
            PROFILE,
            scope,
            carriage,
            pool.clone(),
            ParseLimits::default(),
        )?;
        Ok(Track {
            observer,
            started: false,
            next_counter: None,
        })
    };
    drop(source);
    let mut tracks = [
        bind(video.pid, video.stream_type, 0xe0, RegisteredCarriage::Hevc)?,
        bind(
            audio.pid,
            audio.stream_type,
            0xc0,
            RegisteredCarriage::AacAdts,
        )?,
    ];
    let mut clock = DemuxClock::new(request.program)?;
    let mut totals = Totals::default();
    // Replay the detected grid through the source reader once. Its validated
    // carrier views and table/clock events drive the same bounded fold.
    input.request_range_reusing_window(offset)?;
    let mut replay = TransportStream::new_single_program_feed(
        pool.clone(),
        ParseLimits::default(),
        DescriptorNamespace::Unspecified,
    )?;
    let mut pat_route = None;
    loop {
        let mut completed = None;
        let step =
            replay.poll_feed(
                &mut input,
                &mut WorkBudget::new(WORK),
                cancel,
                &mut |packet, _, _, work| {
                    let Some((index, track)) = tracks.iter_mut().enumerate().find(|(_, track)| {
                        track.started && track.observer.scope().pid == packet.pid
                    }) else {
                        return Ok(FeedContinuation::Emit);
                    };
                    // The source has checked this continuation's transport state.
                    // Keep the original physical ordinal used by ordinary delivery.
                    let context = SourceContext {
                        source_event_ordinal: packet
                            .source_offset
                            .checked_sub(offset)
                            .ok_or(Error::Invalid("replay carrier before the detected grid"))?
                            / 188,
                        epochs,
                        byte_run_epoch: None,
                    };
                    // Leave the source's own unit for consuming the carrier.
                    if work.remaining() < 3 {
                        return Ok(FeedContinuation::Emit);
                    }
                    match track
                        .observer
                        .append_ready_continuation(packet, context, work, cancel)?
                    {
                        ReadyContinuation::Unhandled => Ok(FeedContinuation::Emit),
                        ReadyContinuation::Buffered => Ok(FeedContinuation::Buffered),
                        ReadyContinuation::Complete => {
                            completed = Some(index);
                            Ok(FeedContinuation::Yield)
                        }
                    }
                },
            )?;
        if let Some(index) = completed {
            drain(&mut tracks[index], index, &mut clock, &mut totals, cancel)?;
        }
        match step {
            Step::Ready(TraceEvent::Transport(mut packet)) => {
                let ordinal = packet
                    .source_offset
                    .checked_sub(offset)
                    .ok_or("replay carrier before the detected grid")?
                    / 188;
                if let Some((index, track)) = tracks
                    .iter_mut()
                    .enumerate()
                    .find(|(_, t)| t.observer.scope().pid == packet.pid)
                {
                    if !track.started && !packet.payload_unit_start {
                        continue;
                    }
                    if packet.discontinuity_indicator()
                        && packet
                            .adaptation
                            .as_ref()
                            .is_some_and(|field| field.pcr.is_some())
                    {
                        return Err("discovery clock interval changed".into());
                    }
                    // Ordinary delivery can precede source continuity events.
                    // Check the selected replay before the observer accepts bytes.
                    track.check_counter(&packet)?;
                    if packet.payload.is_some() {
                        track.record_payload(packet.continuity_counter);
                    }
                    track.started = true;
                    let context = SourceContext {
                        source_event_ordinal: ordinal,
                        epochs,
                        byte_run_epoch: None,
                    };
                    // Before the first PMT, the discovery pass already knows
                    // the selected PIDs but the source has no PES routes yet.
                    match track.observer.append_ready_continuation(
                        &mut packet,
                        context,
                        &mut WorkBudget::new(WORK),
                        cancel,
                    )? {
                        ReadyContinuation::Buffered => continue,
                        ReadyContinuation::Unhandled => {
                            track.observer.push(packet, context).map_err(|e| e.error)?;
                        }
                        ReadyContinuation::Complete => {}
                    }
                    drain(track, index, &mut clock, &mut totals, cancel)?;
                }
            }
            Step::Ready(TraceEvent::Pat(pat)) => {
                let mut routes = pat.programs.iter().filter(|p| !p.is_network_pid());
                let route = routes
                    .next()
                    .filter(|route| route.program_number == request.program)
                    .ok_or("discovery program association changed")?
                    .pid;
                if routes.next().is_some() || pat_route.is_some_and(|old| old != route) {
                    return Err("discovery program association changed".into());
                }
                pat_route = Some(route);
            }
            Step::Ready(TraceEvent::ProgramMap {
                map: next,
                active: true,
                ..
            }) => {
                if next.header.extension != request.program
                    || next.source.as_ref() != map.source.as_ref()
                {
                    return Err("discovery program map changed".into());
                }
            }
            Step::Ready(TraceEvent::Timeline { .. } | TraceEvent::Discontinuity { .. }) => {
                return Err("discovery clock interval changed".into());
            }
            Step::Ready(TraceEvent::Continuity {
                outcome:
                    ContinuityOutcome::Lost { .. }
                    | ContinuityOutcome::Damaged
                    | ContinuityOutcome::ExcessDuplicate
                    | ContinuityOutcome::SignalledDiscontinuity,
                ..
            }) => {
                return Err("discovery continuity fault".into());
            }
            Step::Ready(TraceEvent::Diagnostic(fault)) => {
                let at = usize::try_from(fault.location.offset)?;
                let bytes = raw.as_ref().get(at..).unwrap_or_default();
                let adaptation_only = bytes.len() >= 188
                    && bytes[0] == 0x47
                    && bytes[3] & 0x30 == 0x20
                    && bytes[4] == 183;
                let partial_tail = at >= usize::try_from(offset)?
                    && (at - usize::try_from(offset)?) % 188 == 0
                    && bytes.len() < 188
                    && bytes.first() == Some(&0x47);
                if adaptation_only && bytes[5] & 0x80 != 0 {
                    return Err("discovery clock interval changed".into());
                }
                if fault.kind != FaultKind::Transport || (!adaptation_only && !partial_tail) {
                    return Err(format!(
                        "discovery source fault: {:?}: {}",
                        fault.kind, fault.error
                    )
                    .into());
                }
            }
            Step::Ready(TraceEvent::Finished) | Step::End(_) => break,
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(r) => provide_input(&mut input, raw, r)?,
            Step::Backpressure(never) => match never {},
        }
    }
    for (index, track) in tracks.iter_mut().enumerate() {
        track.observer.end_of_source()?;
        drain(track, index, &mut clock, &mut totals, cancel)?;
    }
    Ok(totals)
}
