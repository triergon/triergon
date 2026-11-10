//! Native-compatible timing for ordered indexed AAC-LC core packets.
use super::{
    FeedCodec, MatroskaFeedPacket, MatroskaFeedPayload, MatroskaTrackFeed, check_limit,
    probe_clock::{
        MatroskaClock, MatroskaClockPrefix, MatroskaClockProbe, MatroskaPacketTiming,
        NativePacketTiming, syncframe_duration_ns,
    },
};
use crate::Result;
use triergon_core::{Budget, Error};
use triergon_framing::indexed_aac_frequency;

impl MatroskaTrackFeed {
    /// Prepare one bounded prefix in the same generation as this feed.
    /// The caller retains the original packets in playback order.
    pub fn native_aac_clock_probe(
        &self,
        budget: &Budget,
        maximum_packets: usize,
    ) -> Result<Option<MatroskaClockProbe<'_, Self>>> {
        if !matches!(self.codec, FeedCodec::Aac { .. }) {
            return Ok(None);
        }
        if !self.association.configuration.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        MatroskaClockProbe::new(self, budget, maximum_packets).map(Some)
    }

    fn aac_timing(&self, packet: &MatroskaFeedPacket<'_>) -> Result<NativePacketTiming> {
        let (
            FeedCodec::Aac {
                params,
                infer_duration,
            },
            MatroskaFeedPayload::Aac { raw, .. },
        ) = (&self.codec, &packet.payload)
        else {
            return Err(Error::Invalid("native AAC clock packet codec"));
        };
        if !self.association.matches(packet.source)
            || !std::ptr::eq(*raw, packet.source.packet.data.as_ref())
        {
            return Err(Error::Invalid("native AAC clock feed association"));
        }
        check_limit(
            raw.len().checked_add(7).ok_or(Error::Overflow)?,
            self.limits.aac_output_bytes,
        )?;
        let mut timing = NativePacketTiming::demux(packet.source)?;
        if timing.duration_ns.is_none() && *infer_duration {
            // Indexed AAC-LC cores admitted by this feed have 1024 samples.
            let rate = indexed_aac_frequency(params.frequency_index)
                .ok_or(Error::Invalid("AAC clock frequency"))?;
            timing.duration_ns =
                syncframe_duration_ns(1024, rate, packet.source.packet.timing.scale_ns);
        }
        Ok(timing)
    }
}

impl MatroskaClockProbe<'_, MatroskaTrackFeed> {
    /// Observe one checked packet and return its ordinal within this prefix.
    /// Allocation pressure leaves both the ordinal and the clock unchanged.
    pub fn push(&mut self, packet: &MatroskaFeedPacket<'_>) -> Result<usize> {
        self.push_timing(|feed| feed.aac_timing(packet))
    }
}

impl MatroskaClockPrefix<'_, MatroskaTrackFeed> {
    pub fn timing(&self, ordinal: usize) -> Result<MatroskaPacketTiming> {
        self.resolve(ordinal, "AAC clock prefix ordinal")
    }
}

impl MatroskaClock<'_, MatroskaTrackFeed> {
    pub fn observe(&mut self, packet: &MatroskaFeedPacket<'_>) -> Result<MatroskaPacketTiming> {
        self.advance(|feed| feed.aac_timing(packet))
    }
}
