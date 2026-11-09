//! Drive and fold the public opaque recovery operation.

use super::{NO_TIME, WINDOW, WORK, fold_payload, provide_input, time64};
use crate::runner::{Result, Totals};
use crate::ts_input::{FIRST_SOURCE, Request, capabilities};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, InputCursor, Step, WorkBudget};
use triergon_harness_protocol::digest::signed;
pub(crate) use triergon_mpegts::compat::opaque_video::RecoveryTrigger as Trigger;
use triergon_mpegts::compat::opaque_video::{OpaqueRecovery, RecoveredOpaque};
use triergon_mpegts::compat::pes::{DemuxClock, TransportObservation};
use triergon_mpegts::{PacketFormat, ParseLimits};

pub(crate) fn project(
    pool: &Budget,
    request: &Request,
    trigger: Trigger,
    cancel: &AtomicBool,
) -> Result<Option<Totals>> {
    if request.format != PacketFormat::Ts188 || !request.following.is_empty() {
        return Ok(None);
    }
    match drive(pool, request, trigger, cancel) {
        Ok(totals) => Ok(Some(totals)),
        Err(error)
            if matches!(
                error.downcast_ref::<Error>(),
                Some(Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated)
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn drive(
    pool: &Budget,
    request: &Request,
    trigger: Trigger,
    cancel: &AtomicBool,
) -> Result<Totals> {
    let raw = &request.input;
    let mut input = InputCursor::new(pool, FIRST_SOURCE, capabilities(raw), WINDOW)?;
    let mut recovery = OpaqueRecovery::new(
        &input,
        ParseLimits::default(),
        request.program,
        request.namespace,
        trigger,
    )?;
    let mut clock = DemuxClock::new(request.program)?;
    loop {
        match recovery.poll(&mut input, &mut WorkBudget::new(WORK), cancel)? {
            Step::Ready(recovered) => return fold(&recovered, &mut clock, cancel),
            Step::NeedInput(next) => provide_input(&mut input, raw, next)?,
            Step::Yield => {}
            Step::End(_) => return Err("opaque recovery ended before publication".into()),
        }
    }
}

fn fold(
    recovered: &RecoveredOpaque,
    clock: &mut DemuxClock,
    cancel: &AtomicBool,
) -> Result<Totals> {
    let first = recovered
        .observations()
        .first()
        .ok_or("empty opaque payload")?;
    let mut work = WorkBudget::new(1);
    let (pid, pts, dts, position, stream_id) = match first {
        TransportObservation::Accepted(packet) => {
            let time = clock
                .project_header(
                    packet.pid,
                    packet.source_epochs,
                    packet.time_header(),
                    &mut work,
                    cancel,
                )?
                .ok_or("opaque clock yielded on its grant")?;
            (
                packet.pid,
                time.pts90,
                time.dts90,
                packet.position,
                packet.stream_id,
            )
        }
        TransportObservation::Faulted(packet) => {
            let time = clock
                .project_transport_fault(packet, &mut work, cancel)?
                .ok_or("opaque fault clock yielded on its grant")?;
            (
                packet.pid,
                Some(time.pts90),
                Some(time.dts90),
                packet.position,
                packet.stream_id,
            )
        }
        TransportObservation::PesFault(packet) => {
            let time = clock
                .project_pes_fault(packet, &mut work, cancel)?
                .ok_or("opaque PES clock yielded on its grant")?;
            (
                packet.pid,
                time.pts90,
                time.dts90,
                packet.position,
                packet.stream_id,
            )
        }
    };
    let mut totals = Totals::default();
    // The parser-enabled unframed observation keeps the first anchor.
    // Duration and picture flags are absent across this opaque byte run.
    fold_payload(
        &mut totals,
        [
            0,
            u64::from(pid),
            signed(time64(pts)?.unwrap_or(NO_TIME)),
            signed(time64(dts)?.unwrap_or(NO_TIME)),
            0,
            signed(position),
            0,
            recovered.payload_len() as u64,
        ],
        stream_id,
        recovered.payload(),
    )?;
    Ok(totals)
}
