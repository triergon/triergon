//! Raw AVC or HEVC Annex B through the playback engine: probe the codec from a
//! preloaded forward-only source with `RawVideoProbe`, replay the probed prefix
//! through `RawVideoSource`, attach `RawVideoFeed` before the first access unit
//! and fold every access unit through the end of the source.
//!
//! Annex B carries no container clock: PTS and DTS fold as unknown. The duration
//! is the picture period of the active SPS timing (`2·num_units_in_tick /
//! time_scale` for AVC, `num_units_in_tick / time_scale` for HEVC) in 1/1,200,000 s
//! ticks, the raw demuxer's time base, and unknown without SPS timing. The flag is
//! the coded IDR or IRAP indication.

use crate::packet_fold::{Packet, Work, drive};
use crate::runner::Result;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_framing::{Codec, RawAnnexBLimits};
use triergon_playback::{
    ByteSource,
    raw_video::{
        RawFeedClock, RawVideoConfiguration, RawVideoProbe, RawVideoProbeLimits, RawVideoProbeStep,
    },
};

pub const PREFIX: &str = "raw-video-";
const OPERATION: &str = "raw-annex-b-feed";
const QUANTUM: usize = 4096;
const WORK: u64 = 4096;
const TICKS_PER_SECOND: u64 = 1_200_000;
const MAX_PARAMETER_SET_BYTES: usize = 4096;

/// The picture period in raw-demuxer ticks, when the active SPS signals one
/// that is a whole number of ticks.
fn picture_ticks(configuration: Option<&RawVideoConfiguration>) -> Result<u64> {
    let Some(configuration) = configuration else {
        return Ok(0);
    };
    let (units, scale) = match configuration.codec {
        Codec::Avc => match configuration.avc_timing {
            Some(timing) => (
                2 * u64::from(timing.num_units_in_tick),
                u64::from(timing.time_scale),
            ),
            None => return Ok(0),
        },
        Codec::Hevc => match configuration.hevc_timing()? {
            Some(timing) => (
                u64::from(timing.num_units_in_tick),
                u64::from(timing.time_scale),
            ),
            None => return Ok(0),
        },
    };
    let ticks = units * TICKS_PER_SECOND;
    Ok(if scale != 0 && ticks.is_multiple_of(scale) {
        ticks / scale
    } else {
        0
    })
}

fn operation(pool: &Budget, raw: &Bytes, codec: Codec, mut work: Work) -> Result<Work> {
    let cancel = AtomicBool::new(false);
    let id = SourceId(43);
    let mut probe = RawVideoProbe::new(
        pool,
        id,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        ByteSource::new(id, raw.clone(), QUANTUM)?,
        RawVideoProbeLimits::default(),
        RawAnnexBLimits::default(),
    )?;
    let detected = loop {
        match probe.advance(&mut WorkBudget::new(WORK), &cancel)? {
            RawVideoProbeStep::Detected(codec) => break codec,
            RawVideoProbeStep::NeedInput(_) | RawVideoProbeStep::Yield => {}
        }
    };
    if detected != codec {
        return Err(format!("probe detected {detected:?}").into());
    }
    let mut feed = probe
        .finish()?
        .into_feed(MAX_PARAMETER_SET_BYTES, RawFeedClock::Unspecified)?;
    loop {
        match feed.poll(&mut WorkBudget::new(WORK), &cancel)? {
            Step::Ready(packet) => {
                let unit = &packet.unit;
                let size = usize::try_from(unit.range.end - unit.range.start)?;
                work.packet(
                    Packet::new(0, None, None, size, unit.segments())
                        .duration(picture_ticks(packet.configuration.as_ref())?)
                        .flags(u64::from(unit.random_access)),
                )?;
            }
            Step::End(at) if at.offset == raw.len() as u64 => break,
            Step::End(_) => return Err("raw feed ended before the source".into()),
            Step::NeedInput(_) | Step::Yield => {}
        }
    }
    Ok(work)
}

/// `raw-video-fold FILE CODEC`, `raw-video-bench FILE ITERATIONS CODEC` or
/// `raw-video-memory FILE CODEC`; CODEC is `avc` or `hevc`.
pub fn run(args: &[String]) -> Result<()> {
    drive(args, PREFIX, OPERATION, operate)
}

/// One operation on a preloaded input with the case argument.
pub fn operate(pool: &Budget, raw: &Bytes, argument: &str, work: Work) -> Result<Work> {
    let codec = match argument {
        "avc" => Codec::Avc,
        "hevc" => Codec::Hevc,
        _ => return Err("expected avc or hevc".into()),
    };
    operation(pool, raw, codec, work)
}
