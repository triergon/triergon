//! Native-compatible timing for ordered indexed AAC-LC core packets.
use super::{
    FeedCodec, MatroskaFeedPacket, MatroskaFeedPayload, MatroskaTrackFeed, check_limit,
    probe_clock::syncframe_duration_ns,
};
use crate::{
    Result,
    matroska_feed::probe_clock::{NativePacketTiming, PacketClock, ProbeTimeline, Timeline},
};
use triergon_core::{Budget, BudgetVec, Error};
use triergon_framing::indexed_aac_frequency;

const MAX_PREFIX_PACKETS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatroskaAacTiming {
    pub pts_ns: Option<i64>,
    pub dts_ns: Option<i64>,
    pub duration_ns: Option<u64>,
}

impl From<NativePacketTiming> for MatroskaAacTiming {
    fn from(timing: NativePacketTiming) -> Self {
        Self {
            pts_ns: timing.pts,
            dts_ns: timing.dts,
            duration_ns: timing.duration_ns,
        }
    }
}

impl MatroskaTrackFeed {
    /// Prepare one bounded prefix in the same generation as this feed.
    /// The caller retains the original packets in playback order.
    pub fn native_aac_clock_probe(
        &self,
        budget: &Budget,
        maximum_packets: usize,
    ) -> Result<Option<MatroskaAacClockProbe<'_>>> {
        if !matches!(self.codec, FeedCodec::Aac(_)) {
            return Ok(None);
        }
        if !self.association.configuration.belongs_to(budget) {
            return Err(Error::ForeignPool.into());
        }
        check_limit(maximum_packets, MAX_PREFIX_PACKETS)?;
        Ok(Some(MatroskaAacClockProbe {
            feed: self,
            maximum_packets,
            rows: BudgetVec::new(budget),
            timeline: ProbeTimeline::default(),
        }))
    }

    fn aac_timing(&self, packet: &MatroskaFeedPacket<'_>) -> Result<NativePacketTiming> {
        let (FeedCodec::Aac(params), MatroskaFeedPayload::Aac { raw, .. }) =
            (&self.codec, &packet.payload)
        else {
            return Err(Error::Invalid("native AAC clock packet codec").into());
        };
        if !self.association.matches(packet.source)
            || !std::ptr::eq(*raw, packet.source.packet.data.as_ref())
        {
            return Err(Error::Invalid("native AAC clock feed association").into());
        }
        check_limit(
            raw.len().checked_add(7).ok_or(Error::Overflow)?,
            self.limits.aac_output_bytes,
        )?;
        let mut timing = NativePacketTiming::demux(packet.source)?;
        if timing.duration_ns.is_none() {
            // Indexed AAC-LC cores admitted by this feed have 1024 samples.
            let rate = indexed_aac_frequency(params.frequency_index)
                .ok_or(Error::Invalid("AAC clock frequency"))?;
            timing.duration_ns =
                syncframe_duration_ns(1024, rate, packet.source.packet.timing.scale_ns);
        }
        Ok(timing)
    }
}

/// Retain scalar timing rows for the caller's selected packets during discovery.
pub struct MatroskaAacClockProbe<'feed> {
    feed: &'feed MatroskaTrackFeed,
    maximum_packets: usize,
    rows: BudgetVec<PacketClock>,
    timeline: ProbeTimeline,
}

impl<'feed> MatroskaAacClockProbe<'feed> {
    /// Observe one checked packet and return its ordinal within this prefix.
    /// Allocation pressure leaves both the ordinal and the clock unchanged.
    pub fn push(&mut self, packet: &MatroskaFeedPacket<'_>) -> Result<usize> {
        let ordinal = self.rows.len();
        check_limit(
            ordinal.checked_add(1).ok_or(Error::Overflow)?,
            self.maximum_packets,
        )?;
        let timing = self.feed.aac_timing(packet)?;
        let mut next = self.timeline;
        let row = next.push(ordinal, timing)?;
        self.rows.try_push(row).map_err(|failure| failure.error)?;
        self.timeline = next;
        Ok(ordinal)
    }

    /// Freeze the observed prefix before delivering any of its clock rows.
    pub fn finish(self) -> MatroskaAacClockPrefix<'feed> {
        MatroskaAacClockPrefix(self)
    }
}

/// Replays the probe's timestamp projection without retaining packet payloads.
pub struct MatroskaAacClockPrefix<'feed>(MatroskaAacClockProbe<'feed>);

impl<'feed> MatroskaAacClockPrefix<'feed> {
    pub fn timing(&self, ordinal: usize) -> Result<MatroskaAacTiming> {
        let row = self
            .0
            .rows
            .as_slice()
            .get(ordinal)
            .ok_or(Error::Invalid("AAC clock prefix ordinal"))?;
        Ok(self.0.timeline.resolve(ordinal, *row)?.into())
    }

    /// Continue the same clock after the caller replays the prefix.
    pub fn into_live(self) -> MatroskaAacClock<'feed> {
        MatroskaAacClock {
            feed: self.0.feed,
            timeline: self.0.timeline.into_live(),
        }
    }
}

/// Forward clock for one feed binding. Rebuild it on a new seek generation.
pub struct MatroskaAacClock<'feed> {
    feed: &'feed MatroskaTrackFeed,
    timeline: Timeline,
}

impl MatroskaAacClock<'_> {
    pub fn observe(&mut self, packet: &MatroskaFeedPacket<'_>) -> Result<MatroskaAacTiming> {
        let timing = self.feed.aac_timing(packet)?;
        let mut next = self.timeline;
        let row = next.push(timing)?;
        let result = next.resolve(row)?;
        self.timeline = next;
        Ok(result.into())
    }
}
