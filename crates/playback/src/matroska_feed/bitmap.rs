//! Matroska bitmap subtitle handoff to the existing bounded receivers.
use super::{Association, probe_clock::NativePacketTiming};
use crate::{
    Delivery, Output, RawEvent, Result, TrackKey,
    bitmap_bridge::{BitmapInput, BitmapScope},
    dvb_ts::{DvbQueueAction, microseconds},
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceLocation, TimeBase, WorkBudget};
use triergon_matroska::typed::{Event, Packet};
use triergon_subtitles::compat::dvb::{Options, ReceiverDecoder, Status};
use triergon_subtitles::{compat, dvb, pgs::PacketClock};

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
}

pub struct MatroskaDvbPacket {
    pub packet: dvb::DvbPacket,
    pub source: MatroskaBitmapSource,
}

impl MatroskaDvbPacket {
    /// Decode the packet with the track's receiver
    /// ([`MatroskaBitmapTrack::dvb_decoder`]) and project its subtitle onto
    /// the bitmap queue. A packet the decoder refuses or that presents
    /// nothing, and a preroll or unselected one, has no action. A packet
    /// refused for memory returns the error: `Error::Backpressure` may be
    /// offered again once the pool has room, `Error::Limit` never fits the
    /// pool and is dropped.
    pub fn decode<'d>(
        &self,
        decoder: &'d mut ReceiverDecoder,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<MatroskaDvbStep<'d>> {
        let facts = self.packet.source.as_ref().ok_or(Error::SourceMismatch)?;
        let pts = facts
            .timing()
            .pts
            .map(|value| microseconds(value.ticks, facts.configuration().clock()))
            .transpose()?;
        // The decoder keeps a packet it yielded, stalled or was cancelled on:
        // this one continues it, and another drops the rest of it.
        let status = if decoder.keeps(&self.packet.payload, pts) {
            decoder.resume(work, cancel)
        } else {
            decoder.abandon();
            decoder.decode(&self.packet.payload, pts, work, cancel)
        };
        let subtitle = match status {
            Ok(Status::Subtitle(subtitle)) => subtitle,
            Ok(Status::Yield) => return Ok(MatroskaDvbStep::Yield),
            Ok(Status::Nothing) | Err(Error::Invalid(_)) => return Ok(MatroskaDvbStep::Decoded),
            Ok(Status::Stalled(error)) | Err(error) => return Err(error),
        };
        if matches!(
            self.source.delivery,
            Delivery::DecodeOnly { .. } | Delivery::Unselected
        ) {
            return Ok(MatroskaDvbStep::Decoded);
        }
        Ok(MatroskaDvbStep::Action(DvbQueueAction::from_subtitle(
            subtitle,
        )))
    }
}

/// What decoding a [`MatroskaDvbPacket`] did.
#[derive(Clone, Copy, Debug)]
pub enum MatroskaDvbStep<'a> {
    /// The packet is decoded and asks this of the bitmap queue.
    Action(DvbQueueAction<'a>),
    /// The packet is decoded with no action.
    Decoded,
    /// The work budget ran out. The decoder keeps the packet: decode the same
    /// packet again to resume it, or drop it with
    /// [`ReceiverDecoder::abandon`]. A memory refusal after the packet was
    /// admitted ([`Status::Stalled`]) is returned as its error and keeps the
    /// packet the same way.
    Yield,
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
            _ => return Err(Error::Invalid("Matroska bitmap configuration required")),
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
            _ => return Err(Error::Unsupported("Matroska bitmap subtitle codec")),
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

    /// The DVD receiver's settings from the track's VobSub `CodecPrivate`:
    /// its palette and canvas, read as the reference's decoder reads them.
    pub fn vobsub_settings(&self) -> Result<compat::dvd::Settings> {
        if !matches!(self.codec, Codec::VobSub) {
            return Err(Error::Invalid("VobSub settings require a VobSub track"));
        }
        let raw = self
            .association
            .configuration
            .initialization_bytes(self.association.track)
            .ok_or(Error::Invalid("missing Matroska VobSub initialization"))?;
        compat::dvd::Settings::from_configuration(&raw)
    }

    /// The selected service and the original five-byte CodecPrivate.
    pub fn dvb_configuration(&self) -> Option<&dvb::SourceConfiguration> {
        match &self.codec {
            Codec::Dvb(config) => Some(config),
            _ => None,
        }
    }

    /// The receiver for this track's [`MatroskaDvbPacket`]s, charged to the
    /// track's pool. Like FFmpeg's decoder it decodes every page.
    pub fn dvb_decoder(&self) -> Result<ReceiverDecoder> {
        let config = self.check_dvb()?;
        Ok(ReceiverDecoder::new(
            &self.pool,
            config.original(),
            Options::default(),
        ))
    }

    /// Bind bare Matroska segments for the DVB receiver and retain container
    /// timing, selection, preroll classification and BlockGroup metadata.
    pub fn dvb_packet(&self, output: &Output, sequence: u64) -> Result<Option<MatroskaDvbPacket>> {
        self.check_dvb()?;
        self.packet(output, sequence)?
            .map(|input| self.bind_dvb(input))
            .transpose()
    }

    pub(super) fn check_dvb(&self) -> Result<&dvb::SourceConfiguration> {
        self.dvb_configuration()
            .ok_or(Error::Invalid("DVB packet requires a DVB track"))
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
            .map(triergon_matroska::typed::BlockGroupSource::position)
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
                duration: input.duration.map(i128::from).map(clock),
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
            return Err(Error::SourceMismatch);
        }
        if !output.belongs_to(&self.pool) {
            return Err(Error::ForeignPool);
        }
        if output.generation() != self.scope.generation {
            return Err(Error::StaleGeneration {
                expected: self.scope.generation,
                received: output.generation(),
            });
        }
        if output.epoch() != self.epoch || !self.association.matches(packet) {
            return Err(Error::Invalid("Matroska bitmap configuration changed"));
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
            duration: timing
                .duration_ns
                .map(i64::try_from)
                .transpose()
                .map_err(|_| Error::Overflow)?,
            metadata: MatroskaBitmapSource {
                packet: packet.clone(),
                delivery,
            },
        })
    }
}
