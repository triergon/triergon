//! Native-compatible timing for ordered AC-3 family packets.
use super::{
    FeedCodec, MatroskaHevcDolbyFeed, MatroskaHevcDolbyPacket, MatroskaHevcDolbyPayload,
    check_limit, dolby_duration_ns,
};
use crate::{
    Result,
    matroska_feed::probe_clock::{NativePacketTiming, PacketClock, ProbeTimeline, Timeline},
};
use triergon_core::{Budget, BudgetVec, Error};

const MAX_PREFIX_PACKETS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatroskaDolbyTiming {
    pub pts_ns: Option<i64>,
    pub dts_ns: Option<i64>,
    pub duration_ns: Option<u64>,
}

impl From<NativePacketTiming> for MatroskaDolbyTiming {
    fn from(timing: NativePacketTiming) -> Self {
        Self {
            pts_ns: timing.pts,
            dts_ns: timing.dts,
            duration_ns: timing.duration_ns,
        }
    }
}

impl MatroskaHevcDolbyFeed {
    /// Prepare one bounded prefix in the same generation as this feed.
    /// The caller retains the original packets in playback order.
    pub fn native_dolby_clock_probe(
        &self,
        budget: &Budget,
        maximum_packets: usize,
    ) -> Result<Option<MatroskaDolbyClockProbe<'_>>> {
        if !matches!(self.codec, FeedCodec::Dolby(_)) {
            return Ok(None);
        }
        if !self.association.configuration.belongs_to(budget) {
            return Err(Error::ForeignPool.into());
        }
        check_limit(maximum_packets, MAX_PREFIX_PACKETS)?;
        Ok(Some(MatroskaDolbyClockProbe {
            feed: self,
            maximum_packets,
            rows: BudgetVec::new(budget),
            timeline: ProbeTimeline::default(),
        }))
    }

    fn dolby_timing(&self, packet: &MatroskaHevcDolbyPacket<'_>) -> Result<NativePacketTiming> {
        let (FeedCodec::Dolby(codec), MatroskaHevcDolbyPayload::Dolby { family, raw }) =
            (&self.codec, &packet.payload)
        else {
            return Err(Error::Invalid("native Dolby clock packet codec").into());
        };
        if !self.association.matches(packet.source)
            || *family != codec.family()
            || !std::ptr::eq(*raw, packet.source.packet.data.as_ref())
        {
            return Err(Error::Invalid("native Dolby clock feed association").into());
        }
        check_limit(raw.len(), self.limits.dolby_packet.maximum_packet_bytes)?;
        let mut timing = NativePacketTiming::demux(packet.source)?;
        if timing.duration_ns.is_none() {
            // The native header parser supplies duration in whole packet ticks
            // before the demuxer uses it to advance PTS and DTS.
            timing.duration_ns =
                dolby_duration_ns(*family, raw, packet.source.packet.timing.scale_ns);
        }
        Ok(timing)
    }
}

/// Retain scalar timing rows for the caller's selected packets during discovery.
pub struct MatroskaDolbyClockProbe<'feed> {
    feed: &'feed MatroskaHevcDolbyFeed,
    maximum_packets: usize,
    rows: BudgetVec<PacketClock>,
    timeline: ProbeTimeline,
}

impl<'feed> MatroskaDolbyClockProbe<'feed> {
    /// Observe one checked packet and return its ordinal within this prefix.
    /// Allocation pressure leaves both the ordinal and the clock unchanged.
    pub fn push(&mut self, packet: &MatroskaHevcDolbyPacket<'_>) -> Result<usize> {
        let ordinal = self.rows.len();
        check_limit(
            ordinal.checked_add(1).ok_or(Error::Overflow)?,
            self.maximum_packets,
        )?;
        let timing = self.feed.dolby_timing(packet)?;
        let mut next = self.timeline;
        let row = next.push(ordinal, timing)?;
        self.rows.try_push(row).map_err(|failure| failure.error)?;
        self.timeline = next;
        Ok(ordinal)
    }

    /// Freeze the observed prefix before delivering any of its clock rows.
    pub fn finish(self) -> MatroskaDolbyClockPrefix<'feed> {
        MatroskaDolbyClockPrefix(self)
    }
}

/// Replays the probe's timestamp projection without retaining packet payloads.
pub struct MatroskaDolbyClockPrefix<'feed>(MatroskaDolbyClockProbe<'feed>);

impl<'feed> MatroskaDolbyClockPrefix<'feed> {
    pub fn timing(&self, ordinal: usize) -> Result<MatroskaDolbyTiming> {
        let row = self
            .0
            .rows
            .as_slice()
            .get(ordinal)
            .ok_or(Error::Invalid("Dolby clock prefix ordinal"))?;
        Ok(self.0.timeline.resolve(ordinal, *row)?.into())
    }

    /// Continue the same clock after the caller replays the prefix.
    pub fn into_live(self) -> MatroskaDolbyClock<'feed> {
        MatroskaDolbyClock {
            feed: self.0.feed,
            timeline: self.0.timeline.into_live(),
        }
    }
}

/// Forward clock for one feed binding. Rebuild it on a new seek generation.
pub struct MatroskaDolbyClock<'feed> {
    feed: &'feed MatroskaHevcDolbyFeed,
    timeline: Timeline,
}

impl MatroskaDolbyClock<'_> {
    pub fn observe(&mut self, packet: &MatroskaHevcDolbyPacket<'_>) -> Result<MatroskaDolbyTiming> {
        let timing = self.feed.dolby_timing(packet)?;
        let mut next = self.timeline;
        let row = next.push(timing)?;
        let result = next.resolve(row)?;
        self.timeline = next;
        Ok(result.into())
    }
}
