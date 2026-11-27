//! The demux packet clock of the single-layer HEVC feed, kept beside its source times.
use super::{FeedCodec, MatroskaHevcDolbyFeed, MatroskaHevcDolbyPacket, MatroskaHevcDolbyPayload};
use crate::matroska::{Association, MatroskaPacketTiming, video_clock::DecodeClock};
use triergon_core::{Error, Result};
use triergon_framing::{
    hevc::{HevcAnnexBConfig, check_parameter_sets, read_sps_timing, read_vps_timing},
    nal::Codec,
};
use triergon_raw_video::hevc_startup as hevc;

/// One timing projection for an ordered HEVC feed from a random-access
/// picture. Observe each packet once, in the original decode order.
/// The source packet retains its container clocks.
#[derive(Debug)]
pub struct MatroskaHevcClock<'feed> {
    association: &'feed Association,
    config: &'feed HevcAnnexBConfig,
    parameter_sets: [&'feed [u8]; 3],
    repeat_prefixes_validated: bool,
    period: Option<(u32, u32)>,
    clock: DecodeClock,
}

impl MatroskaHevcDolbyFeed {
    /// Bind a demux clock to this feed's single base-layer parameter-set
    /// chain and temporal sublayer. A Dolby audio feed returns `None`.
    pub fn demux_hevc_clock(&self) -> Result<Option<MatroskaHevcClock<'_>>> {
        let FeedCodec::Hevc(config) = &self.codec else {
            return Ok(None);
        };
        let chain = hevc::startup_chain(config.record().nals().map(|nal| nal.source_bytes()))?;
        let period = match read_vps_timing(chain.vps)? {
            Some(period) => Some(period),
            None => read_sps_timing(chain.sps)?
                .map(|timing| (timing.num_units_in_tick, timing.time_scale)),
        };
        Ok(Some(MatroskaHevcClock {
            association: &self.association,
            config,
            parameter_sets: [chain.vps, chain.sps, chain.pps],
            repeat_prefixes_validated: false,
            period,
            clock: DecodeClock::new(Codec::Hevc, chain.pps_id, chain.reorder),
        }))
    }
}

impl MatroskaHevcClock<'_> {
    /// Project one complete checked sample. Matching parameter sets may
    /// repeat before the first slice. A changed set or unsupported sample
    /// ends this clock's continuity.
    pub fn observe(
        &mut self,
        packet: &MatroskaHevcDolbyPacket<'_>,
    ) -> Result<MatroskaPacketTiming> {
        let MatroskaHevcDolbyPayload::Hevc(unit) = &packet.payload else {
            return Err(Error::Invalid("HEVC demux clock requires an HEVC packet"));
        };
        if !self.association.matches(packet.source)
            || !std::ptr::eq(self.config, unit.configuration())
        {
            return Err(Error::Invalid(
                "HEVC packet belongs to a different feed or configuration",
            ));
        }
        self.clock.observe(
            packet.source,
            unit.source_nals(),
            |nal| {
                let Some((index, payload)) = hevc::parameter_set_payload(nal) else {
                    return Ok(false);
                };
                if self.parameter_sets[index] != payload {
                    return Ok(false);
                }
                if !self.repeat_prefixes_validated {
                    let [vps, sps, pps] = self.parameter_sets;
                    check_parameter_sets(vps, sps, pps)?;
                    self.repeat_prefixes_validated = true;
                }
                Ok(true)
            },
            |tick_ns| self.period.and_then(|period| duration_ns(period, tick_ns)),
        )
    }
}

fn duration_ns((units, scale): (u32, u32), tick_ns: u64) -> Option<u64> {
    // The reference uses the codec clock on a millisecond or
    // finer packet grid. Bound the rational to an exactly representable
    // receiver rate strictly below 1,000 frames per second.
    if units == 0
        || scale == 0
        || units > 1 << 30
        || scale > 1 << 30
        || u64::from(scale) >= u64::from(units) * 1_000
        || tick_ns > 1_000_000
    {
        return None;
    }
    let nanoseconds = u64::from(units) * 1_000_000_000 / u64::from(scale);
    let ticks = nanoseconds.checked_div(tick_ns)?;
    (ticks != 0).then_some(ticks * tick_ns)
}
