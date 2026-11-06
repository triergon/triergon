//! Typed AV1 packet delivery with an explicit container configuration binding.
use crate::{Error as PlaybackError, Mp4Packet, Result, mp4_plan::Mp4Configuration};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    Av1ConfigLimits, Av1ConfigParser, Av1Configuration, Av1TemporalUnitConfig, Av1TemporalUnitJob,
    Av1TemporalUnitLimits, BuildStep,
};
use triergon_isobmff::{Access, BoxChildren, BoxStep, EntryLayout, Limits};
use triergon_matroska::typed::{Configuration, Packet as MatroskaPacket};

/// The player or sink keeps the original packet (and its clock) for the lifetime
/// of the receiver view. Bind a new bridge after a source configuration changes.
#[derive(Clone, Copy)]
pub enum Av1Input<'a> {
    Matroska(&'a MatroskaPacket),
    Mp4(&'a Mp4Packet),
}

enum Binding {
    Matroska {
        configuration: triergon_matroska::typed::Configuration,
        track: usize,
    },
    Mp4 {
        source: SourceId,
        track: u32,
        revision: u64,
        entry: Bytes,
    },
}

pub struct Av1BridgeSetup {
    pool: Budget,
    binding: Option<Binding>,
    entry: Option<BoxChildren>,
    record: Option<Bytes>,
    parser: Option<Av1ConfigParser>,
    source: SourceId,
    config_limits: Av1ConfigLimits,
    packet_limits: Av1TemporalUnitLimits,
}

pub struct Av1Bridge {
    binding: Binding,
    configuration: Option<Av1Configuration>,
    limits: Av1TemporalUnitLimits,
}

impl Av1BridgeSetup {
    /// The source identifier is the owning player's input identity.
    pub fn new(
        pool: &Budget,
        source: SourceId,
        packet: Av1Input<'_>,
        config_limits: Av1ConfigLimits,
        packet_limits: Av1TemporalUnitLimits,
    ) -> Result<Self> {
        match packet {
            Av1Input::Matroska(packet) => Self::for_matroska(
                pool,
                source,
                &packet.configuration,
                packet.packet.track,
                config_limits,
                packet_limits,
            ),
            Av1Input::Mp4(packet) => {
                if packet.source.source != source {
                    return Err(Error::SourceMismatch.into());
                }
                Self::for_mp4(
                    pool,
                    source,
                    &packet.configuration,
                    config_limits,
                    packet_limits,
                )
            }
        }
    }

    /// Bind the selected track before the first packet of a Matroska generation.
    pub fn for_matroska(
        pool: &Budget,
        source: SourceId,
        configuration: &Configuration,
        track: usize,
        config_limits: Av1ConfigLimits,
        packet_limits: Av1TemporalUnitLimits,
    ) -> Result<Self> {
        if !configuration.belongs_to(pool) {
            return Err(Error::ForeignPool.into());
        }
        if configuration.tracks().get(track).map(|t| t.codec.as_str()) != Some("V_AV1") {
            return Err(Error::Invalid("source track is not AV1").into());
        }
        let init = configuration
            .initialization(track)
            .ok_or(Error::Invalid("missing Matroska track configuration"))?;
        if init.data.len() > config_limits.maximum_record_bytes {
            return Err(Error::Limit {
                requested: init.data.len(),
                maximum: config_limits.maximum_record_bytes,
            }
            .into());
        }
        Ok(Self {
            pool: pool.clone(),
            binding: Some(Binding::Matroska {
                configuration: configuration.clone(),
                track,
            }),
            entry: None,
            record: configuration.initialization_bytes(track),
            parser: None,
            source,
            config_limits,
            packet_limits,
        })
    }

    /// Bind the checked MP4 sample description to the producer's input identity.
    pub fn for_mp4(
        pool: &Budget,
        source: SourceId,
        configuration: &Mp4Configuration,
        config_limits: Av1ConfigLimits,
        packet_limits: Av1TemporalUnitLimits,
    ) -> Result<Self> {
        if configuration.description.format != *b"av01" {
            return Err(Error::Invalid("source entry is not AV1").into());
        }
        let entry = configuration.description.raw().bytes();
        if !entry.belongs_to(pool) {
            return Err(Error::ForeignPool.into());
        }
        let children = configuration
            .description
            .extensions(EntryLayout::Visual, Limits::default())?;
        Ok(Self {
            pool: pool.clone(),
            binding: Some(Binding::Mp4 {
                source,
                track: configuration.track.track_id,
                revision: configuration.revision,
                entry: entry.clone(),
            }),
            entry: Some(children),
            record: None,
            parser: None,
            source,
            config_limits,
            packet_limits,
        })
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Av1Bridge>> {
        let result = self.advance(work, cancel);
        if let Err(error) = &result
            && !matches!(
                error,
                PlaybackError::Core(Error::Backpressure { .. } | Error::Allocation)
            )
        {
            self.binding = None;
            self.entry = None;
            self.record = None;
            self.parser = None;
        }
        result
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Av1Bridge>> {
        if self.binding.is_none() {
            return Ok(BuildStep::Complete);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled.into());
        }
        if let Some(entry) = &mut self.entry {
            match entry.poll(work)? {
                BoxStep::Yield => return Ok(BuildStep::Yield),
                BoxStep::End => {
                    self.entry = None;
                    if self.record.is_none() {
                        return Err(Error::Invalid("AV1 sample entry has no av1C").into());
                    }
                }
                BoxStep::Box(boxed) => {
                    if boxed.header().kind == *b"av1C" {
                        if self.record.is_some() {
                            return Err(Error::Invalid("duplicate av1C").into());
                        }
                        let bytes = boxed.bytes();
                        self.record = Some(
                            bytes.slice(usize::from(boxed.header().header_size)..bytes.len())?,
                        );
                    }
                    return Ok(BuildStep::Yield);
                }
            }
        }
        if let Some(record) = &self.record {
            self.parser = Some(Av1ConfigParser::new(
                &self.pool,
                self.source,
                record.clone(),
                self.config_limits,
            )?);
            self.record = None;
        }
        let configuration = match &mut self.parser {
            Some(parser) => match parser.step(work, cancel)? {
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Ready(config) => {
                    Av1TemporalUnitConfig::new(&config)?;
                    Some(config)
                }
                BuildStep::Complete => {
                    return Err(Error::Invalid("AV1 configuration already consumed").into());
                }
            },
            None => None,
        };
        self.parser = None;
        Ok(BuildStep::Ready(Av1Bridge {
            binding: self
                .binding
                .take()
                .ok_or(Error::Invalid("AV1 bridge already delivered"))?,
            configuration,
            limits: self.packet_limits,
        }))
    }
}

impl Av1Bridge {
    pub fn packet<'a>(&'a self, packet: Av1Input<'a>) -> Result<Av1TemporalUnitJob<'a>> {
        let (bytes, random_access) = match (&self.binding, packet) {
            (
                Binding::Matroska {
                    configuration,
                    track,
                },
                Av1Input::Matroska(packet),
            ) if packet.packet.track == *track
                && packet.configuration.revision() == configuration.revision()
                && packet.configuration.generation() == configuration.generation()
                && std::ptr::eq(packet.configuration.tracks(), configuration.tracks()) =>
            {
                (packet.packet.data.as_ref(), packet.packet.keyframe)
            }
            (
                Binding::Mp4 {
                    source,
                    track,
                    revision,
                    entry,
                },
                Av1Input::Mp4(packet),
            ) if packet.source.source == *source
                && packet.configuration.track.track_id == *track
                && packet.configuration.revision == *revision
                && std::ptr::eq(
                    packet.configuration.description.raw().bytes().as_ref(),
                    entry.as_ref(),
                ) =>
            {
                (
                    packet.source.payload.as_ref(),
                    packet.source.sample.access == Access::ContainerSync,
                )
            }
            _ => {
                return Err(Error::Invalid(
                    "source configuration changed; configure AV1 bridge before packet",
                )
                .into());
            }
        };
        let configuration = if random_access {
            self.configuration
                .as_ref()
                .map(Av1TemporalUnitConfig::new)
                .transpose()?
        } else {
            None
        };
        Ok(Av1TemporalUnitJob::new(
            bytes,
            configuration,
            random_access,
            self.limits,
        )?)
    }
}
