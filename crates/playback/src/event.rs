//! Events a player delivers and the engine steps that carry them.

use crate::compat::pes::AdmittedCarriage;
use crate::{Result, SeekIntent, TrackKey, mp4::plan::Mp4Configuration};
use triergon_core::container::{PacketInfo, RandomAccess, TrackId, Trim};
use triergon_core::{Error, InputRequest, SourceLocation, Time, TimeBase};
use triergon_isobmff::{
    Access,
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
/// One MP4 sample with its configuration and, on the demux view, its occurrence.
#[derive(Clone, Debug)]
pub struct Mp4Packet {
    /// Where a front demux plan stops reading, once reached. Source packets after
    /// it are still delivered, without an `occurrence`.
    pub front_read_boundary: Option<FrontReadBoundary>,
    /// The sample and its payload.
    pub source: SourcePacket,
    /// The configuration of the sample's description.
    pub configuration: Mp4Configuration,
    /// The demux view's occurrence of the sample, with its clocks; `None` on the source
    /// view.
    pub occurrence: Option<PacketOccurrence>,
    /// Decoded samples to discard, as FFmpeg's skip-samples side data: leading
    /// and trailing counts as little-endian `u32` in bytes 0..4 and 4..8.
    /// [`Packet::demux_times`] reads it as a `Trim`.
    pub encoded_skip_samples: Option<[u8; 10]>,
}

/// An empty packet the demux view reports at its front read boundary. No sample
/// of the source corresponds to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mp4EmptyPacket {
    /// The read boundary the packet reports.
    pub boundary: FrontReadBoundary,
    /// The time base of `pts`, `dts` and `duration`.
    pub time_base: TimeBase,
    /// The track's handler type.
    pub handler: [u8; 4],
    /// The presentation time, in ticks of `time_base`.
    pub pts: i64,
    /// The decode time, in ticks of `time_base`.
    pub dts: i64,
    /// The duration, in ticks of `time_base`.
    pub duration: i64,
    /// The byte position the packet reports.
    pub position: u64,
    /// The packet flags the packet reports.
    pub flags: u32,
}

/// The demux view's packet for a sample the source ends inside: the bytes that
/// are present, flagged corrupt, with the sample as its table declares it.
#[derive(Clone, Debug)]
pub struct Mp4ShortPacket {
    /// The bytes present of the truncated sample.
    pub prefix: ShortSample,
    /// The configuration of the sample's description.
    pub configuration: Mp4Configuration,
    /// The demux view's occurrence of the declared sample.
    pub occurrence: PacketOccurrence,
    /// The leading samples a decoder skips, as in [`Mp4Packet::encoded_skip_samples`].
    pub encoded_skip_samples: Option<[u8; 10]>,
}

/// The demux view's end at a first sample that is declared with a size but
/// starts at or beyond the confirmed end of the source. No packet precedes it.
#[derive(Clone, Debug)]
pub struct Mp4MissingSample {
    /// The sample the table declares.
    pub declared: triergon_isobmff::Sample,
    /// The ID of the sample's track.
    pub track_id: u32,
    /// The confirmed end of the source.
    pub end: SourceLocation,
}

/// An event of an MP4 engine.
#[derive(Clone, Debug)]
pub enum Mp4Event {
    /// A track's configuration, before its first packet and at each change.
    Configuration(Mp4Configuration),
    /// A sample.
    Packet(Mp4Packet),
    /// The demux view's empty packet.
    EmptyPacket(Mp4EmptyPacket),
    /// The demux view's truncated packet.
    ShortPacket(Mp4ShortPacket),
    /// The demux view's end at a first sample outside the source.
    MissingSample(Mp4MissingSample),
}

/// A transport stream source event with its clocks.
#[derive(Debug)]
pub struct TsEvent {
    /// The transport stream's own event: a packet, table, PES or fault.
    pub source: TraceEvent,
    /// The PES presentation time, as coded and on the source clock.
    pub source_pts: Option<SourceTimestamp>,
    /// The PES decode time, as coded and on the source clock.
    pub source_dts: Option<SourceTimestamp>,
    /// The PES times on the program's demux clock.
    pub demux_clock: Option<DemuxTimeView>,
    /// The program clock the times belong to.
    pub clock_key: Option<ClockKey>,
}

/// One event of an engine: a packet, a configuration or a source fact.
#[derive(Debug)]
pub enum Event {
    /// A top-level box of a progressive MP4 source.
    Mp4Root(triergon_isobmff::boxes::BoxView),
    /// The end of one entity of a TS entity sequence.
    EntityEnd(ts::stream::MediaEntityEnd),
    /// A demux PES of a selected TS track.
    DemuxPes {
        /// The PES and its payload spans.
        packet: DemuxPacket,
        /// Its times on the program's demux clock.
        clock: DemuxTimeView,
    },
    /// The carriage of a selected TS track, admitted from the active PMT.
    PesCarriage(AdmittedCarriage),
    /// A selected TS track the demux view refuses.
    PesRefusal {
        /// The selected track, which delivers no more demux PES.
        track: TrackKey,
        /// Why it is refused.
        error: Error,
        /// The ordinal of the source event the refusal follows.
        source_event_ordinal: u64,
    },
    /// A Matroska reader event.
    Matroska(MkvEvent),
    /// An MP4 engine event.
    Mp4(Mp4Event),
    /// A transport stream source event.
    Ts(TsEvent),
    /// A seek started: later outputs belong to its generation.
    Seek(SeekIntent),
}

/// One step of an engine.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one bounded inline event; queue slots and all retained buffers use Budget; a per-event Box would add an untracked allocation"
)]
pub enum EngineStep {
    /// The next event, in delivery order.
    Event(Event),
    /// The engine waits on this input request.
    NeedInput(InputRequest),
    /// The work budget ran out.
    Yield,
    /// The source ended and the engine drained.
    End,
    /// The pool is full of output the caller still holds: release some, then
    /// advance again.
    Backpressure {
        /// Bytes the step needed.
        requested: usize,
        /// Bytes the pool had free.
        available: usize,
    },
    /// A TS entity ended: continue with the next entity or finish the logical source.
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
    /// The format-neutral view of the packet this event delivers, when its
    /// payload is one contiguous slice: a Matroska frame or an MP4 sample.
    /// Transport stream PES (spans, not one slice) have none; their access
    /// units come from the readers of `triergon-hls` or `triergon_mpegts::Reader`.
    #[inline]
    pub fn packet(&self) -> Option<Packet<'_>> {
        match self {
            Self::Matroska(MkvEvent::Packet(packet)) => Some(Packet::Matroska(packet)),
            Self::Mp4(Mp4Event::Packet(packet)) => Some(Packet::Mp4(packet)),
            Self::Mp4(Mp4Event::ShortPacket(packet)) => Some(Packet::Mp4Short(packet)),
            _ => None,
        }
    }

    /// The track the event belongs to, if any.
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
    /// The presentation time on the source clock, if the event has one.
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
    /// The presentation time on the demux clock, if the event has one.
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
    /// The payload length in bytes, or 0 for an event without one.
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

/// The format-neutral view of a delivered packet: a borrow of the event's own
/// packet, never a copy. Implements [`PacketInfo`] on the source clock; the demux
/// clock is [`Packet::demux_times`].
#[derive(Clone, Copy, Debug)]
pub enum Packet<'a> {
    /// A Matroska frame.
    Matroska(&'a triergon_matroska::TypedPacket),
    /// An MP4 sample.
    Mp4(&'a Mp4Packet),
    /// The readable prefix of an MP4 sample the source ends inside.
    Mp4Short(&'a Mp4ShortPacket),
}

const _: () = assert!(size_of::<Packet<'_>>() <= 2 * size_of::<usize>());

/// A packet's timestamps on the demux clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemuxTimes {
    /// The length of one tick.
    pub time_base: TimeBase,
    /// Presentation tick; `None` where the demux view has none.
    pub pts: Option<i64>,
    /// Decode tick; `None` where the demux view has none.
    pub dts: Option<i64>,
    /// Duration in ticks; `None` where the demux view has none.
    pub duration: Option<i64>,
    /// Decoded samples to discard, where the demux view carries a trim.
    pub trim: Trim,
}

/// A sample tick as `i64`; `None` when it does not fit.
#[inline]
fn tick(ticks: Option<i128>) -> Option<i64> {
    ticks.and_then(|ticks| i64::try_from(ticks).ok())
}

/// The leading and trailing trim of an encoded skip-samples record: bytes 0..4
/// and 4..8, little endian.
#[inline]
fn skip_samples_trim(record: Option<[u8; 10]>) -> Trim {
    let Some(record) = record else {
        return Trim::default();
    };
    let word = |at: usize| {
        let value =
            u32::from_le_bytes([record[at], record[at + 1], record[at + 2], record[at + 3]]);
        (value != 0).then_some(i64::from(value))
    };
    Trim {
        start: word(0),
        end: word(4),
    }
}

impl Packet<'_> {
    /// The packet's timestamps on the demux clock; `None` for an MP4 packet of a
    /// plan indexed in the source view, which has no demux occurrence.
    pub fn demux_times(&self) -> Option<DemuxTimes> {
        match *self {
            Self::Matroska(packet) => Some(DemuxTimes {
                time_base: packet.time_base(),
                pts: packet.pts(),
                dts: packet.dts(),
                duration: packet.duration(),
                trim: packet.trim(),
            }),
            Self::Mp4(packet) => packet.occurrence.as_ref().map(|occurrence| DemuxTimes {
                time_base: occurrence.media_clock.demux_base,
                pts: occurrence.demux.pts,
                dts: occurrence.demux.dts,
                duration: Some(occurrence.demux.duration),
                trim: skip_samples_trim(packet.encoded_skip_samples),
            }),
            Self::Mp4Short(packet) => Some(DemuxTimes {
                time_base: packet.occurrence.media_clock.demux_base,
                pts: packet.occurrence.demux.pts,
                dts: packet.occurrence.demux.dts,
                duration: Some(packet.occurrence.demux.duration),
                trim: skip_samples_trim(packet.encoded_skip_samples),
            }),
        }
    }

    /// The source time base of an MP4 packet's track; `None` for a track whose
    /// media timescale is zero.
    #[inline]
    fn mp4_base(&self) -> Option<TimeBase> {
        match self {
            Self::Matroska(_) => None,
            Self::Mp4(packet) => packet.configuration.track.base.get(),
            Self::Mp4Short(packet) => packet.configuration.track.base.get(),
        }
    }
}

/// Source timestamps of MP4 packets are ticks of the track's media timescale.
/// A track with a zero timescale has no source clock: its packets report no
/// timestamps, in the demux base (one second per tick where FFmpeg uses that).
impl PacketInfo for Packet<'_> {
    #[inline]
    fn track(&self) -> TrackId {
        match self {
            Self::Matroska(packet) => PacketInfo::track(*packet),
            Self::Mp4(packet) => TrackId(u64::from(packet.configuration.track.track_id)),
            Self::Mp4Short(packet) => TrackId(u64::from(packet.occurrence.track_id)),
        }
    }
    #[inline]
    fn payload(&self) -> &[u8] {
        match self {
            Self::Matroska(packet) => packet.packet.data.as_ref(),
            Self::Mp4(packet) => packet.source.payload.as_ref(),
            Self::Mp4Short(packet) => packet.prefix.payload.as_ref(),
        }
    }
    #[inline]
    fn time_base(&self) -> TimeBase {
        match self {
            Self::Matroska(packet) => packet.time_base(),
            Self::Mp4(packet) => self.mp4_base().unwrap_or_else(|| {
                packet
                    .occurrence
                    .as_ref()
                    .map_or(TimeBase::MICROSECONDS, |o| o.media_clock.demux_base)
            }),
            Self::Mp4Short(packet) => self
                .mp4_base()
                .unwrap_or(packet.occurrence.media_clock.demux_base),
        }
    }
    #[inline]
    fn pts(&self) -> Option<i64> {
        match self {
            Self::Matroska(packet) => packet.pts(),
            Self::Mp4(packet) => self.mp4_base().and(tick(packet.source.sample.cts_ticks)),
            Self::Mp4Short(packet) => self.mp4_base().and(tick(packet.prefix.declared.cts_ticks)),
        }
    }
    #[inline]
    fn dts(&self) -> Option<i64> {
        match self {
            Self::Matroska(packet) => packet.dts(),
            Self::Mp4(packet) => self.mp4_base().and(tick(packet.source.sample.dts_ticks)),
            Self::Mp4Short(packet) => self.mp4_base().and(tick(packet.prefix.declared.dts_ticks)),
        }
    }
    #[inline]
    fn duration(&self) -> Option<i64> {
        match self {
            Self::Matroska(packet) => packet.duration(),
            Self::Mp4(packet) => {
                self.mp4_base()?;
                Some(i64::from(packet.source.sample.duration_ticks))
            }
            Self::Mp4Short(_) => None,
        }
    }
    #[inline]
    fn random_access(&self) -> RandomAccess {
        match self {
            Self::Matroska(packet) => packet.random_access(),
            Self::Mp4(packet) => match packet.source.sample.access {
                Access::ContainerSync => RandomAccess::Sync,
                Access::NonSync => RandomAccess::NotSync,
                Access::Unknown => RandomAccess::Unknown,
            },
            Self::Mp4Short(_) => RandomAccess::Unknown,
        }
    }
    #[inline]
    fn trim(&self) -> Trim {
        match self {
            Self::Matroska(packet) => packet.trim(),
            Self::Mp4(_) | Self::Mp4Short(_) => Trim::default(),
        }
    }
}
