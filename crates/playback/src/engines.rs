use crate::compat::pes::{AdmittedCarriage, Carriage, PidSelection};
use crate::input::SourceReply;
use crate::{
    Format, Result, SeekIntent, TrackKey, TrackKind,
    io::provide,
    mp4_native_replay::{DemuxReplay, DemuxReplayStep},
    mp4_open::PendingMedia,
    mp4_plan::{Mp4Configuration, PreparedMp4, SourceRow},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{MediaKind, TrackInfo};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputRequest, SourceCapabilities, SourceId,
    SourceLocation, Step, Time, TimeBase, WorkBudget,
};
use triergon_isobmff::{
    CursorStatus,
    compat::mov::{
        front::FrontReadBoundary,
        packets::{PacketCursor, PacketOccurrence},
        recovery::{RecoverySampleExtent, RecoverySampleRead, RunRecoveryProfile, ShortSample},
    },
    payload::{SampleReader, SourcePacket},
};
use triergon_matroska::{Event as MkvEvent, ParseStep, Reader};
use triergon_mpegts::stream::{FeedContinuation, FeedItem, TraceEvent, TransportStream};
use triergon_mpegts::{
    self as ts,
    clock::{ClockKey, SourceTimePolicy, SourceTimestamp, source_timestamp},
};
use ts::compat::pes::{
    CompactDemuxPacket, DemuxClock, DemuxObserver, DemuxPacket, DemuxProfile, DemuxScope,
    DemuxTimeView, DvbSubtitlesDescriptors, PrivateAudioCarriage, PrivateAudioDescriptors,
    ReadyContinuation, RegisteredCarriage, SourceContext, selected_track_kind,
};
use ts::psi::{DescriptorNamespace, ProgramMap};
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct TrackFact {
    pub key: TrackKey,
    pub kind: TrackKind,
    pub enabled: bool,
}

fn track_kind(kind: MediaKind) -> TrackKind {
    match kind {
        MediaKind::Video => TrackKind::Video,
        MediaKind::Audio => TrackKind::Audio,
        MediaKind::Subtitle => TrackKind::Subtitle,
        MediaKind::Data | MediaKind::Unknown => TrackKind::Other,
    }
}

#[derive(Clone, Copy, Debug)]
struct CachedDescription {
    track: u32,
    description: u32,
    revision: u64,
}
#[derive(Clone, Copy)]
pub(crate) struct Mp4SyncLanding {
    pub(crate) first_row: usize,
    pub(crate) sync_row: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum Mp4RangeLanding<'a> {
    Source(Mp4SyncLanding),
    Demux(&'a DemuxReplay),
    DemuxCursor(u64),
}

pub struct Mp4Engine {
    progressive: Option<crate::progressive::ProgressiveState>,
    media_cache_fill: Option<PendingMedia>,
    front_boundary: Option<FrontReadBoundary>,
    empty_run: Option<(Mp4EmptyPacket, u32)>,
    ended_short: bool,
    pub(crate) input: InputCursor,
    pub plan: PreparedMp4,
    pub(crate) row: usize,
    indexed_sync: Option<(u32, usize)>,
    demux_seek: Option<DemuxReplay>,
    packet: Option<(usize, SampleReader)>,
    configs: [Option<CachedDescription>; 64],
    revision: u64,
    pub(crate) maximum_packet: usize,
}

enum TrackUpdate {
    Configuration(AdmittedCarriage),
    Refusal(Error),
}

struct PesTrack {
    selection: PidSelection,
    observer: Option<DemuxObserver>,
    update: Option<TrackUpdate>,
}
impl PesTrack {
    fn new(selection: PidSelection) -> Self {
        Self {
            selection,
            observer: None,
            update: None,
        }
    }

    fn pid(&self) -> u16 {
        self.selection.pid
    }

    fn refuse(&mut self, error: Error) {
        self.observer = None;
        self.update = Some(TrackUpdate::Refusal(error));
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "legacy single-track playback has an inline observer and requires no extra allocation"
)]
enum PesTracks {
    Inline(Option<PesTrack>),
    Multiple(BudgetVec<PesTrack>),
}
impl PesTracks {
    fn as_slice(&self) -> &[PesTrack] {
        match self {
            Self::Inline(track) => track.as_slice(),
            Self::Multiple(tracks) => tracks.as_slice(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [PesTrack] {
        match self {
            Self::Inline(track) => track.as_mut_slice(),
            Self::Multiple(tracks) => tracks.as_mut_slice(),
        }
    }

    fn index(&self, pid: u16) -> Option<usize> {
        self.as_slice()
            .binary_search_by_key(&pid, PesTrack::pid)
            .ok()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SegmentDiscovery {
    Native,
    Progressive,
    CompactProgressive,
    AvcAacFeed,
    MediaFeed,
}

pub struct TsEngine {
    native_context: Option<ts::compat::input::InputContext>,
    native_prefix_failed: bool,
    input: InputCursor,
    stream: TransportStream,
    format: ts::PacketFormat,
    program: u16,
    limits: ts::ParseLimits,
    policy: SourceTimePolicy,
    namespace: DescriptorNamespace,
    source_pes_clock: Option<DemuxClock>,
    tsid: Option<u16>,
    pcr_pid: Option<u16>,
    facts: BudgetVec<TrackFact>,
    pending: Option<TraceEvent>,
    ended: bool,
    draining: bool,
    pes_tracks: PesTracks,
    pes_active: Option<usize>,
    pes_adopting: usize,
    pes_pending: Option<FeedPacket>,
    pes_clock: Option<DemuxClock>,
    event_ordinal: u64,
    suppressed_events: u64,
    pes_updates_from: Option<usize>,
    segment_discovery: Option<SegmentDiscovery>,
    segmented: bool,
    boundary: Option<ts::stream::MediaEntityEnd>,
}

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
        /// The refused track.
        track: TrackKey,
        /// Why it was refused.
        error: Error,
        /// Ordinal of the source event the refusal is attributed to.
        source_event_ordinal: u64,
    },
    /// A completed PES with its demux clock.
    Packet {
        /// The PES.
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
trait TsStepProjection: Sized {
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

fn map_payload<T, U>(step: ts::AssemblyStep<T>, map: impl FnOnce(T) -> U) -> ts::AssemblyStep<U> {
    match step {
        ts::AssemblyStep::Output(packet) => ts::AssemblyStep::Output(map(packet)),
        ts::AssemblyStep::Yield => ts::AssemblyStep::Yield,
        ts::AssemblyStep::NeedPayload => ts::AssemblyStep::NeedPayload,
        ts::AssemblyStep::Finished => ts::AssemblyStep::Finished,
    }
}
pub struct MkvEngine {
    reader: Reader,
    source: SourceId,
}
impl MkvEngine {
    pub(crate) fn step(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<ParseStep<MkvEvent>> {
        self.reader.step_parse(w, c)
    }
}
impl std::ops::Deref for MkvEngine {
    type Target = Reader;
    fn deref(&self) -> &Reader {
        &self.reader
    }
}
#[expect(
    clippy::large_enum_variant,
    reason = "bounded inline format state; no uncharged boxed parser"
)]
pub enum Engine {
    Matroska(MkvEngine),
    Mp4(Mp4Engine),
    Ts(TsEngine),
}
impl Engine {
    /// Hands a consumed PES's lists back to the observer of `pid`, when this
    /// is a transport stream engine.
    pub(crate) fn reuse_pes_lists(&mut self, pid: u16, lists: crate::ts_payload::SpentLists) {
        if let Self::Ts(engine) = self {
            engine.reuse_pes_lists(pid, lists);
        }
    }
    /// Play an unstarted Matroska reader configured by the caller, for example
    /// with `Reader::new`, `Reader::new_with_payload_ownership`,
    /// `Reader::new_with_immutable_seek_index` or
    /// `compat::block_additions::new_reader`. A player refuses the engine if
    /// the reader has already read or repositioned its input.
    pub fn matroska(reader: Reader) -> Self {
        let source = reader.source();
        Self::Matroska(MkvEngine { reader, source })
    }
    pub fn mp4(input: InputCursor, plan: PreparedMp4, maximum_packet: usize) -> Result<Self> {
        let engine = Self::mp4_unattached(input, plan, maximum_packet)?;
        if matches!(&engine, Self::Mp4(mp4) if mp4.plan.demux_front) {
            return Err(Error::Unsupported(
                "native front plan requires actual source completion",
            ));
        }
        if matches!(&engine, Self::Mp4(mp4) if mp4.plan.terminal_mdat.is_some()) {
            return Err(Error::Unsupported(
                "open terminal MDAT needs progressive source completion",
            ));
        }
        if matches!(&engine, Self::Mp4(mp4) if mp4.plan.forward_regular_extents.is_some()) {
            return Err(Error::Unsupported(
                "forward regular MDAT needs progressive source completion",
            ));
        }
        Ok(engine)
    }
    fn mp4_unattached(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
    ) -> Result<Self> {
        if !input.budget().same_pool(&plan.pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != plan.source {
            return Err(Error::SourceMismatch);
        }
        if input.generation() != plan.opening_generation {
            return Err(Error::StaleGeneration {
                expected: plan.opening_generation,
                received: input.generation(),
            });
        }
        if input.capabilities().length != plan.source_length {
            return Err(Error::Invalid("plan source extent changed"));
        }
        if maximum_packet == 0 {
            return Err(Error::Invalid("zero packet policy"));
        }
        let mut engine = Mp4Engine {
            progressive: None,
            media_cache_fill: None,
            front_boundary: None,
            empty_run: None,
            ended_short: false,
            input,
            plan,
            row: 0,
            indexed_sync: None,
            demux_seek: None,
            packet: None,
            configs: [None; 64],
            revision: 0,
            maximum_packet,
        };
        if engine.plan.demux_order_replay {
            engine.media_cache_fill = Some(crate::progressive::prepare_media_replay(
                &engine.input,
                &mut engine.plan,
            )?);
        }
        Ok(Self::Mp4(engine))
    }
    pub fn mp4_progressive(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
        limits: crate::progressive::ProgressiveLimits,
    ) -> Result<Self> {
        if plan.demux_front {
            return Err(Error::Unsupported(
                "native front plan needs explicit native attachment",
            ));
        }
        Self::mp4_with_progressive(input, plan, maximum_packet, limits)
    }
    /// Complete one actual front-metadata source using the explicit nonseekable
    /// demux view. A terminal MDAT finishes only on a matching Source End.
    pub(crate) fn mp4_front(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
        limits: crate::progressive::ProgressiveLimits,
    ) -> Result<Self> {
        if !plan.demux_front || input.capabilities().random_access {
            return Err(Error::Invalid("native front attachment context"));
        }
        Self::mp4_with_progressive(input, plan, maximum_packet, limits)
    }
    fn mp4_with_progressive(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
        limits: crate::progressive::ProgressiveLimits,
    ) -> Result<Self> {
        let state = crate::progressive::ProgressiveState::new(&plan, limits)?;
        let mut engine = Self::mp4_unattached(input, plan, maximum_packet)?;
        if let Self::Mp4(e) = &mut engine {
            e.progressive = Some(state);
        }
        Ok(engine)
    }
    /// Source events of one program of a transport stream: carriers, tables,
    /// PES with their source clocks, and faults. The source's end ends the stream.
    pub fn ts(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
    ) -> Result<Self> {
        TsEngine::program(input, format, program, limits, policy, namespace).map(Self::Ts)
    }

    /// [`Self::ts`] over a sequence of entities: each entity's end is reported
    /// as [`Event::EntityEnd`], and the caller continues with the next one or
    /// finishes the logical source.
    pub fn ts_entities(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
    ) -> Result<Self> {
        let engine = TsEngine::program(input, format, program, limits, policy, namespace)?;
        Ok(Self::Ts(TsEngine {
            segmented: true,
            ..engine
        }))
    }

    pub fn delivers_demux_pes(&self) -> bool {
        matches!(self, Self::Ts(t) if t.segment_discovery.is_some() || !t.pes_tracks.as_slice().is_empty())
    }
    pub(crate) fn assembles_pid(&self, pid: u16) -> bool {
        matches!(self, Self::Ts(t) if t.pes_tracks.index(pid).is_some())
    }
    pub(crate) fn ts_hls_segment(
        input: InputCursor,
        limits: ts::ParseLimits,
        media: bool,
    ) -> Result<Self> {
        TsEngine::segment(
            input,
            limits,
            SourceTimePolicy::Unanchored,
            DescriptorNamespace::Unspecified,
            if media {
                SegmentDiscovery::MediaFeed
            } else {
                SegmentDiscovery::AvcAacFeed
            },
        )
        .map(Self::Ts)
    }

    pub(crate) fn continue_entity(
        &mut self,
        source: SourceId,
        caps: SourceCapabilities,
        discontinuity: bool,
    ) -> Result<()> {
        let Self::Ts(t) = self else {
            return Err(Error::Invalid("entity continuation requires TS source"));
        };
        let end = t
            .boundary
            .as_ref()
            .ok_or(Error::Invalid("actual entity End required"))?;
        if source == end.location().source {
            return Err(Error::SourceMismatch);
        }
        if discontinuity {
            t.stream.discontinuity()?;
        }
        t.input.continue_source(source, caps)?;
        end.validate_continuation(&t.input)?;
        t.boundary = None;
        Ok(())
    }
    pub(crate) fn finish_logical_source(&mut self) -> Result<()> {
        let Self::Ts(t) = self else {
            return Err(Error::Invalid("logical TS End required"));
        };
        if t.boundary.is_none() {
            return Err(Error::Invalid("actual source entity End required"));
        }
        t.stream.end_of_source()?;
        t.boundary = None;
        t.draining = true;
        Ok(())
    }
    pub fn format(&self) -> Format {
        match self {
            Self::Matroska(_) => Format::Matroska,
            Self::Mp4(_) => Format::Mp4,
            Self::Ts(t) => Format::Ts(t.format),
        }
    }
    pub(crate) fn validate_binding(&self) -> Result<()> {
        if let Self::Matroska(m) = self
            && !m.reader.is_unstarted()
        {
            return Err(Error::Invalid("Matroska engine needs an unstarted reader"));
        }
        if let Self::Mp4(r) = self {
            if !r.input.budget().same_pool(&r.plan.pool) {
                return Err(Error::ForeignPool);
            }
            if r.input.source() != r.plan.source {
                return Err(Error::SourceMismatch);
            }
            if r.input.generation() != r.plan.opening_generation {
                return Err(Error::StaleGeneration {
                    expected: r.plan.opening_generation,
                    received: r.input.generation(),
                });
            }
            if r.input.capabilities().length != r.plan.source_length {
                return Err(Error::Invalid("plan source extent changed"));
            }
        }
        Ok(())
    }
    pub fn source(&self) -> SourceId {
        match self {
            Self::Matroska(m) => m.source,
            Self::Mp4(m) => m.input.source(),
            Self::Ts(t) => t.input.source(),
        }
    }
    pub fn generation(&self) -> u64 {
        match self {
            Self::Matroska(r) => r.generation(),
            Self::Mp4(r) => r.input.generation(),
            Self::Ts(r) => r.input.generation(),
        }
    }
    pub fn pool(&self) -> &Budget {
        match self {
            Self::Matroska(r) => r.budget(),
            Self::Mp4(r) => r.input.budget(),
            Self::Ts(r) => r.input.budget(),
        }
    }
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        match self {
            Self::Mp4(r) => provide(&mut r.input, reply),
            Self::Ts(r) => r.provide(reply),
            Self::Matroska(r) => match reply {
                SourceReply::Pending => Ok(false),
                SourceReply::Data(w) => {
                    r.reader.provide(w)?;
                    Ok(!w.bytes().is_empty())
                }
                SourceReply::End { source, token } => {
                    r.reader.finish(*source, token.clone())?;
                    Ok(true)
                }
                SourceReply::Failed {
                    source,
                    token,
                    failure,
                } => {
                    r.reader.fail(*source, token.clone(), *failure)?;
                    Err(match failure {
                        triergon_core::SourceFailure::Transport => Error::Transport,
                        triergon_core::SourceFailure::Cancelled => Error::Cancelled,
                    })
                }
            },
        }
    }
    pub fn poll(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<EngineStep> {
        match self {
            Self::Matroska(r) => Ok(EngineStep::from_matroska(r.step(w, c)?)),
            Self::Mp4(r) => r.poll(w, c, None),
            Self::Ts(r) => {
                if r.segment_discovery == Some(SegmentDiscovery::CompactProgressive) {
                    return Err(Error::Invalid("compact TS payload polling required"));
                }
                r.poll::<EngineStep>(w, c)
            }
        }
    }

    pub(crate) fn take_suppressed_events(&mut self) -> u64 {
        match self {
            Self::Ts(engine) => engine.take_suppressed_events(),
            _ => 0,
        }
    }

    pub(crate) fn poll_mp4_range(
        &mut self,
        cursor: &mut PacketCursor<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EngineStep> {
        match self {
            Self::Mp4(mp4) => mp4.poll(work, cancel, Some(cursor)),
            _ => Err(Error::Invalid("native range cursor requires MP4")),
        }
    }
    pub(crate) fn facts(&self) -> Result<BudgetVec<TrackFact>> {
        let mut out = BudgetVec::new(self.pool());
        match self {
            Self::Matroska(r) => {
                if r.tracks().len() > 64 {
                    return Err(Error::Limit {
                        requested: r.tracks().len(),
                        maximum: 64,
                    });
                }
                for t in r.tracks() {
                    out.try_push(TrackFact {
                        key: TrackKey::Matroska(t.number),
                        kind: track_kind(t.kind()),
                        enabled: t.enabled,
                    })
                    .map_err(|e| e.error)?;
                }
            }
            Self::Mp4(r) => {
                for t in r.plan.tracks.as_slice() {
                    let h = &t.snapshot;
                    out.try_push(TrackFact {
                        key: TrackKey::Mp4(h.track_id),
                        kind: track_kind(triergon_isobmff::media_kind(&h.handler)),
                        enabled: h.enabled,
                    })
                    .map_err(|e| e.error)?;
                }
            }
            Self::Ts(r) => {
                out.try_extend_from_slice(r.facts.as_slice())?;
            }
        }
        Ok(out)
    }
    /// Matroska uses validated Cues, and MP4 can use a checked sync landing.
    /// Without an indexed landing, MP4 and TS replay preceding state.
    pub(crate) fn begin_seek(
        &mut self,
        intent: &SeekIntent,
        mp4_landing: Option<Mp4RangeLanding<'_>>,
    ) -> Result<()> {
        match self {
            Self::Matroska(r) => {
                let TrackKey::Matroska(number) = intent.track else {
                    return Err(Error::Invalid("seek track format"));
                };
                let at = r
                    .tracks()
                    .iter()
                    .position(|t| t.number == number && t.kind == 1)
                    .ok_or(Error::Invalid("seek needs source video track"))?;
                r.reader.seek_for_playback(
                    at,
                    intent
                        .target
                        .to_nanoseconds(triergon_core::Rounding::Floor)?,
                )?;
            }
            Self::Mp4(r) => {
                if r.progressive.is_some() {
                    return Err(Error::Unsupported(
                        "progressive replay requires retained source/index context",
                    ));
                }
                if !r.input.capabilities().random_access {
                    return Err(Error::Unsupported("MP4 replay needs source ranges"));
                }
                if r.plan.unavailable_first.is_some() {
                    return Err(Error::Unsupported("native first sample is unavailable"));
                }
                let TrackKey::Mp4(id) = intent.track else {
                    return Err(Error::Invalid("seek track format"));
                };
                let t = r.plan.track(id)?;
                if t.snapshot.handler != *b"vide" {
                    return Err(Error::Invalid("MP4 seek track is not video"));
                }
                let (row, offset, indexed_sync, demux_seek) = match mp4_landing {
                    Some(Mp4RangeLanding::Source(landing)) => {
                        let rows = r.plan.source_rows();
                        let first = rows
                            .get(landing.first_row)
                            .ok_or(Error::Invalid("MP4 seek first row"))?;
                        let sync = rows
                            .get(landing.sync_row)
                            .ok_or(Error::Invalid("MP4 seek sync row"))?;
                        if landing.first_row > landing.sync_row
                            || sync.track_id != id
                            || !sync.sample.is_container_sync()
                        {
                            return Err(Error::Invalid("MP4 seek landing"));
                        }
                        let skip_video = (landing.first_row < landing.sync_row)
                            .then_some((id, landing.sync_row));
                        (
                            landing.first_row,
                            first.sample.range.start,
                            skip_video,
                            None,
                        )
                    }
                    Some(Mp4RangeLanding::Demux(replay)) => {
                        let first = replay.first_row();
                        let occurrence = r
                            .plan
                            .demux_row(first)
                            .ok_or(Error::Invalid("MP4 native seek first row"))?;
                        (
                            first,
                            occurrence.source.range.start,
                            None,
                            Some(replay.clone()),
                        )
                    }
                    Some(Mp4RangeLanding::DemuxCursor(offset)) => (0, offset, None, None),
                    None => (0, 0, None, None),
                };
                r.input.reset(offset)?;
                r.row = row;
                r.indexed_sync = indexed_sync;
                r.demux_seek = demux_seek;
                r.packet = None;
                r.ended_short = false;
                r.configs = [None; 64];
            }
            Self::Ts(r) => {
                if r.segment_discovery.is_some() {
                    return Err(Error::Unsupported("seek needs a fresh TS segment"));
                }
                if r.native_context.is_some() {
                    return Err(Error::Unsupported(
                        "bound native startup needs a fresh observed context after seek",
                    ));
                }
                if !r.input.capabilities().random_access {
                    return Err(Error::Unsupported("TS replay needs source rewind"));
                }
                let TrackKey::Ts { program, pid: _ } = intent.track else {
                    return Err(Error::Invalid("seek track format"));
                };
                if program != r.program {
                    return Err(Error::Invalid("seek source program"));
                }
                if matches!(r.policy, SourceTimePolicy::Unanchored)
                    && intent.clock == crate::ClockChoice::Source
                {
                    return Err(Error::Unsupported("unanchored TS source time"));
                }
                r.input.reset(0)?;
                r.stream = TransportStream::new_with_namespace(
                    r.input.budget().clone(),
                    r.format,
                    r.limits,
                    r.program,
                    r.namespace,
                )?;
                r.source_pes_clock = Some(DemuxClock::new(r.program)?);
                r.tsid = None;
                r.pcr_pid = None;
                r.pending = None;
                r.ended = false;
                r.draining = false;
                for track in r.pes_tracks.as_mut_slice() {
                    track.observer = None;
                    track.update = None;
                }
                r.pes_active = None;
                r.pes_adopting = 0;
                r.pes_pending = None;
                r.pes_clock = Some(DemuxClock::new(r.program)?);
                r.event_ordinal = 0;
                r.pes_updates_from = None;
                r.boundary = None;
            }
        }
        Ok(())
    }
}
impl Mp4Engine {
    pub(crate) fn is_progressive(&self) -> bool {
        self.progressive.is_some()
    }

    pub(crate) fn supports_range_replay(&self) -> bool {
        self.progressive.is_none()
            && self.plan.unavailable_first.is_none()
            && self.input.capabilities().random_access
    }

    fn packet_config(&self, at_track: usize, description: u32) -> Result<Mp4Configuration> {
        let revision = self.configs[at_track]
            .ok_or(Error::Invalid("configuration missing"))?
            .revision;
        self.plan.tracks.as_slice()[at_track].configuration(description, revision)
    }

    fn skip_samples(
        &self,
        at_track: usize,
        occurrence: &PacketOccurrence,
    ) -> Result<Option<[u8; 10]>> {
        if occurrence.source.fragment_flags.is_some() && occurrence.source.ordinal == 1 {
            let samples = self.plan.tracks.as_slice()[at_track].fragment_aac_trim;
            return Ok((samples != 0).then(|| {
                let mut trim = [0; 10];
                trim[..4].copy_from_slice(&samples.to_le_bytes());
                trim
            }));
        }
        match &self.plan.encoded_edits {
            Some(plans) => plans.as_slice()[at_track]
                .as_ref()
                .map_or(Ok(None), |plan| plan.skip_samples(&occurrence.source)),
            None => Ok(None),
        }
    }

    fn emit_packet(
        &mut self,
        at_track: usize,
        source: SourcePacket,
        cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<EngineStep> {
        let cfg = self.packet_config(at_track, source.sample.description_index)?;
        let occurrence = match cursor {
            Some(cursor) => Some(cursor.take_occurrence()?),
            None => self.plan.demux_row(self.row).cloned(),
        };
        let encoded_skip_samples = occurrence.as_ref().map_or(Ok(None), |occurrence| {
            self.skip_samples(at_track, occurrence)
        })?;
        if occurrence.is_some() && source.sample.range.is_empty() {
            self.front_boundary = self.plan.front_read_boundary().filter(|boundary| {
                boundary.track_id == cfg.track.track_id
                    && boundary.source_ordinal == source.sample.ordinal
                    && boundary.source_offset == source.sample.range.start
            });
        }
        if let Some(replay) = &mut self.demux_seek {
            replay.delivered(self.row)?;
        } else {
            self.row += 1;
        }
        Ok(EngineStep::Event(Event::Mp4(Mp4Event::Packet(Mp4Packet {
            front_read_boundary: self.front_boundary,
            source,
            configuration: cfg,
            occurrence,
            encoded_skip_samples,
        }))))
    }

    fn poll_progressive(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<EngineStep> {
        let Some(state) = self.progressive.as_mut() else {
            return Ok(EngineStep::End);
        };
        let mut continuation = crate::progressive::Continuation {
            input: &mut self.input,
            plan: &mut self.plan,
            row: &mut self.row,
            media_cache_fill: &mut self.media_cache_fill,
            empty_run: &mut self.empty_run,
        };
        state.poll(&mut continuation, w, c)
    }

    fn poll_unavailable_first(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<EngineStep> {
        let start = self
            .plan
            .unavailable_first
            .as_ref()
            .ok_or(Error::Invalid("missing first sample declaration"))?
            .sample
            .range
            .start;
        let within_source = if self.plan.demux_recovery {
            Error::Invalid("native first sample starts before physical end")
        } else {
            crate::mp4_plan::BACKWARD_LAYOUT_REFUSAL
        };
        if self.input.position() > start {
            return Err(within_source);
        }
        let step = self.poll_progressive(w, c)?;
        if !matches!(step, EngineStep::End) {
            return Ok(step);
        }
        match self.input.read_discard(1, w, c)? {
            Step::End(end) => {
                if end.offset > start {
                    return Err(within_source);
                }
                if !self.plan.demux_recovery {
                    return Err(Error::Invalid("sample outside media extent"));
                }
                if !w.try_charge(1) {
                    return Ok(EngineStep::Yield);
                }
                let row = self
                    .plan
                    .unavailable_first
                    .as_ref()
                    .ok_or(Error::Invalid("missing first sample declaration"))?;
                self.ended_short = true;
                Ok(EngineStep::Event(Event::Mp4(Mp4Event::MissingSample(
                    Mp4MissingSample {
                        declared: row.sample.clone(),
                        track_id: row.track_id,
                        end,
                    },
                ))))
            }
            Step::NeedInput(r) => Ok(EngineStep::NeedInput(r)),
            Step::Yield => Ok(EngineStep::Yield),
            Step::Ready(_) => Err(Error::Invalid("first sample requires source end")),
        }
    }

    fn poll_packet(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
        cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<EngineStep> {
        let (at_track, reader) = self
            .packet
            .as_mut()
            .ok_or(Error::Invalid("pending sample missing"))?;
        let at_track = *at_track;
        let step = if self.plan.demux_recovery && self.plan.demux_row(self.row).is_some() {
            reader.poll_recovery(&mut self.input, w, c, RunRecoveryProfile)?
        } else {
            match reader.poll(&mut self.input, w, c)? {
                Step::Ready(packet) => Step::Ready(RecoverySampleRead::Complete(packet)),
                Step::NeedInput(r) => Step::NeedInput(r),
                Step::Yield => Step::Yield,
                Step::End(at) => Step::End(at),
            }
        };
        match step {
            Step::Ready(RecoverySampleRead::Complete(source)) => {
                let event = self.emit_packet(at_track, source, cursor)?;
                self.packet = None;
                Ok(event)
            }
            Step::Ready(RecoverySampleRead::Short(prefix)) => {
                let configuration =
                    self.packet_config(at_track, prefix.declared.description_index)?;
                let mut occurrence = self
                    .plan
                    .demux_row(self.row)
                    .ok_or(Error::Invalid("short packet needs native occurrence"))?
                    .clone();
                occurrence.demux.flags |= 2;
                let encoded_skip_samples = self.skip_samples(at_track, &occurrence)?;
                self.packet = None;
                self.ended_short = true;
                Ok(EngineStep::Event(Event::Mp4(Mp4Event::ShortPacket(
                    Mp4ShortPacket {
                        prefix,
                        configuration,
                        occurrence,
                        encoded_skip_samples,
                    },
                ))))
            }
            Step::NeedInput(r) => Ok(EngineStep::NeedInput(r)),
            Step::Yield => Ok(EngineStep::Yield),
            Step::End(_) => Err(Error::Truncated),
        }
    }

    // Keep backend-specific polling workspaces out of the shared dispatcher.
    #[inline(never)]
    fn poll(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
        mut cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<EngineStep> {
        if self.ended_short {
            self.input.read_into(&mut [], w, c)?;
            return Ok(EngineStep::End);
        }
        if self.plan.unavailable_first.is_some() {
            return self.poll_unavailable_first(w, c);
        }
        if let Some((mut packet, remaining)) = self.empty_run
            && self.front_boundary == Some(packet.boundary)
        {
            if !w.try_charge(1) {
                return Ok(EngineStep::Yield);
            }
            if remaining > 1 {
                self.empty_run = Some((packet, remaining - 1));
                packet.duration = 0;
            } else {
                self.empty_run = None;
            }
            return Ok(EngineStep::Event(Event::Mp4(Mp4Event::EmptyPacket(packet))));
        }
        if let Some(replay) = self.media_cache_fill.as_mut() {
            return match replay.poll(&mut self.input, w, c)? {
                Step::Ready(cached) => {
                    self.plan
                        .media_cache
                        .try_push(cached)
                        .map_err(|failure| failure.error)?;
                    self.media_cache_fill = None;
                    Ok(EngineStep::Yield)
                }
                Step::NeedInput(r) => Ok(EngineStep::NeedInput(r)),
                Step::Yield => Ok(EngineStep::Yield),
                Step::End(_) => Err(Error::Truncated),
            };
        }
        if self.packet.is_some() {
            return self.poll_packet(w, c, cursor);
        }
        if let Some(replay) = &mut self.demux_seek {
            match replay.poll(self.plan.demux_rows(), w, c)? {
                DemuxReplayStep::Ready(row) => self.row = row,
                DemuxReplayStep::Yield => return Ok(EngineStep::Yield),
                DemuxReplayStep::End => return Ok(EngineStep::End),
            }
        }
        if let Some((video, sync)) = self.indexed_sync {
            while self.row < sync && self.plan.source_rows()[self.row].track_id == video {
                if c.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                if !w.try_charge(1) {
                    return Ok(EngineStep::Yield);
                }
                self.row += 1;
            }
            if self.row >= sync {
                self.indexed_sync = None;
            }
        }
        let row = if let Some(cursor) = cursor.as_deref_mut() {
            match cursor.poll(w, c)? {
                CursorStatus::Ready => {
                    let occurrence = cursor
                        .occurrence()
                        .ok_or(Error::Invalid("missing native range occurrence"))?;
                    Some(SourceRow {
                        track_id: occurrence.track_id,
                        sample: occurrence.source.clone(),
                    })
                }
                CursorStatus::Yield => return Ok(EngineStep::Yield),
                CursorStatus::End => return Ok(EngineStep::End),
            }
        } else {
            self.plan.row(self.row)
        };
        let Some(row) = row else {
            return self.poll_progressive(w, c);
        };
        if self.plan.forward_regular_extents.is_some() {
            let [extent] = self.plan.media.as_slice() else {
                return Err(Error::Invalid("forward regular live media extent"));
            };
            if row.sample.require_within(extent.clone()).is_err() {
                if row.sample.range.start >= extent.end {
                    return self.poll_progressive(w, c);
                }
                if !w.try_charge(1) {
                    return Ok(EngineStep::Yield);
                }
                return Err(Error::Invalid("sample outside media extent"));
            }
        }
        if w.remaining() == 0 {
            return Ok(EngineStep::Yield);
        }
        let at = self
            .plan
            .tracks
            .as_slice()
            .iter()
            .position(|t| t.snapshot.track_id == row.track_id)
            .ok_or(Error::Invalid("sample track missing"))?;
        let changed = self.configs[at].is_none_or(|d| {
            d.track != row.track_id || d.description != row.sample.description_index
        });
        if changed {
            w.take(1);
            let revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
            let cfg = self.plan.tracks.as_slice()[at]
                .configuration(row.sample.description_index, revision)?;
            self.configs[at] = Some(CachedDescription {
                track: row.track_id,
                description: row.sample.description_index,
                revision,
            });
            self.revision = revision;
            return Ok(EngineStep::Event(Event::Mp4(Mp4Event::Configuration(cfg))));
        }
        let sequential = !self.input.capabilities().random_access;
        let position = self.input.position();
        if sequential && position < row.sample.range.start {
            let count = usize::try_from((row.sample.range.start - position).min(usize::MAX as u64))
                .map_err(|_| Error::Overflow)?;
            // Discard charges the byte work itself. With a one-unit quantum
            // that unit must reach the input cursor for the gap to advance.
            return match self.input.read_discard(count, w, c)? {
                Step::Ready(_) | Step::Yield => Ok(EngineStep::Yield),
                Step::NeedInput(r) => Ok(EngineStep::NeedInput(r)),
                Step::End(_) => Err(Error::Truncated),
            };
        }
        w.take(1);
        if sequential && position > row.sample.range.start {
            if let Some(payload) = self.plan.cached_payload(&row.sample.range)? {
                self.input.read_into(&mut [], w, c)?;
                if payload.len() > self.maximum_packet {
                    return Err(Error::Limit {
                        requested: payload.len(),
                        maximum: self.maximum_packet,
                    });
                }
                let source = SourcePacket {
                    sample: row.sample.clone(),
                    payload,
                    source: self.input.source(),
                    token: self.input.token(),
                };
                return self.emit_packet(at, source, cursor);
            }
            return Err(Error::Unsupported(
                "sequential source sample bytes already passed",
            ));
        }
        let extent = self
            .plan
            .media
            .as_slice()
            .iter()
            .find(|r| row.sample.require_within((*r).clone()).is_ok())
            .cloned();
        let media = if let Some(extent) = extent {
            RecoverySampleExtent::Finite(extent)
        } else if let Some(terminal) = self.plan.terminal_mdat {
            RecoverySampleExtent::Terminal(terminal)
        } else {
            return Err(Error::Invalid("sample has no source mdat"));
        };
        let packet = if self.plan.demux_recovery && self.plan.demux_row(self.row).is_some() {
            SampleReader::new_recovery(
                &mut self.input,
                row.sample,
                media,
                self.maximum_packet,
                RunRecoveryProfile,
            )?
        } else {
            match media {
                RecoverySampleExtent::Finite(extent) => {
                    // The extent lookup checked this range. A whole sample
                    // at the cursor can be emitted without staging a read.
                    let bytes = row.sample.range.end - row.sample.range.start;
                    if position == row.sample.range.start
                        && (1..=self.maximum_packet as u64).contains(&bytes)
                        && w.remaining() > bytes
                        && self.input.buffered_owner().is_some()
                        && bytes <= self.input.buffered().len() as u64
                    {
                        if !sequential {
                            self.input.request_range_reusing_window(position)?;
                        }
                        let Step::Ready(payload) = self.input.read_span(bytes as usize, w, c)?
                        else {
                            return Err(Error::Invalid("buffered sample unavailable"));
                        };
                        w.take(1);
                        let source = SourcePacket {
                            sample: row.sample,
                            payload,
                            source: self.input.source(),
                            token: self.input.token(),
                        };
                        return self.emit_packet(at, source, cursor);
                    }
                    SampleReader::new_with_shared_window(
                        &mut self.input,
                        row.sample,
                        extent,
                        self.maximum_packet,
                    )?
                }
                RecoverySampleExtent::Terminal(terminal) => SampleReader::new_terminal_mdat(
                    &mut self.input,
                    row.sample,
                    terminal,
                    self.maximum_packet,
                )?,
            }
        };
        self.packet = Some((at, packet));
        if w.remaining() == 0 {
            return Ok(EngineStep::Yield);
        }
        self.poll_packet(w, c, cursor)
    }
}
impl TsEngine {
    fn from_stream(
        input: InputCursor,
        format: ts::PacketFormat,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
        stream: TransportStream,
    ) -> Result<Self> {
        let pool = input.budget().clone();
        let program = stream.selected_program();
        let clock = || match program {
            0 => Ok(None),
            program => DemuxClock::new(program).map(Some),
        };
        Ok(Self {
            native_context: None,
            native_prefix_failed: false,
            stream,
            source_pes_clock: clock()?,
            input,
            format,
            program,
            limits,
            policy,
            namespace,
            tsid: None,
            pcr_pid: None,
            facts: BudgetVec::new(&pool),
            pending: None,
            ended: false,
            draining: false,
            pes_tracks: PesTracks::Inline(None),
            pes_active: None,
            pes_adopting: 0,
            pes_pending: None,
            pes_clock: clock()?,
            event_ordinal: 0,
            suppressed_events: 0,
            pes_updates_from: None,
            segment_discovery: None,
            segmented: false,
            boundary: None,
        })
    }

    /// The source events of `program`; see [`Engine::ts`].
    fn program(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
    ) -> Result<Self> {
        let stream = TransportStream::new_with_namespace(
            input.budget().clone(),
            format,
            limits,
            program,
            namespace,
        )?;
        Self::from_stream(input, format, limits, policy, namespace, stream)
    }

    /// The source events of a TS188 `program` and the demux PES of the selected
    /// PIDs: 1 to 64 distinct PIDs from 1 to 0x1ffe.
    pub(crate) fn with_selections(
        input: InputCursor,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
        selections: &[PidSelection],
    ) -> Result<Self> {
        if selections.is_empty() {
            return Err(Error::Invalid("native program selection is empty"));
        }
        if selections.len() > 64 {
            return Err(Error::Limit {
                requested: selections.len(),
                maximum: 64,
            });
        }
        if selections
            .iter()
            .any(|selection| !(1..=0x1ffe).contains(&selection.pid))
        {
            return Err(Error::Invalid("native program selection PID"));
        }
        let engine = Self::program(
            input,
            ts::PacketFormat::Ts188,
            program,
            limits,
            policy,
            namespace,
        )?;
        let pes_tracks = if let [selection] = selections {
            PesTracks::Inline(Some(PesTrack::new(*selection)))
        } else {
            let mut tracks = BudgetVec::new(engine.input.budget());
            tracks.try_reserve_exact(selections.len())?;
            for selection in selections {
                tracks
                    .try_push(PesTrack::new(*selection))
                    .map_err(|e| e.error)?;
            }
            tracks.as_mut_slice().sort_unstable_by_key(PesTrack::pid);
            if tracks
                .as_slice()
                .windows(2)
                .any(|pair| pair[0].pid() == pair[1].pid())
            {
                return Err(Error::Invalid("duplicate native program selection PID"));
            }
            PesTracks::Multiple(tracks)
        };
        Ok(Self {
            pes_tracks,
            ..engine
        })
    }

    /// The source events of `program` and the demux PES of one PID, with the
    /// observer bound to the startup context of this input.
    pub(crate) fn with_bound_input(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        selection: PidSelection,
        context: ts::compat::input::InputContext,
    ) -> Result<Self> {
        context.validate_cursor(&input)?;
        let engine = Self::program(
            input,
            format,
            program,
            limits,
            policy,
            DescriptorNamespace::Unspecified,
        )?;
        Ok(Self {
            pes_tracks: PesTracks::Inline(Some(PesTrack::new(selection))),
            native_context: Some(context),
            ..engine
        })
    }

    /// The source events of one independent TS188 segment and the demux PES of
    /// the tracks its own PMT names.
    pub(crate) fn discovered_segment(
        input: InputCursor,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
    ) -> Result<Self> {
        Self::segment(input, limits, policy, namespace, SegmentDiscovery::Native)
    }

    fn segment(
        input: InputCursor,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
        discovery: SegmentDiscovery,
    ) -> Result<Self> {
        let budget = input.budget().clone();
        let stream = match discovery {
            SegmentDiscovery::Native => {
                TransportStream::new_single_program_segment(budget, limits, namespace)?
            }
            SegmentDiscovery::Progressive | SegmentDiscovery::CompactProgressive => {
                TransportStream::new_single_program_progressive_feed(budget, limits, namespace)?
            }
            SegmentDiscovery::AvcAacFeed | SegmentDiscovery::MediaFeed => {
                TransportStream::new_single_program_feed(budget, limits, namespace)?
            }
        };
        let engine = Self::from_stream(
            input,
            ts::PacketFormat::Ts188,
            limits,
            policy,
            namespace,
            stream,
        )?;
        Ok(Self {
            segment_discovery: Some(discovery),
            ..engine
        })
    }

    /// A progressive PES feed of a single-program TS188 source: AVC and AAC
    /// tracks found in its PMT, delivered as owned or, with `compact`, compact
    /// payloads. Poll it with [`Self::poll_progressive_payload`].
    pub fn progressive(input: InputCursor, limits: ts::ParseLimits, compact: bool) -> Result<Self> {
        Self::segment(
            input,
            limits,
            SourceTimePolicy::Unanchored,
            DescriptorNamespace::Unspecified,
            if compact {
                SegmentDiscovery::CompactProgressive
            } else {
                SegmentDiscovery::Progressive
            },
        )
    }

    /// The input cursor the feed reads.
    #[inline]
    pub fn input(&self) -> &InputCursor {
        &self.input
    }

    /// Hands a source reply to the input; true when it was accepted.
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        provide(&mut self.input, reply)
    }

    /// The next step of a feed made by [`Self::progressive`].
    pub fn poll_progressive_payload(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<FeedStep> {
        if !matches!(
            self.segment_discovery,
            Some(SegmentDiscovery::Progressive | SegmentDiscovery::CompactProgressive)
        ) {
            return Err(Error::Invalid("compact TS payload selection required"));
        }
        self.poll::<FeedStep>(work, cancel)
    }

    /// Source events the feed consumed without delivering since the last call.
    pub fn take_suppressed_events(&mut self) -> u64 {
        std::mem::take(&mut self.suppressed_events)
    }

    /// Hands a consumed PES's lists back to the observer of `pid` for its next PES.
    pub fn reuse_pes_lists(&mut self, pid: u16, lists: crate::ts_payload::SpentLists) {
        let Some(index) = self.pes_tracks.index(pid) else {
            return;
        };
        if let Some(observer) = self.pes_tracks.as_mut_slice()[index].observer.as_mut() {
            if let Some(spans) = lists.spans {
                observer.reuse_payload_spans(spans);
            }
            if let Some(header) = lists.header {
                observer.reuse_header_ranges(header);
            }
        }
    }

    fn discover_tracks(&self, map: &ProgramMap) -> Result<PesTracks> {
        let mut selections = map.streams.as_slice().iter().filter_map(|stream| {
            let carriage = match stream.stream_type {
                0x1b => Carriage::Registered(RegisteredCarriage::Avc),
                0x24 => Carriage::Registered(RegisteredCarriage::Hevc),
                0x0f | 0x11 | 0x81 | 0x87 => Carriage::AudioFromPmt,
                0x06 if (matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_))
                    || self.segment_discovery == Some(SegmentDiscovery::MediaFeed))
                    && stream.descriptors().any(|entry| {
                        entry.is_ok_and(|descriptor| matches!(descriptor.tag, 0x6a | 0x7a))
                    }) =>
                {
                    Carriage::AudioFromPmt
                }
                _ => return None,
            };
            Some(PidSelection {
                pid: stream.pid,
                carriage,
            })
        });
        let count = selections.clone().count();
        if count <= 1 {
            return Ok(PesTracks::Inline(selections.next().map(PesTrack::new)));
        }
        let mut tracks = BudgetVec::new(self.input.budget());
        tracks.try_reserve_exact(count)?;
        for selection in selections {
            tracks
                .try_push(PesTrack::new(selection))
                .map_err(|error| error.error)?;
        }
        tracks.as_mut_slice().sort_unstable_by_key(PesTrack::pid);
        Ok(PesTracks::Multiple(tracks))
    }

    fn observer_for(
        &self,
        spec: PidSelection,
        map: &ProgramMap,
        configuration_epoch: u64,
    ) -> Result<(DemuxObserver, Carriage)> {
        let mismatch =
            Error::Unsupported("selected native carriage disagrees with accepted source PMT");
        let stream = map
            .streams
            .as_slice()
            .iter()
            .find(|entry| entry.pid == spec.pid)
            .ok_or(mismatch)?;
        let mut scope = DemuxScope {
            program: self.program,
            pid: spec.pid,
            stream_type: stream.stream_type,
            stream_id: 0xbd,
            entity: self.input.source(),
            epochs: ts::SourceEpochs {
                clock: self.stream.source_epochs().clock,
                configuration: configuration_epoch,
            },
        };
        let private_audio = PrivateAudioDescriptors {
            program: map.program_descriptors.as_ref(),
            elementary: stream.descriptor_bytes.as_ref(),
        };
        let hls_audio = self.segment_discovery == Some(SegmentDiscovery::MediaFeed);
        let carriage = match spec.carriage {
            Carriage::AudioFromPmt => match stream.stream_type {
                0x0f => Carriage::Registered(RegisteredCarriage::AacAdts),
                0x11 => Carriage::Registered(RegisteredCarriage::AacLatm),
                0x06 if !hls_audio
                    && !matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_)) =>
                {
                    return Err(Error::Unsupported(
                        "native PMT audio needs explicit DVB context",
                    ));
                }
                0x06 | 0x81 | 0x87 => Carriage::PrivateAudio(if hls_audio {
                    PrivateAudioCarriage::from_hls_pmt(
                        stream.stream_type,
                        private_audio,
                        self.limits,
                    )?
                } else {
                    PrivateAudioCarriage::from_observed_pmt(
                        stream.stream_type,
                        private_audio,
                        self.limits,
                    )?
                }),
                _ => return Err(mismatch),
            },
            fixed => fixed,
        };
        let profile = DemuxProfile::PesPackets;
        let pool = self.input.budget().clone();
        let mut observer = match carriage {
            Carriage::Registered(carriage) => {
                let (stream_type, stream_id) = match carriage {
                    RegisteredCarriage::Avc => (0x1b, 0xe0),
                    RegisteredCarriage::Hevc => (0x24, 0xe0),
                    RegisteredCarriage::AacAdts => (0x0f, 0xc0),
                    RegisteredCarriage::AacLatm => (0x11, 0xc0),
                };
                if stream.stream_type != stream_type {
                    return Err(mismatch);
                }
                scope.stream_id = stream_id;
                DemuxObserver::new_registered(profile, scope, carriage, pool, self.limits)
            }
            Carriage::PrivateAudio(carriage) if hls_audio => DemuxObserver::new_hls_private_audio(
                profile,
                scope,
                carriage,
                private_audio,
                pool,
                self.limits,
            ),
            Carriage::PrivateAudio(carriage) => DemuxObserver::new_private_audio(
                profile,
                scope,
                carriage,
                private_audio,
                pool,
                self.limits,
            ),
            Carriage::DvbSubtitles => DemuxObserver::new_dvb_subtitles(
                profile,
                scope,
                map.descriptor_namespace,
                DvbSubtitlesDescriptors {
                    program: map.program_descriptors.as_ref(),
                    elementary: stream.descriptor_bytes.as_ref(),
                },
                pool,
                self.limits,
            ),
            Carriage::AudioFromPmt => Err(mismatch),
        }?;
        if self.segment_discovery == Some(SegmentDiscovery::CompactProgressive)
            && matches!(carriage, Carriage::Registered(_))
        {
            observer.enable_compact_payloads()?;
        }
        Ok((observer, carriage))
    }

    fn offer_transport(
        &mut self,
        index: usize,
        mut packet: ts::transport::TransportPacket,
        context: SourceContext,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let track = &mut self.pes_tracks.as_mut_slice()[index];
        if let Some(observer) = track.observer.as_mut() {
            match observer.append_ready_continuation(&mut packet, context, work, cancel)? {
                ReadyContinuation::Buffered => return Ok(true),
                ReadyContinuation::Complete => {
                    self.pes_active = Some(index);
                    return Ok(false);
                }
                ReadyContinuation::Unhandled => {}
            }
            let pushed = if self.native_context.is_some() {
                observer.push_with_input(packet, context, &self.input)
            } else {
                observer.push(packet, context)
            };
            match pushed {
                Ok(()) => self.pes_active = Some(index),
                Err(rejected) => {
                    track.refuse(rejected.error);
                    self.pes_updates_from = Some(index);
                }
            }
            return Ok(false);
        }
        Ok(true)
    }

    fn source_context(&self) -> SourceContext {
        SourceContext {
            source_event_ordinal: self.event_ordinal,
            epochs: self.stream.source_epochs(),
            byte_run_epoch: None,
        }
    }

    const FEED_EVENT_QUANTUM: u64 = 64;

    fn poll_feed_stream(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<TraceEvent>> {
        let tracks = &mut self.pes_tracks;
        let active = &mut self.pes_active;
        let ordinal = self.event_ordinal;
        let suppressed = self.suppressed_events;
        self.stream.poll_feed_with_payload_runs(
            &mut self.input,
            work,
            cancel,
            &mut |item, omitted, epochs, work| {
                let packet = match item {
                    FeedItem::Carrier(packet) => packet,
                    FeedItem::PayloadRun(run) => {
                        // Leave the carrier that triggers a yield to the
                        // ordinary path, including its deferred continuity event.
                        let maximum = (Self::FEED_EVENT_QUANTUM - 1)
                            .saturating_sub(suppressed.saturating_add(omitted))
                            .div_ceil(2) as usize;
                        if maximum >= 2
                            && active.is_none()
                            && let Some(source_event_ordinal) = ordinal.checked_add(omitted)
                            && let Some(index) = tracks.index(run.pid())
                            && let Some(observer) = tracks.as_mut_slice()[index].observer.as_mut()
                            && run.append_ready(
                                observer,
                                SourceContext {
                                    source_event_ordinal,
                                    epochs,
                                    byte_run_epoch: None,
                                },
                                maximum,
                                work,
                            )?
                        {
                            return Ok(FeedContinuation::Buffered);
                        }
                        return Ok(FeedContinuation::Emit);
                    }
                };
                // Leave one unit for event consumption in addition to native append.
                if work.remaining() < 3 {
                    return Ok(FeedContinuation::Emit);
                }
                if let Some(index) = tracks.index(packet.pid())
                    && let Some(observer) = tracks.as_mut_slice()[index].observer.as_mut()
                {
                    let context = SourceContext {
                        source_event_ordinal: ordinal
                            .checked_add(omitted)
                            .ok_or(Error::Overflow)?,
                        epochs,
                        byte_run_epoch: None,
                    };
                    match packet.append_ready_continuation(observer, context, work, cancel)? {
                        ReadyContinuation::Unhandled => return Ok(FeedContinuation::Emit),
                        ReadyContinuation::Buffered => {}
                        ReadyContinuation::Complete => *active = Some(index),
                    }
                }
                let charged = work.try_charge(1);
                debug_assert!(charged);
                if active.is_some()
                    || work.remaining() == 0
                    || suppressed.saturating_add(omitted) >= Self::FEED_EVENT_QUANTUM - 1
                {
                    Ok(FeedContinuation::Yield)
                } else {
                    Ok(FeedContinuation::Buffered)
                }
            },
        )
    }

    fn project_pending_packet<T: TsStepProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<T> {
        let packet = self
            .pes_pending
            .as_ref()
            .ok_or(Error::Invalid("native packet missing"))?;
        let projector = self
            .pes_clock
            .as_mut()
            .ok_or(Error::Invalid("native TS packet before program discovery"))?;
        let (pid, epochs, header) = match packet {
            FeedPacket::Owned(packet) => (packet.pid, packet.source_epochs, packet.time_header()),
            FeedPacket::Compact(packet) => (packet.pid, packet.source_epochs, packet.time_header()),
        };
        let Some(clock) = projector.project_header(pid, epochs, header, work, cancel)? else {
            return T::project(FeedStep::Yield);
        };
        let packet = self
            .pes_pending
            .take()
            .ok_or(Error::Invalid("native packet missing"))?;
        T::project(FeedStep::Packet { packet, clock })
    }

    // Keep backend-specific polling workspaces out of the shared dispatcher.
    #[inline(never)]
    /// The next native track's configuration or refusal from track `at` on,
    /// if any is waiting.
    #[cold]
    fn next_track_update(&mut self, at: usize) -> Option<FeedStep> {
        let (index, pid, update) = self
            .pes_tracks
            .as_mut_slice()
            .iter_mut()
            .enumerate()
            .skip(at)
            .find_map(|(i, track)| track.update.take().map(|update| (i, track.pid(), update)))?;
        self.pes_updates_from = Some(index + 1);
        Some(match update {
            TrackUpdate::Configuration(configuration) => FeedStep::Configuration(configuration),
            TrackUpdate::Refusal(error) => FeedStep::Refusal {
                track: TrackKey::Ts {
                    program: self.program,
                    pid,
                },
                error,
                source_event_ordinal: self.event_ordinal,
            },
        })
    }

    /// Ends the current entity: a segment reports its end, and the source's
    /// end starts draining the stream and every native track.
    #[cold]
    fn end_entity<T: TsStepProjection>(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<T> {
        if self.draining {
            return Err(Error::Invalid("TS repeated undrained entity End"));
        }
        if self.segmented {
            if let Some(end) = self.stream.media_entity_end(&mut self.input, w, c)? {
                self.boundary = Some(end.clone());
                return T::project(FeedStep::EntityEnd(end));
            }
            return T::project(FeedStep::Yield);
        }
        if self.segment_discovery.is_some() && self.pcr_pid.is_none() {
            return Err(Error::Invalid("TS segment ended without an active PMT"));
        }
        self.stream.end_of_source()?;
        self.draining = true;
        for track in self.pes_tracks.as_mut_slice() {
            if let Some(observer) = track.observer.as_mut() {
                observer.end_of_source()?;
            }
        }
        self.pes_active = Some(0);
        T::project(FeedStep::Yield)
    }

    /// Refuses every native track once an M2TS input loses its carrier grid.
    #[cold]
    fn refuse_after_lost_grid(&mut self) {
        self.native_prefix_failed = true;
        for track in self.pes_tracks.as_mut_slice() {
            track.refuse(Error::Unsupported("native M2TS carrier grid lost"));
        }
        self.pes_updates_from = Some(0);
    }

    fn poll<T: TsStepProjection>(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<T> {
        if let Some(at) = self.pes_updates_from.take()
            && self
                .pes_tracks
                .as_slice()
                .get(at..)
                .is_some_and(|tracks| tracks.iter().any(|track| track.update.is_some()))
            && let Some(event) = self.next_track_update(at)
        {
            return T::project(event);
        }
        if self.pes_pending.is_some() {
            return self.project_pending_packet(w, c);
        }
        while let Some(index) = self.pes_active {
            let Some(track) = self.pes_tracks.as_mut_slice().get_mut(index) else {
                self.pes_active = None;
                break;
            };
            if let Some(observer) = track.observer.as_mut() {
                let result = if observer.uses_compact_payloads() {
                    let step = if self.native_context.is_some() {
                        observer.poll_compact_payload_with_input(&self.input, w, c)
                    } else {
                        observer.poll_compact_payload(w, c)
                    };
                    step.map(|step| map_payload(step, FeedPacket::Compact))
                } else {
                    let step = if self.native_context.is_some() {
                        observer.poll_with_input(&self.input, w, c)
                    } else {
                        observer.poll(w, c)
                    };
                    step.map(|step| map_payload(step, FeedPacket::Owned))
                };
                match result {
                    Ok(ts::AssemblyStep::Output(packet)) => {
                        self.pes_pending = Some(packet);
                        if T::PROGRESSIVE
                            || self.segment_discovery.is_none()
                            || matches!(
                                self.segment_discovery,
                                Some(
                                    SegmentDiscovery::Progressive
                                        | SegmentDiscovery::CompactProgressive
                                )
                            )
                        {
                            return self.project_pending_packet(w, c);
                        }
                        return T::project(FeedStep::Yield);
                    }
                    Ok(ts::AssemblyStep::Yield) => return T::project(FeedStep::Yield),
                    Ok(ts::AssemblyStep::NeedPayload | ts::AssemblyStep::Finished) => {}
                    Err(error @ (Error::Backpressure { .. } | Error::Cancelled)) => {
                        return Err(error);
                    }
                    Err(error) => {
                        track.refuse(error);
                        self.pes_updates_from = Some(index);
                        self.pes_active = self.draining.then_some(index + 1);
                        return T::project(FeedStep::Yield);
                    }
                }
            }
            self.pes_active = self.draining.then_some(index + 1);
        }
        let progressive = T::PROGRESSIVE
            || matches!(
                self.segment_discovery,
                Some(SegmentDiscovery::Progressive | SegmentDiscovery::CompactProgressive)
            );
        let feed_only = progressive
            || matches!(
                self.segment_discovery,
                Some(
                    SegmentDiscovery::Progressive
                        | SegmentDiscovery::CompactProgressive
                        | SegmentDiscovery::AvcAacFeed
                        | SegmentDiscovery::MediaFeed
                )
            );
        if self.ended {
            return T::project(FeedStep::End);
        }
        if self.boundary.is_some() {
            return T::project(FeedStep::AwaitEntity);
        }
        loop {
            if self.pending.is_none() {
                let step = if feed_only {
                    self.poll_feed_stream(w, c)
                } else {
                    self.stream.poll(&mut self.input, w, c)
                };
                if feed_only {
                    let omitted = self.stream.take_feed_omissions();
                    self.event_ordinal = self
                        .event_ordinal
                        .checked_add(omitted)
                        .ok_or(Error::Overflow)?;
                    self.suppressed_events += omitted;
                }
                match step? {
                    Step::Ready(e) => self.pending = Some(e),
                    Step::NeedInput(r) => return T::project(FeedStep::NeedInput(r)),
                    Step::Yield => return T::project(FeedStep::Yield),
                    Step::End(_) => return self.end_entity(w, c),
                }
            }
            if (feed_only || !matches!(self.pending, Some(TraceEvent::Pes(_)))) && !w.try_charge(1)
            {
                return T::project(FeedStep::Yield);
            }
            if let Some(TraceEvent::Diagnostic(fault)) = &self.pending
                && fault.kind == ts::stream::FaultKind::Transport
                && matches!(
                    fault.error,
                    Error::Invalid("TS sync byte") | Error::Truncated
                )
                && !self.native_prefix_failed
                && self
                    .native_context
                    .as_ref()
                    .is_some_and(triergon_mpegts::compat::input::InputContext::is_prefix)
            {
                self.refuse_after_lost_grid();
            }
            if matches!(
                self.pending,
                Some(TraceEvent::ProgramMap { active: true, .. } | TraceEvent::Pat(_))
            ) && self.adopt_table(feed_only)?
            {
                return T::project(FeedStep::Yield);
            }
            let event = self
                .pending
                .as_ref()
                .ok_or(Error::Invalid("TS event missing"))?;
            // Feeds consume native payloads and the source facts needed for
            // admission. The source parser still validates every carrier.
            if feed_only
                && !matches!(
                    event,
                    TraceEvent::ProgramMap { active: true, .. } | TraceEvent::Diagnostic(_)
                )
                && !(progressive
                    && matches!(
                        event,
                        TraceEvent::Pat(_)
                            | TraceEvent::Continuity { .. }
                            | TraceEvent::Timeline { .. }
                    ))
            {
                let event = self
                    .pending
                    .take()
                    .ok_or(Error::Invalid("TS event missing"))?;
                let mut drained = true;
                match event {
                    TraceEvent::Transport(packet) => {
                        if let Some(index) = self.pes_tracks.index(packet.pid) {
                            let context = self.source_context();
                            drained = self.offer_transport(index, packet, context, w, c)?;
                        }
                    }
                    TraceEvent::Pes(_) if self.program == 0 => {
                        return Err(Error::Invalid("TS PES before program discovery"));
                    }
                    TraceEvent::Finished => self.ended = true,
                    _ => {}
                }
                self.event_ordinal = self.event_ordinal.checked_add(1).ok_or(Error::Overflow)?;
                self.suppressed_events += 1;
                // Return periodically to the player's cancellation and event cap.
                if !drained
                    || self.ended
                    || w.remaining() == 0
                    || self.suppressed_events >= Self::FEED_EVENT_QUANTUM
                {
                    return T::project(FeedStep::Yield);
                }
                continue;
            }
            // A feed delivers only its rare table, clock and fault events whole.
            return if feed_only {
                self.deliver_feed_event(w, c)
            } else {
                self.deliver_event(w, c)
            };
        }
    }

    /// Adopts the pending PAT or active PMT: the program, the native tracks and
    /// their observers, and the track facts. True while the PMT's next track
    /// still needs its observer, which leaves the event pending.
    #[cold]
    fn adopt_table(&mut self, feed_only: bool) -> Result<bool> {
        let Some(event) = self.pending.as_ref() else {
            return Ok(false);
        };
        if let TraceEvent::ProgramMap {
            map,
            active: true,
            configuration_epoch,
        } = event
        {
            if map.streams.len() > 64 {
                return Err(Error::Limit {
                    requested: map.streams.len(),
                    maximum: 64,
                });
            }
            if self.segment_discovery.is_some() && self.pes_adopting == 0 {
                self.pes_tracks = self.discover_tracks(map)?;
            }
            if !self.native_prefix_failed
                && let Some(track) = self.pes_tracks.as_slice().get(self.pes_adopting)
            {
                let observer = self.observer_for(track.selection, map, *configuration_epoch);
                let track = &mut self.pes_tracks.as_mut_slice()[self.pes_adopting];
                match observer {
                    Ok((mut observer, carriage)) => {
                        if let Some(context) = self.native_context.as_ref() {
                            observer.bind_input(context, &self.input)?;
                        }
                        track.observer = Some(observer);
                        track.update = (self.segment_discovery.is_some()
                            || track.selection.carriage == Carriage::AudioFromPmt)
                            .then_some(TrackUpdate::Configuration(AdmittedCarriage {
                                track: TrackKey::Ts {
                                    program: self.program,
                                    pid: track.pid(),
                                },
                                configuration_epoch: *configuration_epoch,
                                carriage,
                            }));
                    }
                    Err(error) => track.refuse(error),
                }
                self.pes_adopting += 1;
                if self.pes_adopting < self.pes_tracks.as_slice().len() {
                    return Ok(true);
                }
            }
            self.facts
                .try_reserve_exact(map.streams.len().saturating_sub(self.facts.len()))?;
            self.facts.clear();
            for e in map.streams.as_slice() {
                let observer = self
                    .pes_tracks
                    .index(e.pid)
                    .and_then(|i| self.pes_tracks.as_slice()[i].observer.as_ref());
                let kind = track_kind(selected_track_kind(e, observer));
                self.facts
                    .try_push(TrackFact {
                        key: TrackKey::Ts {
                            program: self.program,
                            pid: e.pid,
                        },
                        kind,
                        enabled: true,
                    })
                    .map_err(|e| e.error)?;
            }
            self.pes_adopting = 0;
            self.pes_updates_from = self
                .pes_tracks
                .as_slice()
                .iter()
                .position(|track| track.update.is_some());
            self.pcr_pid = Some(map.pcr_pid);
        }
        if let TraceEvent::Pat(table) = event {
            if self.program == 0 {
                let program = self.stream.selected_program();
                if !feed_only {
                    self.source_pes_clock = Some(DemuxClock::new(program)?);
                }
                self.pes_clock = Some(DemuxClock::new(program)?);
                self.program = program;
            }
            self.tsid = Some(table.header.extension);
        }
        Ok(false)
    }

    /// Delivers the pending source event with its source and native clocks,
    /// offering a transport packet to its native track first.
    #[inline(always)]
    fn deliver_event<T: TsStepProjection>(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<T> {
        let event = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("TS event missing"))?;
        let key = match (self.tsid, self.pcr_pid) {
            (Some(tsid), Some(pcr_pid)) => Some(ClockKey {
                tsid,
                program: self.program,
                pcr_pid,
                epoch: match event {
                    TraceEvent::Pes(p) => p.epochs.clock,
                    _ => self.stream.source_epochs().clock,
                },
            }),
            _ => None,
        };
        let mut source_pts = None;
        let mut source_dts = None;
        let mut demux_clock = None;
        if let TraceEvent::Pes(pes) = event {
            if let Some(key) = key {
                source_pts = Some(source_timestamp(pes.header.pts_raw33, key, self.policy)?);
                source_dts = Some(source_timestamp(pes.header.dts_raw33, key, self.policy)?);
            }
            // Native clock work remains separately charged and may yield without taking
            // the source event. No native time is copied into source fields.
            let projector = self
                .source_pes_clock
                .as_mut()
                .ok_or(Error::Invalid("TS PES before program discovery"))?;
            demux_clock = projector.project(pes, w, c)?;
            if demux_clock.is_none() {
                return T::project(FeedStep::Yield);
            }
        }
        let event = self
            .pending
            .take()
            .ok_or(Error::Invalid("TS event missing"))?;
        if let TraceEvent::Transport(packet) = &event
            && let Some(index) = self.pes_tracks.index(packet.pid)
        {
            let context = self.source_context();
            let track = &mut self.pes_tracks.as_mut_slice()[index];
            if let Some(observer) = track.observer.as_mut() {
                // Keep one unit for delivery of the original source event.
                let ready = if w.remaining() >= 3 {
                    observer.append_ready_borrowed_continuation(
                        packet,
                        context,
                        &self.input,
                        w,
                        c,
                    )?
                } else {
                    ReadyContinuation::Unhandled
                };
                match ready {
                    ReadyContinuation::Buffered => {}
                    ReadyContinuation::Complete => self.pes_active = Some(index),
                    ReadyContinuation::Unhandled => match observer.push_borrowed(
                        packet,
                        context,
                        self.native_context.as_ref().map(|_| &self.input),
                    ) {
                        Ok(()) => self.pes_active = Some(index),
                        Err(error) => {
                            track.refuse(error);
                            self.pes_updates_from = Some(index);
                        }
                    },
                }
            }
        }
        self.event_ordinal = self.event_ordinal.checked_add(1).ok_or(Error::Overflow)?;
        if matches!(event, TraceEvent::Finished) {
            self.ended = true;
        }
        T::project(FeedStep::Event(TsEvent {
            source: event,
            source_pts,
            source_dts,
            demux_clock,
            clock_key: key,
        }))
    }

    /// [`Self::deliver_event`] for a feed, which consumes its payload events and
    /// delivers only the rare ones whole.
    #[cold]
    fn deliver_feed_event<T: TsStepProjection>(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<T> {
        self.deliver_event(w, c)
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

#[cfg(test)]
mod projection_tests {
    use super::*;
    use ts::{
        SourceEpochs,
        clock::SourceTimePolicy,
        compat::pes::{ClockOrigin, TimestampOrigin},
    };

    #[test]
    fn ts_projection_preserves_source_clock_fields_and_refusal_ordinal() {
        let key = ClockKey {
            tsid: 3,
            program: 7,
            pcr_pid: 256,
            epoch: 11,
        };
        let event = TsEvent {
            source: TraceEvent::Timeline {
                program: key.program,
                clock_epoch: key.epoch,
            },
            source_pts: Some(
                source_timestamp(Some(9), key, SourceTimePolicy::RelativeEpochZero).unwrap(),
            ),
            source_dts: Some(source_timestamp(None, key, SourceTimePolicy::Unanchored).unwrap()),
            demux_clock: Some(DemuxTimeView {
                profile: DemuxProfile::PesPackets,
                program: key.program,
                pid: 257,
                source_epochs: SourceEpochs {
                    clock: key.epoch,
                    configuration: 13,
                },
                source_pts_raw33: Some(9),
                source_dts_raw33: None,
                pts90: Some(-17),
                dts90: Some(-17),
                pts_origin: TimestampOrigin::SourcePts,
                dts_origin: TimestampOrigin::PtsOnlyCompatibility,
                origin: Some(ClockOrigin {
                    first_reference_raw90: 9,
                    interval_origin90: -26,
                }),
            }),
            clock_key: Some(key),
        };
        let expected = format!("{event:?}");
        let step = FeedStep::project(FeedStep::Event(event)).unwrap();
        assert_eq!(step.payload_bytes(), Some(0));
        let EngineStep::Event(Event::Ts(event)) = EngineStep::project(step).unwrap() else {
            panic!("source event was not delivered");
        };
        assert_eq!(format!("{event:?}"), expected);

        let track = TrackKey::Ts {
            program: 7,
            pid: 257,
        };
        let error = Error::Unsupported("test carriage");
        let step = FeedStep::project(FeedStep::Refusal {
            track,
            error,
            source_event_ordinal: 109,
        })
        .unwrap();
        assert_eq!(step.payload_bytes(), Some(0));
        assert!(
            matches!(EngineStep::project(step), Ok(EngineStep::Event(Event::PesRefusal {
            track: actual_track, error: actual_error, source_event_ordinal: 109,
        })) if actual_track == track && actual_error == error)
        );
        assert_eq!(FeedStep::End.payload_bytes(), None);
        assert_eq!(FeedStep::AwaitEntity.payload_bytes(), None);
    }
}
