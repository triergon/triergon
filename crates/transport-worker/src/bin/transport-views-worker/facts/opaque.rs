use triergon_mpegts::compat::opaque_video::{OpaqueRecovery, RecoveredOpaque, RecoveryTrigger};
use triergon_mpegts::compat::pes::RecoveredPes;

use super::*;

pub(super) fn run(
    pool: &Budget,
    bytes: &Bytes,
    program: u16,
    pid: u16,
    out: &mut Output<'_, '_>,
) -> Result<(), String> {
    let caps = SourceCapabilities {
        length: Some(bytes.len() as u64),
        random_access: false,
    };
    let cancel = AtomicBool::new(false);
    let limits = ParseLimits::default();
    let mut source = TransportStream::new(pool.clone(), PacketFormat::Ts188, limits, program)
        .map_err(explain)?;
    let mut cursor = InputCursor::new(pool, SOURCE, caps, 32 * 1024).map_err(explain)?;
    let trigger = loop {
        match source
            .poll(&mut cursor, &mut WorkBudget::new(WORK), &cancel)
            .map_err(explain)?
        {
            Step::Ready(TraceEvent::Diagnostic(fault)) => {
                if let Some(trigger) = RecoveryTrigger::from_fault(&fault) {
                    break trigger;
                }
            }
            Step::Ready(TraceEvent::Finished) => {
                return Err("source completed without an opaque recovery trigger".into());
            }
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(request) => provide(&mut cursor, bytes, request).map_err(explain)?,
            Step::End(_) => source.end_of_source().map_err(explain)?,
        }
    };
    drop(source);
    let mut cursor = InputCursor::new(pool, SOURCE, caps, 32 * 1024).map_err(explain)?;
    let mut recovery = OpaqueRecovery::new(
        &cursor,
        limits,
        program,
        DescriptorNamespace::Unspecified,
        trigger,
    )
    .map_err(explain)?;
    loop {
        match recovery
            .poll(&mut cursor, &mut WorkBudget::new(WORK), &cancel)
            .map_err(explain)?
        {
            Step::Ready(recovered) => return packet(&recovered, program, pid, &cancel, out),
            Step::NeedInput(request) => provide(&mut cursor, bytes, request).map_err(explain)?,
            Step::Yield => {}
            Step::End(_) => return Err("opaque recovery ended before publication".into()),
        }
    }
}

fn packet(
    recovered: &RecoveredOpaque,
    program: u16,
    pid: u16,
    cancel: &AtomicBool,
    out: &mut Output<'_, '_>,
) -> Result<(), String> {
    let mut clock = DemuxClock::new(program).map_err(explain)?;
    let mut grant = WorkBudget::new(WORK);
    let first = recovered
        .observations()
        .first()
        .ok_or("empty opaque payload")?;
    let (actual_pid, pts, dts, pos) = match first {
        RecoveredPes::Accepted(packet) => {
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
            (packet.pid, time.pts90, time.dts90, packet.position)
        }
        RecoveredPes::Faulted(packet) => {
            let time = clock
                .project_transport_fault(packet, &mut grant, cancel)
                .map_err(explain)?
                .ok_or("transport clock yielded")?;
            (
                packet.pid,
                Some(time.pts90),
                Some(time.dts90),
                packet.position,
            )
        }
        RecoveredPes::PesFault(packet) => {
            let time = clock
                .project_pes_fault(packet, &mut grant, cancel)
                .map_err(explain)?
                .ok_or("PES clock yielded")?;
            (packet.pid, time.pts90, time.dts90, packet.position)
        }
    };
    if actual_pid != pid {
        return Err("opaque recovery reported another PID".into());
    }
    if let Some(trace) = &mut out.trace {
        writeln!(
            trace,
            "# opaque {} {:?} {}",
            recovered.payload_len(),
            recovered.source_fault().error,
            recovered.physical_end().location().offset
        )
        .map_err(explain)?;
    }
    out.packet(pid, pts, dts, pos, 0, recovered.payload())
}
