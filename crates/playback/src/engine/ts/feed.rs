//! The steps of a PES feed and the admission facts shared by owned and
//! borrowed-range PES delivery.

use triergon_core::{Budget, BudgetVec, SourceLocation};
use triergon_mpegts::pes::PesHeader;
use triergon_mpegts::{
    SourceEpochs, SourceRange,
    compat::pes::{
        CompactDemuxPacket, CompactPayload, DemuxPacket, DemuxPayloadKind, DemuxProfile, DemuxSpan,
    },
};

/// What a reader checks of a PES before it takes the payload, the same for
/// owned and compact payloads.
#[derive(Debug)]
pub struct PesFacts<'a> {
    /// The program number of the PMT that declared the stream.
    pub program: u16,
    /// The PID the PES was carried on.
    pub pid: u16,
    /// The observer profile that cut the PES.
    pub profile: DemuxProfile,
    /// Where the PES started in the source.
    pub source_origin: SourceLocation,
    /// The source's clock and configuration epochs at the PES.
    pub source_epochs: SourceEpochs,
    /// Demux packet flags.
    pub flags: u32,
    /// The PES header's `stream_id` byte, such as 0xe0 for video.
    pub stream_id: u8,
    /// How the payload is carried.
    pub payload_kind: DemuxPayloadKind,
    /// The PES header as the source carries it.
    pub source_header: &'a PesHeader,
    /// Whether the header's timestamps apply to this packet.
    pub time_header_applies: bool,
    /// Payload length in bytes.
    pub payload_len: usize,
    /// Whether the payload is empty.
    pub empty: bool,
    /// The pool the payload's storage is charged to.
    pub payload_budget: &'a Budget,
    /// The pool the header range list is charged to.
    pub header_budget: &'a Budget,
}

impl PesFacts<'_> {
    /// The header whose timestamps apply, if any.
    pub fn time_header(&self) -> Option<&PesHeader> {
        self.time_header_applies.then_some(self.source_header)
    }
}

/// A completed PES as a reader takes it: owned spans or a compact payload.
pub trait PesInput {
    /// What the reader checks before taking the payload.
    fn facts(&self) -> PesFacts<'_>;
    /// The payload, and the header source ranges a reader does not read.
    fn into_parts(self) -> (CompactPayload, BudgetVec<SourceRange>);
}

/// The lists of the PES a reader consumed last, kept until the reader's owner
/// hands them back to the observer that emitted them.
#[derive(Debug, Default)]
pub struct SpentLists {
    /// The payload span list.
    pub spans: Option<BudgetVec<DemuxSpan>>,
    /// The header source range list.
    pub header: Option<BudgetVec<SourceRange>>,
}

impl PesInput for DemuxPacket {
    fn facts(&self) -> PesFacts<'_> {
        PesFacts {
            program: self.program,
            pid: self.pid,
            profile: self.profile,
            source_origin: self.source_origin,
            source_epochs: self.source_epochs,
            flags: self.flags,
            stream_id: self.stream_id,
            payload_kind: self.payload_kind,
            source_header: &self.source_header,
            time_header_applies: self.time_header_applies,
            payload_len: self.payload_len,
            empty: self.spans.is_empty(),
            payload_budget: self.spans.budget(),
            header_budget: self.header_source_ranges.budget(),
        }
    }
    fn into_parts(self) -> (CompactPayload, BudgetVec<SourceRange>) {
        (
            CompactPayload::from_spans(self.spans),
            self.header_source_ranges,
        )
    }
}

impl PesInput for CompactDemuxPacket {
    fn facts(&self) -> PesFacts<'_> {
        PesFacts {
            program: self.program,
            pid: self.pid,
            profile: self.profile,
            source_origin: self.source_origin,
            source_epochs: self.source_epochs,
            flags: self.flags,
            stream_id: self.stream_id,
            payload_kind: self.payload_kind,
            source_header: &self.source_header,
            time_header_applies: self.time_header_applies,
            payload_len: self.payload_len,
            empty: self.payload.is_empty(),
            payload_budget: self.payload.budget(),
            header_budget: self.header_source_ranges.budget(),
        }
    }
    fn into_parts(self) -> (CompactPayload, BudgetVec<SourceRange>) {
        (self.payload, self.header_source_ranges)
    }
}

use crate::compat::pes::AdmittedCarriage;
use crate::event::{EngineStep, Event, TsEvent};
use crate::{Result, TrackKey};
use triergon_core::{Error, InputRequest};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{self as ts, compat::pes::DemuxTimeView};

/// A completed PES of a feed, with its payload as the observer delivers it.
#[derive(Debug)]
pub enum FeedPacket {
    /// Payload spans of the shared input windows.
    Owned(DemuxPacket),
    /// One compact payload per carrier run.
    Compact(CompactDemuxPacket),
}

/// One step of a PES feed.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "return original transport events without a heap allocation"
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
    /// Whether only progressive feeds poll through this projection, so that
    /// their polling holds no code of the other engines.
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
    // `poll_progressive_payload` admits progressive engines alone.
    const PROGRESSIVE: bool = true;

    #[inline]
    fn project(step: Self) -> Result<Self> {
        Ok(step)
    }
}

pub(crate) fn map_payload<T, U>(
    step: ts::AssemblyStep<T>,
    map: impl FnOnce(T) -> U,
) -> ts::AssemblyStep<U> {
    match step {
        ts::AssemblyStep::Output(packet) => ts::AssemblyStep::Output(map(packet)),
        ts::AssemblyStep::Yield => ts::AssemblyStep::Yield,
        ts::AssemblyStep::NeedPayload => ts::AssemblyStep::NeedPayload,
        ts::AssemblyStep::Finished => ts::AssemblyStep::Finished,
    }
}
