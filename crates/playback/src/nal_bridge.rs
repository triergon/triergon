//! Real MP4 sample-entry -> bounded AVC config -> pinned packet projection.
//! All packet clocks/side facts remain opaque owned metadata of the framing job.
use crate::{Mp4Packet, Result, mp4_plan::Mp4Configuration};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, SourceId, SourceLocation, WorkBudget};
use triergon_framing::{
    AvcConfigLimits, AvcConfigParser, AvcConfigSource, BuildStep, NalPacket,
    NalPacketConfiguration, NalPacketJob, NalPacketLimits, NalPacketSession,
};
use triergon_isobmff::{BoxChildren, BoxStep, EntryLayout, Limits};
pub struct AvcBridgeSetup {
    pool: Budget,
    source: SourceId,
    configuration: Mp4Configuration,
    children: BoxChildren,
    parser: Option<AvcConfigParser>,
    limits: AvcConfigLimits,
    packet_limits: NalPacketLimits,
    done: bool,
}
pub struct AvcBridge {
    session: NalPacketSession,
    source: SourceId,
    track: u32,
    configuration_revision: u64,
}
impl AvcBridgeSetup {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        configuration: Mp4Configuration,
        limits: AvcConfigLimits,
        packet_limits: NalPacketLimits,
    ) -> Result<Self> {
        if !matches!(&configuration.description.format, b"avc1" | b"avc3") {
            return Err(Error::Invalid("source entry is not AVC"));
        }
        if !configuration.description.raw().bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let children = configuration
            .description
            .extensions(EntryLayout::Visual, Limits::default())?;
        Ok(Self {
            pool: pool.clone(),
            source,
            configuration,
            children,
            parser: None,
            limits,
            packet_limits,
            done: false,
        })
    }
    pub fn poll(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<Option<AvcBridge>> {
        if c.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.done {
            return Err(Error::Invalid("AVC bridge setup consumed"));
        }
        if let Some(parser) = self.parser.as_mut() {
            return match parser.step(w, c)? {
                BuildStep::Yield => Ok(None),
                BuildStep::Ready(record) => {
                    self.done = true;
                    Ok(Some(AvcBridge {
                        session: NalPacketSession::new(
                            &self.pool,
                            NalPacketConfiguration::Avc(record),
                            self.packet_limits,
                        )?,
                        source: self.source,
                        track: self.configuration.track.track_id,
                        configuration_revision: self.configuration.revision,
                    }))
                }
                BuildStep::Complete => Err(Error::Invalid("AVC record not retained")),
            };
        }
        match self.children.poll(w)? {
            BoxStep::Yield => Ok(None),
            BoxStep::End => Err(Error::Invalid("AVC sample entry has no avcC")),
            BoxStep::Box(raw) => {
                if raw.header().kind == *b"avcC" {
                    let offset = raw
                        .header()
                        .start
                        .checked_add(u64::from(raw.header().header_size))
                        .ok_or(Error::Overflow)?;
                    let bytes = raw
                        .bytes()
                        .slice(usize::from(raw.header().header_size)..raw.bytes().len())?;
                    self.parser = Some(AvcConfigParser::new(
                        &self.pool,
                        AvcConfigSource {
                            source: self.source,
                            generation: self.configuration.revision,
                            offset,
                        },
                        bytes,
                        self.limits,
                    )?);
                }
                Ok(None)
            }
        }
    }
}
impl AvcBridge {
    pub fn packet(&mut self, packet: Mp4Packet) -> Result<NalPacketJob<'_, Mp4Packet>> {
        if packet.source.source != self.source {
            return Err(Error::SourceMismatch);
        }
        if packet.configuration.track.track_id != self.track
            || packet.configuration.revision != self.configuration_revision
        {
            return Err(Error::Invalid(
                "source configuration changed; configure bridge before packet",
            ));
        }
        let source = NalPacket {
            bytes: packet.source.payload.clone(),
            location: SourceLocation {
                source: packet.source.source,
                offset: packet.source.sample.range.start,
            },
            generation: packet.source.token.generation(),
            fields: packet,
            new_extradata: None,
        };
        self.session.packet(source, None)
    }
    pub fn flush(&mut self) {
        self.session.flush();
    }
    pub fn drain(&mut self) {
        self.session.drain();
    }
}
