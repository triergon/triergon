//! An existing ASC record and a real source MP4 access unit -> bounded ADTS.
//! The caller supplies the configuration association. Its raw source/provenance
//! remains separate from packet-source generation and the constructed header.
use crate::{Mp4Packet, Result};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error};
use triergon_framing::{
    AacAdtsOutput, AacAdtsParameters, AacAdtsWriter, AacAudioSpecificConfig, AacSource, BuildStep,
};
pub struct AacBridge {
    pool: Budget,
    source: triergon_core::SourceId,
    track: u32,
    revision: u64,
    configuration: AacAudioSpecificConfig,
    parameters: AacAdtsParameters,
    maximum: usize,
}
pub struct AacPacketJob {
    source: Option<Mp4Packet>,
    writer: AacAdtsWriter,
}
pub struct AacPacketOutput {
    pub source: Mp4Packet,
    pub adts: AacAdtsOutput,
}
impl AacBridge {
    pub fn new(
        pool: &Budget,
        source: triergon_core::SourceId,
        track: u32,
        revision: u64,
        configuration: AacAudioSpecificConfig,
        maximum: usize,
    ) -> Result<Self> {
        if !configuration.source_bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if track == 0 || maximum < 7 {
            return Err(Error::Invalid("AAC bridge policy"));
        }
        let parameters = AacAdtsParameters::from_asc(configuration.facts())?;
        Ok(Self {
            pool: pool.clone(),
            source,
            track,
            revision,
            configuration,
            parameters,
            maximum,
        })
    }
    pub fn configuration(&self) -> &AacAudioSpecificConfig {
        &self.configuration
    }
    pub fn packet(&self, packet: Mp4Packet) -> Result<AacPacketJob> {
        if packet.source.source != self.source {
            return Err(Error::SourceMismatch);
        }
        if packet.configuration.track.track_id != self.track
            || packet.configuration.revision != self.revision
            || packet.configuration.description.format != *b"mp4a"
        {
            return Err(Error::Invalid(
                "AAC source configuration association changed",
            ));
        }
        let writer = AacAdtsWriter::new(
            &self.pool,
            AacSource {
                source: packet.source.source,
                generation: packet.source.token.generation(),
                offset: packet.source.sample.range.start,
            },
            packet.source.payload.clone(),
            self.parameters,
            self.maximum,
        )?;
        Ok(AacPacketJob {
            source: Some(packet),
            writer,
        })
    }
}
impl AacPacketJob {
    pub fn step(
        &mut self,
        w: &mut triergon_core::WorkBudget,
        c: &AtomicBool,
    ) -> Result<BuildStep<AacPacketOutput>> {
        Ok(match self.writer.step(w, c)? {
            BuildStep::Yield => BuildStep::Yield,
            BuildStep::Complete => BuildStep::Complete,
            BuildStep::Ready(adts) => BuildStep::Ready(AacPacketOutput {
                source: self
                    .source
                    .take()
                    .ok_or(Error::Invalid("AAC source output already consumed"))?,
                adts,
            }),
        })
    }
}
