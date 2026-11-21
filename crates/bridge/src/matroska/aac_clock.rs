//! The demux clock of AAC-LC packets in order.
use super::{
    FeedCodec, MatroskaFeedPacket, MatroskaFeedPayload, MatroskaTrackFeed, check_limit,
    probe_clock::{
        DemuxPacketTiming, MatroskaClock, MatroskaClockPrefix, MatroskaClockProbe,
        MatroskaPacketTiming, syncframe_duration_ns,
    },
};
use triergon_core::{Budget, Error, Result};
use triergon_framing::aac;

impl MatroskaTrackFeed {
    /// Prepare one bounded prefix in the same generation as this feed.
    /// The caller retains the original packets in playback order.
    pub fn demux_aac_clock_probe(
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

    fn aac_timing(&self, packet: &MatroskaFeedPacket<'_>) -> Result<DemuxPacketTiming> {
        let (
            FeedCodec::Aac {
                params,
                infer_duration,
            },
            MatroskaFeedPayload::Aac { raw, .. },
        ) = (&self.codec, &packet.payload)
        else {
            return Err(Error::Invalid("AAC demux clock requires an AAC packet"));
        };
        if !self.association.matches(packet.source)
            || !std::ptr::eq(*raw, packet.source.packet.data.as_ref())
        {
            return Err(Error::Invalid(
                "AAC packet belongs to a different feed or configuration",
            ));
        }
        check_limit(
            raw.len().checked_add(7).ok_or(Error::Overflow)?,
            self.limits.aac_output_bytes,
        )?;
        let mut timing = DemuxPacketTiming::demux(packet.source)?;
        if timing.duration_ns.is_none() && *infer_duration {
            // Indexed AAC-LC cores admitted by this feed have 1024 samples.
            let rate = aac::indexed_frequency(params.frequency_index)
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
    /// The clock of the packet the probe numbered `ordinal`. Fails with `Invalid` for an
    /// ordinal it did not give.
    pub fn timing(&self, ordinal: usize) -> Result<MatroskaPacketTiming> {
        self.resolve(ordinal, "AAC clock prefix ordinal")
    }
}

impl MatroskaClock<'_, MatroskaTrackFeed> {
    /// The clock of the next framed packet.
    pub fn observe(&mut self, packet: &MatroskaFeedPacket<'_>) -> Result<MatroskaPacketTiming> {
        self.advance(|feed| feed.aac_timing(packet))
    }
}
