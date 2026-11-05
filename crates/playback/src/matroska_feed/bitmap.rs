//! Matroska bitmap subtitle handoff to the existing bounded receivers.
use super::{Association, probe_clock::NativePacketTiming};
use crate::{
    Delivery, Output, RawEvent, Result, TrackKey,
    bitmap_bridge::{BitmapInput, BitmapScope},
    dvb_ts::DvbQueueAction,
};
use triergon::typed::{Event, Packet};
use triergon_core::{Budget, Bytes, Error, SourceLocation, TimeBase};
use triergon_subtitles::{dvb, dvd, pgs::PacketClock};

const NS_PER_SECOND: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatroskaBitmapCodec {
    Pgs,
    VobSub,
    Dvb,
}

pub struct MatroskaBitmapSource {
    pub packet: Packet,
    pub delivery: Delivery,
    pub native_duration_ns: Option<u64>,
}

pub struct MatroskaDvbPacket {
    pub packet: dvb::DvbPacket,
    pub source: MatroskaBitmapSource,
}

impl MatroskaDvbPacket {
    /// Project the event triggered by this packet onto the existing bitmap
    /// queue. A preroll packet can update the decoder without an action.
    pub fn packet_clock_action<'a>(
        &self,
        event: &'a dvb::DisplayEvent,
    ) -> Result<Option<DvbQueueAction<'a>>> {
        if event.generation != self.packet.generation {
            return Err(Error::StaleGeneration {
                expected: self.packet.generation,
                received: event.generation,
            }
            .into());
        }
        let publication = event
            .native_publication
            .ok_or(Error::Unsupported("DVB queue requires native publication"))?;
        if publication.packet_sequence != self.packet.sequence
            || !event.source_packets().iter().any(|packet| {
                packet.sequence == self.packet.sequence
                    && std::ptr::eq(packet.payload.as_ref(), self.packet.payload.as_ref())
            })
        {
            return Err(Error::SourceMismatch.into());
        }
        if matches!(
            self.source.delivery,
            Delivery::DecodeOnly { .. } | Delivery::Unselected
        ) {
            return Ok(None);
        }
        Ok(DvbQueueAction::from_native_publication(event, publication)?)
    }
}

enum Codec {
    Pgs,
    VobSub,
    Dvb(dvb::SourceConfiguration),
}

/// Binds a bitmap decoder to one source track and configuration revision.
pub struct MatroskaBitmapTrack {
    association: Association,
    pool: Budget,
    scope: BitmapScope,
    epoch: u64,
    codec: Codec,
}

impl MatroskaBitmapTrack {
    /// Bind from a current player output, including a packet that restored
    /// configuration after seeking. `track` is the Matroska TrackNumber.
    pub fn new(output: &Output, track: u64) -> Result<Self> {
        let configuration = match output.event() {
            RawEvent::Matroska(Event::Configuration(configuration)) => configuration,
            RawEvent::Matroska(Event::Packet(packet)) => &packet.configuration,
            _ => return Err(Error::Invalid("Matroska bitmap configuration required").into()),
        };
        let (index, descriptor) = configuration
            .tracks()
            .iter()
            .enumerate()
            .find(|(_, descriptor)| descriptor.number == track && descriptor.enabled)
            .ok_or(Error::Invalid("Matroska bitmap track number"))?;
        let codec = match (descriptor.kind, descriptor.codec.as_str()) {
            (17, "S_HDMV/PGS") => Codec::Pgs,
            (17, "S_VOBSUB") => Codec::VobSub,
            (17, "S_DVBSUB") => {
                let raw = configuration
                    .initialization_bytes(index)
                    .ok_or(Error::Invalid(
                        "missing Matroska DVB subtitle initialization",
                    ))?;
                Codec::Dvb(dvb::SourceConfiguration::from_matroska_private(
                    output.pool(),
                    &raw,
                    dvb::SourceClock::new(1, NS_PER_SECOND)?,
                )?)
            }
            _ => return Err(Error::Unsupported("Matroska bitmap subtitle codec").into()),
        };
        Ok(Self {
            association: Association {
                configuration: configuration.clone(),
                track: index,
            },
            pool: output.pool().clone(),
            scope: BitmapScope {
                source: output.source_id(),
                track,
                generation: output.generation(),
                configuration: configuration.revision(),
            },
            epoch: output.epoch(),
            codec,
        })
    }

    pub fn codec(&self) -> MatroskaBitmapCodec {
        match self.codec {
            Codec::Pgs => MatroskaBitmapCodec::Pgs,
            Codec::VobSub => MatroskaBitmapCodec::VobSub,
            Codec::Dvb(_) => MatroskaBitmapCodec::Dvb,
        }
    }

    pub fn scope(&self) -> BitmapScope {
        self.scope
    }

    pub(super) fn pool(&self) -> &Budget {
        &self.pool
    }

    /// Read the effective VobSub palette and canvas under the caller's work
    /// budget. The job owns a shared view of the original configuration.
    pub fn vobsub_configuration(&self, limits: dvd::ConfigLimits) -> Result<dvd::ConfigJob> {
        if !matches!(self.codec, Codec::VobSub) {
            return Err(Error::Invalid("VobSub configuration requires a VobSub track").into());
        }
        let raw = self
            .association
            .configuration
            .initialization_bytes(self.association.track)
            .ok_or(Error::Invalid("missing Matroska VobSub initialization"))?;
        Ok(dvd::ConfigJob::new(raw, &self.pool, limits)?)
    }

    /// The selected service and the original five-byte CodecPrivate. Pass this
    /// configuration to a DVB decoder using the binding's generation.
    pub fn dvb_configuration(&self) -> Option<&dvb::SourceConfiguration> {
        match &self.codec {
            Codec::Dvb(config) => Some(config),
            _ => None,
        }
    }

    /// Bind bare Matroska segments to the DVB receiver and retain container
    /// timing, selection, preroll classification and BlockGroup metadata.
    pub fn dvb_packet(&self, output: &Output, sequence: u64) -> Result<Option<MatroskaDvbPacket>> {
        self.check_dvb()?;
        self.packet(output, sequence)?
            .map(|input| self.bind_dvb(input))
            .transpose()
    }

    pub(super) fn check_dvb(&self) -> Result<&dvb::SourceConfiguration> {
        self.dvb_configuration()
            .ok_or(Error::Invalid("DVB packet requires a DVB track").into())
    }

    pub(super) fn bind_dvb(
        &self,
        input: BitmapInput<MatroskaBitmapSource>,
    ) -> Result<MatroskaDvbPacket> {
        let config = self.check_dvb()?;
        let position = input
            .metadata
            .packet
            .group
            .as_ref()
            .map(|group| group.position())
            .transpose()?
            .flatten();
        let clock = |ticks| dvb::ClockValue {
            ticks,
            origin: dvb::ClockOrigin::Inferred,
        };
        let packet = config.packet(
            &self.pool,
            &input.raw,
            dvb::PacketTiming {
                pts: input.clock.pts.map(i128::from).map(clock),
                dts: input.clock.dts.map(i128::from).map(clock),
                duration: input.metadata.native_duration_ns.map(i128::from).map(clock),
            },
            dvb::PacketIdentity {
                generation: input.scope.generation,
                sequence: input.sequence,
                container_position: position.map(|position| SourceLocation {
                    source: position.source,
                    offset: position.header,
                }),
            },
        )?;
        Ok(MatroskaDvbPacket {
            packet,
            source: input.metadata,
        })
    }

    /// Use the player's delivery check before feeding the receiver. Unselected
    /// tracks are ignored and the original packet retains its exact clocks and
    /// block metadata. Delivery is carried through decode so the caller can
    /// keep preroll state without presenting it. The bitmap queue uses the
    /// Matroska demuxer's compatibility clock before generic stream probing.
    pub fn packet(
        &self,
        output: &Output,
        sequence: u64,
    ) -> Result<Option<BitmapInput<MatroskaBitmapSource>>> {
        let Some(packet) = self.selected_packet(output)? else {
            return Ok(None);
        };
        self.make_input(
            packet,
            output.delivery(),
            sequence,
            NativePacketTiming::demux(packet)?,
        )
        .map(Some)
    }

    pub(super) fn selected_packet<'a>(&self, output: &'a Output) -> Result<Option<&'a Packet>> {
        if output.delivery() == Delivery::Unselected
            || output.event().track() != Some(TrackKey::Matroska(self.scope.track))
        {
            return Ok(None);
        }
        let RawEvent::Matroska(Event::Packet(packet)) = output.event() else {
            return Ok(None);
        };
        if output.source_id() != self.scope.source {
            return Err(Error::SourceMismatch.into());
        }
        if !output.belongs_to(&self.pool) {
            return Err(Error::ForeignPool.into());
        }
        if output.generation() != self.scope.generation {
            return Err(Error::StaleGeneration {
                expected: self.scope.generation,
                received: output.generation(),
            }
            .into());
        }
        if output.epoch() != self.epoch || !self.association.matches(packet) {
            return Err(Error::Invalid("Matroska bitmap configuration changed").into());
        }
        Ok(Some(packet))
    }

    pub(super) fn make_input(
        &self,
        packet: &Packet,
        delivery: Delivery,
        sequence: u64,
        timing: NativePacketTiming,
    ) -> Result<BitmapInput<MatroskaBitmapSource>> {
        let raw = match packet.packet.data.shared_bytes()? {
            Some(raw) => raw,
            None => Bytes::copy_from_slice(&self.pool, &packet.packet.data)?,
        };
        Ok(BitmapInput {
            scope: self.scope,
            sequence,
            raw,
            clock: PacketClock {
                pts: timing.pts,
                dts: timing.dts,
                timebase: TimeBase::new(1, NS_PER_SECOND)?,
            },
            metadata: MatroskaBitmapSource {
                packet: packet.clone(),
                delivery,
                native_duration_ns: timing.duration_ns,
            },
        })
    }
}
