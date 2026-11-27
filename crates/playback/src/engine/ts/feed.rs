//! Project the steps a TS engine reports to its consumer.

use crate::compat::pes::AdmittedCarriage;
use crate::event::{EngineStep, Event, TsEvent};
use crate::{Result, TrackKey};
use triergon_core::{Error, InputRequest};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{
    self as ts,
    compat::pes::{DemuxTimeView, FeedPacket},
};

/// One step of a PES feed.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "a source event is returned whole; boxing it would allocate per event"
)]
pub enum FeedStep {
    /// A source table, clock or fault event the feed delivers whole.
    Event(TsEvent),
    /// The carriage a track was admitted with under the active PMT.
    Configuration(AdmittedCarriage),
    /// A track stopped: its carriage or one of its packets was refused.
    Refusal {
        /// The program and PID of the refused track.
        track: TrackKey,
        /// Why it was refused.
        error: Error,
        /// Ordinal of the source event the refusal is attributed to.
        source_event_ordinal: u64,
    },
    /// A completed PES with its demux clock.
    Packet {
        /// The PES, with an owned or a compact payload as the feed was made.
        packet: FeedPacket,
        /// Its timestamps on the demux clock.
        clock: DemuxTimeView,
    },
    /// The current entity ended.
    EntityEnd(ts::stream::MediaEntityEnd),
    /// The feed needs this input before it can continue.
    NeedInput(InputRequest),
    /// The work budget ran out or the feed returns to its caller.
    Yield,
    /// The source ended and every track drained.
    End,
    /// An entity ended; the caller continues with the next one.
    AwaitEntity,
}

impl FeedStep {
    /// Payload bytes the step retains; `None` for steps without an event.
    pub fn payload_bytes(&self) -> Option<usize> {
        Some(match self {
            Self::Packet { packet, .. } => match packet {
                FeedPacket::Owned(packet) => packet.payload_len,
                FeedPacket::Compact(packet) => packet.payload_len,
            },
            Self::Event(TsEvent {
                source: TraceEvent::Pes(packet),
                ..
            }) => packet.payload_len,
            Self::Event(_) | Self::Configuration(_) | Self::Refusal { .. } | Self::EntityEnd(_) => {
                0
            }
            _ => return None,
        })
    }
}

// Each poller keeps the return layout of its consumer.
pub(crate) trait TsStepProjection: Sized {
    /// Whether this is the projection of a
    /// [`ProgressiveFeed`](crate::pes_feed::ProgressiveFeed), the one poller of a
    /// progressive engine.
    const PROGRESSIVE: bool;

    fn project(step: FeedStep) -> Result<Self>;
}

impl TsStepProjection for EngineStep {
    const PROGRESSIVE: bool = false;

    #[inline(always)]
    fn project(step: FeedStep) -> Result<Self> {
        Ok(match step {
            FeedStep::Event(event) => Self::Event(Event::Ts(event)),
            FeedStep::Configuration(config) => Self::Event(Event::PesCarriage(config)),
            FeedStep::Refusal {
                track,
                error,
                source_event_ordinal,
            } => Self::Event(Event::PesRefusal {
                track,
                error,
                source_event_ordinal,
            }),
            FeedStep::Packet {
                packet: FeedPacket::Owned(packet),
                clock,
            } => Self::Event(Event::DemuxPes { packet, clock }),
            FeedStep::Packet {
                packet: FeedPacket::Compact(_),
                ..
            } => return Err(Error::Invalid("compact TS payload polling required")),
            FeedStep::EntityEnd(end) => Self::Event(Event::EntityEnd(end)),
            FeedStep::NeedInput(request) => Self::NeedInput(request),
            FeedStep::Yield => Self::Yield,
            FeedStep::End => Self::End,
            FeedStep::AwaitEntity => Self::AwaitEntity,
        })
    }
}

impl TsStepProjection for FeedStep {
    // Only a `ProgressiveFeed` polls through this projection.
    const PROGRESSIVE: bool = true;

    #[inline]
    fn project(step: Self) -> Result<Self> {
        Ok(step)
    }
}
