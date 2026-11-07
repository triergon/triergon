//! Separate native packet timing for the admitted single-layer HEVC feed.
use super::{FeedCodec, MatroskaHevcDolbyFeed, MatroskaHevcDolbyPacket, MatroskaHevcDolbyPayload};
use crate::{
    Result,
    matroska_feed::{Association, MatroskaPacketTiming, video_clock::DecodeClock},
    raw_video::hevc_syntax as hevc,
};
use triergon_core::Error;
use triergon_framing::{Codec, HevcAnnexBConfig};

/// One timing projection for an ordered HEVC feed from a random-access
/// picture. Observe each packet once, in the original decode order.
/// The source packet retains its container clocks.
pub struct MatroskaHevcClock<'feed> {
    association: &'feed Association,
    config: &'feed HevcAnnexBConfig,
    period: Option<(u32, u32)>,
    clock: DecodeClock,
}

impl MatroskaHevcDolbyFeed {
    /// Bind a native clock to this feed's single base-layer parameter-set
    /// chain and temporal sublayer. A Dolby audio feed returns `None`.
    pub fn native_hevc_clock(&self) -> Result<Option<MatroskaHevcClock<'_>>> {
        let FeedCodec::Hevc(config) = &self.codec else {
            return Ok(None);
        };
        let chain = hevc::startup_chain(config.record().nals().map(|nal| nal.source_bytes()))?;
        let period = match hevc::vps_timing(chain.vps)? {
            Some(period) => Some(period),
            None => hevc::sps_timing(chain.sps)?
                .map(|timing| (timing.num_units_in_tick, timing.time_scale)),
        };
        Ok(Some(MatroskaHevcClock {
            association: &self.association,
            config,
            period,
            clock: DecodeClock::new(Codec::Hevc, chain.pps_id, chain.reorder),
        }))
    }
}

impl MatroskaHevcClock<'_> {
    /// Project one complete checked sample. A parameter-set change or
    /// unsupported sample ends this clock's continuity.
    pub fn observe(
        &mut self,
        packet: &MatroskaHevcDolbyPacket<'_>,
    ) -> Result<MatroskaPacketTiming> {
        let MatroskaHevcDolbyPayload::Hevc(unit) = &packet.payload else {
            return Err(Error::Invalid("native HEVC clock packet codec"));
        };
        if !self.association.matches(packet.source)
            || !std::ptr::eq(self.config, unit.configuration())
        {
            return Err(Error::Invalid("native HEVC clock feed association"));
        }
        self.clock
            .observe(packet.source, unit.source_nals(), |tick_ns| {
                self.period.and_then(|period| duration_ns(period, tick_ns))
            })
    }
}

fn duration_ns((units, scale): (u32, u32), tick_ns: u64) -> Option<u64> {
    // The selected native path uses the codec clock on a millisecond or
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
