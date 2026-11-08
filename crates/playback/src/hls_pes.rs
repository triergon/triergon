//! Admission checks every HLS lane makes before accepting a PES.
use crate::{TrackKey, ts_payload::PesFacts};
use triergon_core::{Error, Result, Time};
use triergon_mpegts::compat::pes::DemuxTimeView;

/// Whether a PES belongs to an HLS lane's track and configuration epoch.
pub(crate) fn lane_track(track: TrackKey, configuration_epoch: u64, packet: &PesFacts<'_>) -> bool {
    track
        == (TrackKey::Ts {
            program: packet.program,
            pid: packet.pid,
        })
        && packet.source_epochs.configuration == configuration_epoch
}

/// Whether the demux clock view describes this PES and its header's raw stamps.
pub(crate) fn clock_describes(
    clock: &DemuxTimeView,
    packet: &PesFacts<'_>,
    pts_raw33: Option<u64>,
    dts_raw33: Option<u64>,
) -> bool {
    clock.program == packet.program
        && clock.pid == packet.pid
        && clock.profile == packet.profile
        && clock.source_epochs == packet.source_epochs
        && clock.source_pts_raw33 == pts_raw33
        && clock.source_dts_raw33 == dts_raw33
}

/// The 90 kHz time whose ticks match a PES's raw 33-bit stamp. A PTS-only
/// clock projection can mirror PTS into DTS; an absent stamp stays absent.
pub(crate) fn pes_time(
    raw: Option<u64>,
    ticks: Option<i128>,
    mismatch: &'static str,
) -> Result<Option<Time>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let ticks = ticks
        .filter(|value| value.rem_euclid(1 << 33) == i128::from(raw))
        .ok_or(Error::Invalid(mismatch))?;
    Ok(Some(Time::new(ticks, 90_000)?))
}
