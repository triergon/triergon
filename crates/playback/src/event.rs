//! Events a player delivers and the engine steps that carry them.

use crate::compat::pes::AdmittedCarriage;
use crate::{Result, SeekIntent, TrackKey, mp4_plan::Mp4Configuration};
use triergon_core::{Error, InputRequest, SourceLocation, Time, TimeBase};
use triergon_isobmff::{
    compat::mov::{front::FrontReadBoundary, packets::PacketOccurrence, recovery::ShortSample},
    payload::SourcePacket,
};
use triergon_matroska::{Event as MkvEvent, ParseStep};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{
    self as ts,
    clock::{ClockKey, SourceTimestamp},
};
use ts::compat::pes::{DemuxPacket, DemuxTimeView};
#[derive(Clone, Debug)]
pub struct Mp4Packet {
    /// Explicit native read boundary, independent of the full Source stream.
    /// Later Source packets remain present with `native == None`.
    pub front_read_boundary: Option<FrontReadBoundary>,
    pub source: SourcePacket,
    pub configuration: Mp4Configuration,
    pub occurrence: Option<PacketOccurrence>,
    /// Sample trim from the associated encoded edit plan, in native skip-samples layout.
    pub encoded_skip_samples: Option<[u8; 10]>,
}

/// An unassociated empty packet in the pinned native receiver view. Its public
/// position is a receiver field, with no corresponding physical sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mp4EmptyPacket {
    pub boundary: FrontReadBoundary,
    pub time_base: TimeBase,
    pub handler: [u8; 4],
    pub pts: i64,
    pub dts: i64,
    pub duration: i64,
    pub position: u64,
    pub flags: u32,
}

/// A native corrupt packet with an actual physical prefix and a separately
/// retained sample declaration. It is never a complete Source packet.
#[derive(Clone, Debug)]
pub struct Mp4ShortPacket {
    pub prefix: ShortSample,
    pub configuration: Mp4Configuration,
    pub occurrence: PacketOccurrence,
    /// Decoder trim attached to the recovered occurrence.
    pub encoded_skip_samples: Option<[u8; 10]>,
}

/// The first native read has a positive declaration starting at or beyond the
/// confirmed physical end. The view ends without emitting a packet.
#[derive(Clone, Debug)]
pub struct Mp4MissingSample {
    pub declared: triergon_isobmff::Sample,
    pub track_id: u32,
    pub end: SourceLocation,
}

#[derive(Clone, Debug)]
pub enum Mp4Event {
    Configuration(Mp4Configuration),
    Packet(Mp4Packet),
    EmptyPacket(Mp4EmptyPacket),
    ShortPacket(Mp4ShortPacket),
    MissingSample(Mp4MissingSample),
}

#[derive(Debug)]
pub struct TsEvent {
    pub source: TraceEvent,
    pub source_pts: Option<SourceTimestamp>,
    pub source_dts: Option<SourceTimestamp>,
    pub demux_clock: Option<DemuxTimeView>,
    pub clock_key: Option<ClockKey>,
}

#[derive(Debug)]
pub enum Event {
    Mp4Root(triergon_isobmff::boxes::BoxView),
    EntityEnd(ts::stream::MediaEntityEnd),
    DemuxPes {
        packet: DemuxPacket,
        clock: DemuxTimeView,
    },
    PesCarriage(AdmittedCarriage),
    PesRefusal {
        track: TrackKey,
        error: Error,
        source_event_ordinal: u64,
    },
    Matroska(MkvEvent),
    Mp4(Mp4Event),
    Ts(TsEvent),
    Seek(SeekIntent),
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one bounded inline event; queue slots and all retained buffers use Budget; a per-event Box would add an untracked allocation"
)]
pub enum EngineStep {
    Event(Event),
    NeedInput(InputRequest),
    Yield,
    End,
    Backpressure { requested: usize, available: usize },
    AwaitEntity,
}

impl EngineStep {
    pub(crate) fn from_matroska(step: ParseStep<MkvEvent>) -> Self {
        match step {
            ParseStep::Event(event) => Self::Event(Event::Matroska(event)),
            ParseStep::NeedInput(request) => Self::NeedInput(request),
            ParseStep::Yield => Self::Yield,
            ParseStep::Backpressure {
                requested,
                available,
            } => Self::Backpressure {
                requested,
                available,
            },
            ParseStep::Finished => Self::End,
        }
    }
}

impl Event {
    pub fn track(&self) -> Option<TrackKey> {
        match self {
            Self::DemuxPes { packet, .. } => Some(TrackKey::Ts {
                program: packet.program,
                pid: packet.pid,
            }),
            Self::Matroska(MkvEvent::Packet(p)) => p
                .configuration
                .tracks()
                .get(p.packet.track)
                .map(|t| TrackKey::Matroska(t.number)),
            Self::Mp4(Mp4Event::Packet(p)) => Some(TrackKey::Mp4(p.configuration.track.track_id)),
            Self::Mp4(Mp4Event::ShortPacket(p)) => Some(TrackKey::Mp4(p.occurrence.track_id)),
            Self::Ts(TsEvent {
                source: TraceEvent::Pes(p),
                clock_key: Some(k),
                ..
            }) => Some(TrackKey::Ts {
                program: k.program,
                pid: p.pid,
            }),
            _ => None,
        }
    }
    pub fn source_pts(&self) -> Result<Option<Time>> {
        match self {
            Self::Matroska(MkvEvent::Packet(p)) => Ok(p.source_pts()?),
            Self::Mp4(Mp4Event::Packet(p)) => {
                let Some(base) = p.configuration.track.base.get() else {
                    return Ok(None);
                };
                Ok(p.source.sample.source_times(base)?.cts)
            }
            Self::Ts(e) => Ok(e.source_pts.and_then(|p| p.exact)),
            _ => Ok(None),
        }
    }
    pub fn demux_pts(&self) -> Result<Option<Time>> {
        match self {
            Self::DemuxPes { clock, .. } => clock.pts90.map(|t| Time::new(t, 90_000)).transpose(),
            Self::Mp4(Mp4Event::Packet(p)) => p
                .occurrence
                .as_ref()
                .map(|n| {
                    n.demux
                        .pts
                        .map(|ticks| Time::from_ticks(i128::from(ticks), n.media_clock.demux_base))
                        .transpose()
                })
                .transpose()
                .map(Option::flatten),
            Self::Mp4(Mp4Event::ShortPacket(p)) => p
                .occurrence
                .demux
                .pts
                .map(|ticks| {
                    Time::from_ticks(i128::from(ticks), p.occurrence.media_clock.demux_base)
                })
                .transpose(),
            Self::Ts(e) => e
                .demux_clock
                .and_then(|n| n.pts90)
                .map(|t| Time::new(t, 90_000))
                .transpose(),
            Self::Matroska(MkvEvent::Packet(p)) => p
                .packet
                .pts_ns
                .map(|t| Time::new(i128::from(t), 1_000_000_000))
                .transpose(),
            _ => Ok(None),
        }
    }
    #[inline]
    pub fn payload_bytes(&self) -> usize {
        match self {
            Self::DemuxPes { packet, .. } => packet.payload_len,
            Self::Matroska(MkvEvent::Packet(p)) => p.packet.data.len(),
            Self::Matroska(MkvEvent::Envelope(triergon_matroska::ElementEvent::Leaf {
                data,
                ..
            })) => data.len(),
            Self::Mp4(Mp4Event::Packet(p)) => p.source.payload.len(),
            Self::Mp4(Mp4Event::ShortPacket(p)) => p.prefix.payload.len(),
            Self::Mp4Root(raw) => raw.bytes().len(),
            Self::Ts(TsEvent {
                source: TraceEvent::Pes(p),
                ..
            }) => p.payload_len,
            _ => 0,
        }
    }
}
