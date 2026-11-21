//! An existing ASC record and a real source MP4 access unit -> bounded ADTS.
//! The caller supplies the configuration association. Its raw source/provenance
//! remains separate from packet-source generation and the constructed header.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, Result};
use triergon_framing::{
    BuildStep,
    aac::{AacAdtsOutput, AacAdtsParameters, AacAdtsWriter, AacAudioSpecificConfig, AacSource},
};
use triergon_playback::Mp4Packet;
/// Wraps the raw AAC access units of one MP4 track and configuration revision in
/// ADTS headers.
#[derive(Debug)]
pub struct AacBridge {
    pool: Budget,
    source: triergon_core::SourceId,
    track: u32,
    revision: u64,
    configuration: AacAudioSpecificConfig,
    parameters: AacAdtsParameters,
    maximum: usize,
}
/// The ADTS framing of one MP4 packet, stepped within a work budget.
#[derive(Debug)]
pub struct AacPacketJob {
    source: Option<Mp4Packet>,
    writer: AacAdtsWriter,
}
/// One MP4 packet framed as ADTS.
#[derive(Debug)]
pub struct AacPacketOutput {
    /// The source packet.
    pub source: Mp4Packet,
    /// The ADTS header and the packet's payload.
    pub adts: AacAdtsOutput,
}
impl AacBridge {
    /// A bridge for `track` of `source` at configuration `revision`, writing frames of at
    /// most `maximum` bytes. Fails with `ForeignPool` when `configuration` is not charged
    /// to `pool` and with `Invalid` for track 0 or a `maximum` below 7. It also fails
    /// when ADTS cannot carry the configuration.
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
    /// The AudioSpecificConfig the headers are written from.
    pub fn configuration(&self) -> &AacAudioSpecificConfig {
        &self.configuration
    }
    /// Starts framing `packet`. Fails with `SourceMismatch` for another source and with
    /// `Invalid` when its track, revision or `mp4a` sample entry differs from the
    /// bridge's.
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
    /// Writes the frame within `w`: `Ready` once, then `Complete`.
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
