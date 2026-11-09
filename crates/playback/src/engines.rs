use crate::input::SourceReply;
use crate::{
    Format, Result, SeekIntent, TrackKey, TrackKind,
    io::provide,
    mp4_native_replay::{NativeMp4Replay, NativeMp4ReplayStep},
    mp4_open::{PendingMedia, RETAINED_MEDIA_BYTES},
    mp4_plan::{Mp4Configuration, PreparedMp4, SourceRow},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Error, InputCursor, InputRequest, SourceCapabilities, SourceId,
    SourceLocation, Step, Time, TimeBase, WorkBudget,
};
use triergon_isobmff::{
    SampleReader, SourcePacket,
    compat::mov::{
        front::FrontReadBoundary,
        packets::{CursorStatus, PacketCursor, PacketOccurrence},
        recovery::{RecoverySampleExtent, RecoverySampleRead, RunRecoveryProfile, ShortSample},
    },
};
use triergon_matroska::{
    incremental::ParseStep,
    typed::{Event as MkvEvent, Reader},
};
use triergon_mpegts::{
    self as ts, FeedContinuation, FeedItem, TraceEvent, TransportStream,
    clock::{ClockKey, SourceTimePolicy, SourceTimestamp, source_timestamp},
};
use ts::compat::pes::{
    CompactDemuxPacket, DemuxClock, DemuxObserver, DemuxPacket, DemuxProfile, DemuxScope,
    DemuxTimeView, DvbSubtitlesDescriptors, PrivateAudioCarriage, PrivateAudioDescriptors,
    ReadyContinuation, RegisteredCarriage, SourceContext,
};
use ts::psi::{DescriptorNamespace, ProgramMap};
#[derive(Clone, Debug)]
pub struct Mp4Packet {
    /// Explicit native read boundary, independent of the full Source stream.
    /// Later Source packets remain present with `native == None`.
    pub native_front_read_boundary_v1: Option<FrontReadBoundary>,
    pub source: SourcePacket,
    pub configuration: Mp4Configuration,
    pub native: Option<PacketOccurrence>,
    /// Sample trim from the associated encoded edit plan, in native skip-samples layout.
    pub encoded_skip_samples: Option<[u8; 10]>,
}
/// An unassociated empty packet in the pinned native receiver view. Its public
/// position is a receiver field, with no corresponding physical sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mp4NativeEmptyPacket {
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
pub struct Mp4NativeShortPacket {
    pub prefix: ShortSample,
    pub configuration: Mp4Configuration,
    pub native: PacketOccurrence,
}
/// The first native read has a positive declaration starting at or beyond the
/// confirmed physical end. The view ends without emitting a packet.
#[derive(Clone, Debug)]
pub struct Mp4NativeMissingSample {
    pub declared: triergon_isobmff::Sample,
    pub track_id: u32,
    pub end: SourceLocation,
}
#[derive(Clone, Debug)]
pub enum Mp4Event {
    Configuration(Mp4Configuration),
    Packet(Mp4Packet),
    NativeEmptyPacket(Mp4NativeEmptyPacket),
    NativeShortPacket(Mp4NativeShortPacket),
    NativeMissingSample(Mp4NativeMissingSample),
}
#[derive(Debug)]
pub struct TsEvent {
    pub source: TraceEvent,
    pub source_pts: Option<SourceTimestamp>,
    pub source_dts: Option<SourceTimestamp>,
    pub native_clock: Option<DemuxTimeView>,
    pub clock_key: Option<ClockKey>,
}
#[derive(Debug)]
pub enum RawEvent {
    Mp4Root(triergon_isobmff::BoxView),
    EntityEnd(ts::MediaEntityEnd),
    NativeTs {
        packet: DemuxPacket,
        clock: DemuxTimeView,
    },
    NativeTsConfiguration(TsNativeConfiguration),
    NativeRefusal {
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
pub enum BackendStep {
    Event(RawEvent),
    NeedInput(InputRequest),
    Yield,
    End,
    Backpressure { requested: usize, available: usize },
    AwaitEntity,
}
impl BackendStep {
    pub(crate) fn from_matroska(step: ParseStep<MkvEvent>) -> Self {
        match step {
            ParseStep::Event(event) => Self::Event(RawEvent::Matroska(event)),
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
    Native(&'a NativeMp4Replay),
    NativeCursor(u64),
}

pub struct Mp4Engine {
    progressive: Option<crate::progressive::ProgressiveState>,
    replay_media: Option<PendingMedia>,
    native_boundary_observed: Option<FrontReadBoundary>,
    native_empty_run: Option<(Mp4NativeEmptyPacket, u32)>,
    native_recovery_end: bool,
    pub(crate) input: InputCursor,
    pub plan: PreparedMp4,
    pub(crate) row: usize,
    indexed_sync: Option<(u32, usize)>,
    native_seek: Option<NativeMp4Replay>,
    packet: Option<(usize, SampleReader)>,
    configs: [Option<CachedDescription>; 64],
    revision: u64,
    pub(crate) maximum_packet: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsNativeSelection {
    pub pid: u16,
    pub carriage: RegisteredCarriage,
}
/// Opt in to the finite private-audio packet view for one TS188 PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsNativePrivateAudioSelection {
    pub pid: u16,
    pub carriage: PrivateAudioCarriage,
}

/// Explicit packet interpretation for one selected PID of a TS program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TsNativeCarriage {
    Registered(RegisteredCarriage),
    PrivateAudio(PrivateAudioCarriage),
    DvbSubtitles,
    /// Resolve the established AAC and private-audio profiles from each active PMT.
    AudioFromPmt,
}

/// Snapshot of a track's admitted carriage for one active source PMT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsNativeConfiguration {
    pub track: TrackKey,
    pub configuration_epoch: u64,
    /// The resolved registered or private-audio carriage.
    pub carriage: TsNativeCarriage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsNativeTrackSelection {
    pub pid: u16,
    pub carriage: TsNativeCarriage,
}
impl From<TsNativeSelection> for TsNativeTrackSelection {
    fn from(selection: TsNativeSelection) -> Self {
        Self {
            pid: selection.pid,
            carriage: TsNativeCarriage::Registered(selection.carriage),
        }
    }
}

enum TsNativeUpdate {
    Configuration(TsNativeConfiguration),
    Refusal(Error),
}

struct TsNativeTrack {
    selection: TsNativeTrackSelection,
    observer: Option<DemuxObserver>,
    update: Option<TsNativeUpdate>,
}
impl TsNativeTrack {
    fn new(selection: TsNativeTrackSelection) -> Self {
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
        self.update = Some(TsNativeUpdate::Refusal(error));
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "legacy single-track playback has an inline observer and requires no extra allocation"
)]
enum TsNativeTracks {
    Inline(Option<TsNativeTrack>),
    Multiple(BudgetVec<TsNativeTrack>),
}
impl TsNativeTracks {
    fn as_slice(&self) -> &[TsNativeTrack] {
        match self {
            Self::Inline(track) => track.as_slice(),
            Self::Multiple(tracks) => tracks.as_slice(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [TsNativeTrack] {
        match self {
            Self::Inline(track) => track.as_mut_slice(),
            Self::Multiple(tracks) => tracks.as_mut_slice(),
        }
    }

    fn index(&self, pid: u16) -> Option<usize> {
        self.as_slice()
            .binary_search_by_key(&pid, TsNativeTrack::pid)
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
    pub(crate) input: InputCursor,
    stream: TransportStream,
    format: ts::PacketFormat,
    program: u16,
    limits: ts::ParseLimits,
    policy: SourceTimePolicy,
    namespace: DescriptorNamespace,
    native: Option<DemuxClock>,
    tsid: Option<u16>,
    pcr_pid: Option<u16>,
    facts: BudgetVec<TrackFact>,
    pending: Option<TraceEvent>,
    ended: bool,
    draining: bool,
    native_tracks: TsNativeTracks,
    native_active: Option<usize>,
    native_map_next: usize,
    native_pending: Option<NativePacket>,
    native_packet_clock: Option<DemuxClock>,
    event_ordinal: u64,
    suppressed_events: u64,
    native_update_at: Option<usize>,
    segment_discovery: Option<SegmentDiscovery>,
    segmented: bool,
    boundary: Option<ts::MediaEntityEnd>,
}

#[derive(Debug)]
pub(crate) enum NativePacket {
    Owned(DemuxPacket),
    Compact(CompactDemuxPacket),
}

#[derive(Debug)]
pub(crate) enum TsPayloadStep {
    Event(TsEvent),
    Configuration(TsNativeConfiguration),
    Refusal {
        track: TrackKey,
        error: Error,
        source_event_ordinal: u64,
    },
    Packet {
        packet: NativePacket,
        clock: DemuxTimeView,
    },
    EntityEnd(ts::MediaEntityEnd),
    NeedInput(InputRequest),
    Yield,
    End,
    AwaitEntity,
}

impl TsPayloadStep {
    pub(crate) fn payload_bytes(&self) -> Option<usize> {
        Some(match self {
            Self::Packet { packet, .. } => match packet {
                NativePacket::Owned(packet) => packet.payload_len,
                NativePacket::Compact(packet) => packet.payload_len,
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
    fn project(step: TsPayloadStep) -> Result<Self>;
}

impl TsStepProjection for BackendStep {
    #[inline]
    fn project(step: TsPayloadStep) -> Result<Self> {
        Ok(match step {
            TsPayloadStep::Event(event) => Self::Event(RawEvent::Ts(event)),
            TsPayloadStep::Configuration(config) => {
                Self::Event(RawEvent::NativeTsConfiguration(config))
            }
            TsPayloadStep::Refusal {
                track,
                error,
                source_event_ordinal,
            } => Self::Event(RawEvent::NativeRefusal {
                track,
                error,
                source_event_ordinal,
            }),
            TsPayloadStep::Packet {
                packet: NativePacket::Owned(packet),
                clock,
            } => Self::Event(RawEvent::NativeTs { packet, clock }),
            TsPayloadStep::Packet {
                packet: NativePacket::Compact(_),
                ..
            } => return Err(Error::Invalid("compact TS payload polling required")),
            TsPayloadStep::EntityEnd(end) => Self::Event(RawEvent::EntityEnd(end)),
            TsPayloadStep::NeedInput(request) => Self::NeedInput(request),
            TsPayloadStep::Yield => Self::Yield,
            TsPayloadStep::End => Self::End,
            TsPayloadStep::AwaitEntity => Self::AwaitEntity,
        })
    }
}

impl TsStepProjection for TsPayloadStep {
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
    pub fn matroska(
        pool: &Budget,
        id: SourceId,
        caps: SourceCapabilities,
        limits: triergon_matroska::Limits,
        window: usize,
    ) -> Result<Self> {
        Ok(Self::matroska_reader(Reader::new(
            pool, id, caps, limits, window,
        )?))
    }

    /// Play an unstarted Matroska reader configured by the caller, for example
    /// with `Reader::new_with_payload_ownership`,
    /// `Reader::new_with_immutable_seek_index` or
    /// `compat::block_additions::new_reader`. A player refuses the engine if
    /// the reader has already read or repositioned its input.
    pub fn matroska_reader(reader: Reader) -> Self {
        let source = reader.source();
        Self::Matroska(MkvEngine { reader, source })
    }
    pub fn mp4(input: InputCursor, plan: PreparedMp4, maximum_packet: usize) -> Result<Self> {
        let engine = Self::mp4_unattached(input, plan, maximum_packet)?;
        if matches!(&engine, Self::Mp4(mp4) if mp4.plan.native_front) {
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
            replay_media: None,
            native_boundary_observed: None,
            native_empty_run: None,
            native_recovery_end: false,
            input,
            plan,
            row: 0,
            indexed_sync: None,
            native_seek: None,
            packet: None,
            configs: [None; 64],
            revision: 0,
            maximum_packet,
        };
        if engine.plan.replay_native_order {
            engine.begin_media_replay()?;
        }
        Ok(Self::Mp4(engine))
    }
    pub fn mp4_progressive(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
        limits: crate::progressive::ProgressiveLimits,
    ) -> Result<Self> {
        if plan.native_front {
            return Err(Error::Unsupported(
                "native front plan needs explicit native attachment",
            ));
        }
        Self::mp4_with_progressive(input, plan, maximum_packet, limits)
    }
    /// Complete one actual front-metadata source using the explicit nonseekable
    /// native view. A terminal MDAT finishes only on a matching Source End.
    pub fn mp4_native_terminal_v1(
        input: InputCursor,
        plan: PreparedMp4,
        maximum_packet: usize,
        limits: crate::progressive::ProgressiveLimits,
    ) -> Result<Self> {
        if !plan.native_front || input.capabilities().random_access {
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
    pub fn ts(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
    ) -> Result<Self> {
        Self::ts_with_namespace(
            input,
            format,
            program,
            limits,
            policy,
            DescriptorNamespace::Unspecified,
        )
    }

    /// Strict TS playback with an explicit descriptor namespace.
    pub fn ts_with_namespace(
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
        TsEngine::from_stream(input, format, limits, policy, namespace, stream).map(Self::Ts)
    }
    pub fn ts_native(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        selection: TsNativeSelection,
    ) -> Result<Self> {
        if format != ts::PacketFormat::Ts188 {
            return Err(Error::Unsupported(
                "full native M2TS requires an admitted startup-position context; use source and clock views",
            ));
        }
        Self::ts_with_native(input, format, program, limits, policy, selection.into())
    }
    /// Expose the admitted native private-audio PES alongside unchanged Source
    /// facts. The chosen carriage is checked against the active program map.
    pub fn ts_native_private_audio_v1(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        selection: TsNativePrivateAudioSelection,
    ) -> Result<Self> {
        if format != ts::PacketFormat::Ts188 {
            return Err(Error::Unsupported("native private audio requires TS188"));
        }
        Self::ts_with_native(
            input,
            format,
            program,
            limits,
            policy,
            TsNativeTrackSelection {
                pid: selection.pid,
                carriage: TsNativeCarriage::PrivateAudio(selection.carriage),
            },
        )
    }
    fn ts_with_native(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        selection: TsNativeTrackSelection,
    ) -> Result<Self> {
        let mut engine = Self::ts(input, format, program, limits, policy)?;
        if let Self::Ts(t) = &mut engine {
            t.native_tracks = TsNativeTracks::Inline(Some(TsNativeTrack::new(selection)));
        }
        Ok(engine)
    }
    pub(crate) fn ts_native_context(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        selection: TsNativeSelection,
        context: ts::compat::input::InputContext,
    ) -> Result<Self> {
        context.validate_cursor(&input)?;
        let mut engine =
            Self::ts_with_native(input, format, program, limits, policy, selection.into())?;
        if let Self::Ts(t) = &mut engine {
            t.native_context = Some(context);
        }
        Ok(engine)
    }
    /// Assemble explicitly selected PIDs from one TS188 program. The input
    /// carrier order determines completed packet order. Remaining packets drain
    /// by ascending PID at logical EOF. All selected packets share the same
    /// native program clock, independently of the source-clock view.
    pub fn ts_native_program_v1(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
        selections: &[TsNativeTrackSelection],
    ) -> Result<Self> {
        if format != ts::PacketFormat::Ts188 {
            return Err(Error::Unsupported(
                "multiplexed native playback requires TS188",
            ));
        }
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
        let mut engine =
            Self::ts_with_namespace(input, format, program, limits, policy, namespace)?;
        if let Self::Ts(ts) = &mut engine {
            ts.native_tracks = if let [selection] = selections {
                TsNativeTracks::Inline(Some(TsNativeTrack::new(*selection)))
            } else {
                let mut tracks = BudgetVec::new(ts.input.budget());
                tracks.try_reserve_exact(selections.len())?;
                for selection in selections {
                    tracks
                        .try_push(TsNativeTrack::new(*selection))
                        .map_err(|e| e.error)?;
                }
                tracks
                    .as_mut_slice()
                    .sort_unstable_by_key(TsNativeTrack::pid);
                if tracks
                    .as_slice()
                    .windows(2)
                    .any(|pair| pair[0].pid() == pair[1].pid())
                {
                    return Err(Error::Invalid("duplicate native program selection PID"));
                }
                TsNativeTracks::Multiple(tracks)
            };
        }
        Ok(engine)
    }

    pub fn has_native_packet_view(&self) -> bool {
        matches!(self, Self::Ts(t) if t.segment_discovery.is_some() || !t.native_tracks.as_slice().is_empty())
    }
    pub(crate) fn replaces_source_ts_pes(&self, pid: u16) -> bool {
        matches!(self, Self::Ts(t) if t.native_tracks.index(pid).is_some())
    }
    /// Read an independent TS188 media segment with an in-band PAT and PMT.
    /// Discover its single program, announce supported track carriage before
    /// packet delivery, and preserve other tracks as Source events. Create a new
    /// engine for each segment.
    pub fn ts_native_segment_v1(
        input: InputCursor,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
    ) -> Result<Self> {
        Self::ts_native_segment(input, limits, policy, namespace, SegmentDiscovery::Native)
    }

    pub(crate) fn ts_hls_segment(
        input: InputCursor,
        limits: ts::ParseLimits,
        media: bool,
    ) -> Result<Self> {
        Self::ts_native_segment(
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
    }

    fn ts_native_segment(
        input: InputCursor,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
        namespace: DescriptorNamespace,
        discovery: SegmentDiscovery,
    ) -> Result<Self> {
        TsEngine::native_segment(input, limits, policy, namespace, discovery).map(Self::Ts)
    }

    pub fn ts_segmented(
        input: InputCursor,
        format: ts::PacketFormat,
        program: u16,
        limits: ts::ParseLimits,
        policy: SourceTimePolicy,
    ) -> Result<Self> {
        let mut engine = Self::ts(input, format, program, limits, policy)?;
        if let Self::Ts(t) = &mut engine {
            t.segmented = true;
        }
        Ok(engine)
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
    pub fn poll(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<BackendStep> {
        match self {
            Self::Matroska(r) => Ok(BackendStep::from_matroska(r.step(w, c)?)),
            Self::Mp4(r) => r.poll(w, c, None),
            Self::Ts(r) => {
                if r.segment_discovery == Some(SegmentDiscovery::CompactProgressive) {
                    return Err(Error::Invalid("compact TS payload polling required"));
                }
                r.poll::<BackendStep>(w, c)
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
    ) -> Result<BackendStep> {
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
                        kind: match t.kind {
                            1 => TrackKind::Video,
                            2 => TrackKind::Audio,
                            17 => TrackKind::Subtitle,
                            _ => TrackKind::Other,
                        },
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
                        kind: match &h.handler {
                            b"vide" => TrackKind::Video,
                            b"soun" => TrackKind::Audio,
                            b"text" | b"subt" | b"sbtl" | b"subp" => TrackKind::Subtitle,
                            _ => TrackKind::Other,
                        },
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
                let (row, offset, indexed_sync, native_seek) = match mp4_landing {
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
                    Some(Mp4RangeLanding::Native(replay)) => {
                        let first = replay.first_row();
                        let occurrence = r
                            .plan
                            .native(first)
                            .ok_or(Error::Invalid("MP4 native seek first row"))?;
                        (
                            first,
                            occurrence.source.range.start,
                            None,
                            Some(replay.clone()),
                        )
                    }
                    Some(Mp4RangeLanding::NativeCursor(offset)) => (0, offset, None, None),
                    None => (0, 0, None, None),
                };
                r.input.reset(offset)?;
                r.row = row;
                r.indexed_sync = indexed_sync;
                r.native_seek = native_seek;
                r.packet = None;
                r.native_recovery_end = false;
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
                    && intent.clock == crate::ClockChoice::SourceExact
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
                r.native = Some(DemuxClock::new(r.program)?);
                r.tsid = None;
                r.pcr_pid = None;
                r.pending = None;
                r.ended = false;
                r.draining = false;
                for track in r.native_tracks.as_mut_slice() {
                    track.observer = None;
                    track.update = None;
                }
                r.native_active = None;
                r.native_map_next = 0;
                r.native_pending = None;
                r.native_packet_clock = Some(DemuxClock::new(r.program)?);
                r.event_ordinal = 0;
                r.native_update_at = None;
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

    pub(crate) fn stage_native_empty_run(
        &mut self,
        boundary: FrontReadBoundary,
        time_base: TimeBase,
        duration: i64,
        remaining: u32,
    ) {
        self.native_empty_run = Some((
            Mp4NativeEmptyPacket {
                boundary,
                time_base,
                handler: *b"vide",
                pts: 0,
                dts: 0,
                duration,
                position: 0,
                flags: 0,
            },
            remaining,
        ));
    }

    pub(crate) fn begin_media_replay(&mut self) -> Result<()> {
        let [extent] = self.plan.media.as_slice() else {
            return Err(Error::Unsupported(
                "native replay needs a finite media extent",
            ));
        };
        if self.input.position() != extent.start || self.input.capabilities().random_access {
            return Err(Error::Invalid("native replay source position"));
        }
        let length = extent.end - extent.start;
        if !(1..=RETAINED_MEDIA_BYTES as u64).contains(&length) {
            return Err(Error::Unsupported(
                "native replay exceeds media cache bound",
            ));
        }
        let pending = PendingMedia::new(extent.start, length as usize, self.input.budget())?;
        self.plan.media_cache.try_reserve_exact(1)?;
        self.replay_media = Some(pending);
        self.plan.replay_native_order = true;
        Ok(())
    }

    fn packet_config(&self, at_track: usize, description: u32) -> Result<Mp4Configuration> {
        let revision = self.configs[at_track]
            .ok_or(Error::Invalid("configuration missing"))?
            .revision;
        self.plan.tracks.as_slice()[at_track].configuration(description, revision)
    }

    fn emit_packet(
        &mut self,
        at_track: usize,
        source: SourcePacket,
        cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<BackendStep> {
        let cfg = self.packet_config(at_track, source.sample.description_index)?;
        let native = match cursor {
            Some(cursor) => Some(cursor.take_occurrence()?),
            None => self.plan.native(self.row).cloned(),
        };
        let encoded_skip_samples =
            if let (Some(occurrence), Some(plans)) = (&native, &self.plan.encoded_edits) {
                plans.as_slice()[at_track]
                    .as_ref()
                    .map_or(Ok(None), |plan| plan.skip_samples(&occurrence.source))?
            } else {
                None
            };
        if native.is_some() && source.sample.range.is_empty() {
            self.native_boundary_observed =
                self.plan
                    .native_front_read_boundary_v1()
                    .filter(|boundary| {
                        boundary.track_id == cfg.track.track_id
                            && boundary.source_ordinal == source.sample.ordinal
                            && boundary.source_offset == source.sample.range.start
                    });
        }
        if let Some(replay) = &mut self.native_seek {
            replay.delivered(self.row)?;
        } else {
            self.row += 1;
        }
        Ok(BackendStep::Event(RawEvent::Mp4(Mp4Event::Packet(
            Mp4Packet {
                native_front_read_boundary_v1: self.native_boundary_observed,
                source,
                configuration: cfg,
                native,
                encoded_skip_samples,
            },
        ))))
    }

    fn poll_progressive(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<BackendStep> {
        if let Some(mut state) = self.progressive.take() {
            let result = state.poll(self, w, c);
            self.progressive = Some(state);
            return result;
        }
        Ok(BackendStep::End)
    }

    fn poll_unavailable_first(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<BackendStep> {
        let start = self
            .plan
            .unavailable_first
            .as_ref()
            .ok_or(Error::Invalid("missing first sample declaration"))?
            .sample
            .range
            .start;
        let within_source = if self.plan.native_recovery {
            Error::Invalid("native first sample starts before physical end")
        } else {
            crate::mp4_plan::BACKWARD_LAYOUT_REFUSAL
        };
        if self.input.position() > start {
            return Err(within_source);
        }
        let step = self.poll_progressive(w, c)?;
        if !matches!(step, BackendStep::End) {
            return Ok(step);
        }
        match self.input.read_discard(1, w, c)? {
            Step::End(end) => {
                if end.offset > start {
                    return Err(within_source);
                }
                if !self.plan.native_recovery {
                    return Err(Error::Invalid("sample outside media extent"));
                }
                if !w.try_charge(1) {
                    return Ok(BackendStep::Yield);
                }
                let row = self
                    .plan
                    .unavailable_first
                    .as_ref()
                    .ok_or(Error::Invalid("missing first sample declaration"))?;
                self.native_recovery_end = true;
                Ok(BackendStep::Event(RawEvent::Mp4(
                    Mp4Event::NativeMissingSample(Mp4NativeMissingSample {
                        declared: row.sample.clone(),
                        track_id: row.track_id,
                        end,
                    }),
                )))
            }
            Step::NeedInput(r) => Ok(BackendStep::NeedInput(r)),
            Step::Yield => Ok(BackendStep::Yield),
            Step::Ready(_) => Err(Error::Invalid("first sample requires source end")),
        }
    }

    fn poll_packet(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
        cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<BackendStep> {
        let (at_track, reader) = self
            .packet
            .as_mut()
            .ok_or(Error::Invalid("pending sample missing"))?;
        let at_track = *at_track;
        let step = if self.plan.native_recovery && self.plan.native(self.row).is_some() {
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
                let mut native = self
                    .plan
                    .native(self.row)
                    .ok_or(Error::Invalid("short packet needs native occurrence"))?
                    .clone();
                native.demux.flags |= 2;
                self.packet = None;
                self.native_recovery_end = true;
                Ok(BackendStep::Event(RawEvent::Mp4(
                    Mp4Event::NativeShortPacket(Mp4NativeShortPacket {
                        prefix,
                        configuration,
                        native,
                    }),
                )))
            }
            Step::NeedInput(r) => Ok(BackendStep::NeedInput(r)),
            Step::Yield => Ok(BackendStep::Yield),
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
    ) -> Result<BackendStep> {
        if self.native_recovery_end {
            self.input.read_into(&mut [], w, c)?;
            return Ok(BackendStep::End);
        }
        if self.plan.unavailable_first.is_some() {
            return self.poll_unavailable_first(w, c);
        }
        if let Some((mut packet, remaining)) = self.native_empty_run
            && self.native_boundary_observed == Some(packet.boundary)
        {
            if !w.try_charge(1) {
                return Ok(BackendStep::Yield);
            }
            if remaining > 1 {
                self.native_empty_run = Some((packet, remaining - 1));
                packet.duration = 0;
            } else {
                self.native_empty_run = None;
            }
            return Ok(BackendStep::Event(RawEvent::Mp4(
                Mp4Event::NativeEmptyPacket(packet),
            )));
        }
        if let Some(replay) = self.replay_media.as_mut() {
            return match replay.poll(&mut self.input, w, c)? {
                Step::Ready(cached) => {
                    self.plan
                        .media_cache
                        .try_push(cached)
                        .map_err(|failure| failure.error)?;
                    self.replay_media = None;
                    Ok(BackendStep::Yield)
                }
                Step::NeedInput(r) => Ok(BackendStep::NeedInput(r)),
                Step::Yield => Ok(BackendStep::Yield),
                Step::End(_) => Err(Error::Truncated),
            };
        }
        if self.packet.is_some() {
            return self.poll_packet(w, c, cursor);
        }
        if let Some(replay) = &mut self.native_seek {
            match replay.poll(self.plan.native_rows(), w, c)? {
                NativeMp4ReplayStep::Ready(row) => self.row = row,
                NativeMp4ReplayStep::Yield => return Ok(BackendStep::Yield),
                NativeMp4ReplayStep::End => return Ok(BackendStep::End),
            }
        }
        if let Some((video, sync)) = self.indexed_sync {
            while self.row < sync && self.plan.source_rows()[self.row].track_id == video {
                if c.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                if !w.try_charge(1) {
                    return Ok(BackendStep::Yield);
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
                CursorStatus::Yield => return Ok(BackendStep::Yield),
                CursorStatus::End => return Ok(BackendStep::End),
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
                    return Ok(BackendStep::Yield);
                }
                return Err(Error::Invalid("sample outside media extent"));
            }
        }
        if w.remaining() == 0 {
            return Ok(BackendStep::Yield);
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
            return Ok(BackendStep::Event(RawEvent::Mp4(Mp4Event::Configuration(
                cfg,
            ))));
        }
        let sequential = !self.input.capabilities().random_access;
        let position = self.input.position();
        if sequential && position < row.sample.range.start {
            let count = usize::try_from((row.sample.range.start - position).min(usize::MAX as u64))
                .map_err(|_| Error::Overflow)?;
            // Discard charges the byte work itself. With a one-unit quantum
            // that unit must reach the input cursor for the gap to advance.
            return match self.input.read_discard(count, w, c)? {
                Step::Ready(_) | Step::Yield => Ok(BackendStep::Yield),
                Step::NeedInput(r) => Ok(BackendStep::NeedInput(r)),
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
        let packet = if self.plan.native_recovery && self.plan.native(self.row).is_some() {
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
            return Ok(BackendStep::Yield);
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
            native: clock()?,
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
            native_tracks: TsNativeTracks::Inline(None),
            native_active: None,
            native_map_next: 0,
            native_pending: None,
            native_packet_clock: clock()?,
            event_ordinal: 0,
            suppressed_events: 0,
            native_update_at: None,
            segment_discovery: None,
            segmented: false,
            boundary: None,
        })
    }

    fn native_segment(
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
        let mut engine = Self::from_stream(
            input,
            ts::PacketFormat::Ts188,
            limits,
            policy,
            namespace,
            stream,
        )?;
        engine.segment_discovery = Some(discovery);
        Ok(engine)
    }

    pub(crate) fn progressive(
        input: InputCursor,
        limits: ts::ParseLimits,
        compact: bool,
    ) -> Result<Self> {
        Self::native_segment(
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

    pub(crate) fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        provide(&mut self.input, reply)
    }

    pub(crate) fn poll_progressive_payload(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TsPayloadStep> {
        if !matches!(
            self.segment_discovery,
            Some(SegmentDiscovery::Progressive | SegmentDiscovery::CompactProgressive)
        ) {
            return Err(Error::Invalid("compact TS payload selection required"));
        }
        self.poll::<TsPayloadStep>(work, cancel)
    }

    pub(crate) fn take_suppressed_events(&mut self) -> u64 {
        std::mem::take(&mut self.suppressed_events)
    }

    fn discover_tracks(&self, map: &ProgramMap) -> Result<TsNativeTracks> {
        let mut selections = map.streams.as_slice().iter().filter_map(|stream| {
            let carriage = match stream.stream_type {
                0x1b => TsNativeCarriage::Registered(RegisteredCarriage::Avc),
                0x24 => TsNativeCarriage::Registered(RegisteredCarriage::Hevc),
                0x0f | 0x11 | 0x81 | 0x87 => TsNativeCarriage::AudioFromPmt,
                0x06 if (matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_))
                    || self.segment_discovery == Some(SegmentDiscovery::MediaFeed))
                    && stream.descriptors().any(|entry| {
                        entry.is_ok_and(|descriptor| matches!(descriptor.tag, 0x6a | 0x7a))
                    }) =>
                {
                    TsNativeCarriage::AudioFromPmt
                }
                _ => return None,
            };
            Some(TsNativeTrackSelection {
                pid: stream.pid,
                carriage,
            })
        });
        let count = selections.clone().count();
        if count <= 1 {
            return Ok(TsNativeTracks::Inline(
                selections.next().map(TsNativeTrack::new),
            ));
        }
        let mut tracks = BudgetVec::new(self.input.budget());
        tracks.try_reserve_exact(count)?;
        for selection in selections {
            tracks
                .try_push(TsNativeTrack::new(selection))
                .map_err(|error| error.error)?;
        }
        tracks
            .as_mut_slice()
            .sort_unstable_by_key(TsNativeTrack::pid);
        Ok(TsNativeTracks::Multiple(tracks))
    }

    fn native_observer(
        &self,
        spec: TsNativeTrackSelection,
        map: &ProgramMap,
        configuration_epoch: u64,
    ) -> Result<(DemuxObserver, TsNativeCarriage)> {
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
            TsNativeCarriage::AudioFromPmt => match stream.stream_type {
                0x0f => TsNativeCarriage::Registered(RegisteredCarriage::AacAdts),
                0x11 => TsNativeCarriage::Registered(RegisteredCarriage::AacLatm),
                0x06 if !hls_audio
                    && !matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_)) =>
                {
                    return Err(Error::Unsupported(
                        "native PMT audio needs explicit DVB context",
                    ));
                }
                0x06 | 0x81 | 0x87 => TsNativeCarriage::PrivateAudio(if hls_audio {
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
            TsNativeCarriage::Registered(carriage) => {
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
            TsNativeCarriage::PrivateAudio(carriage) if hls_audio => {
                DemuxObserver::new_hls_private_audio(
                    profile,
                    scope,
                    carriage,
                    private_audio,
                    pool,
                    self.limits,
                )
            }
            TsNativeCarriage::PrivateAudio(carriage) => DemuxObserver::new_private_audio(
                profile,
                scope,
                carriage,
                private_audio,
                pool,
                self.limits,
            ),
            TsNativeCarriage::DvbSubtitles => DemuxObserver::new_dvb_subtitles(
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
            TsNativeCarriage::AudioFromPmt => Err(mismatch),
        }?;
        if self.segment_discovery == Some(SegmentDiscovery::CompactProgressive)
            && matches!(carriage, TsNativeCarriage::Registered(_))
        {
            observer.enable_compact_payloads()?;
        }
        Ok((observer, carriage))
    }

    fn offer_native_transport(
        &mut self,
        index: usize,
        mut packet: ts::TransportPacket,
        context: SourceContext,
        ready: Option<(&mut WorkBudget, &AtomicBool)>,
    ) -> Result<bool> {
        let track = &mut self.native_tracks.as_mut_slice()[index];
        if let Some(observer) = track.observer.as_mut() {
            if let Some((work, cancel)) = ready {
                match observer.append_ready_continuation(&mut packet, context, work, cancel)? {
                    ReadyContinuation::Buffered => return Ok(true),
                    ReadyContinuation::Complete => {
                        self.native_active = Some(index);
                        return Ok(false);
                    }
                    ReadyContinuation::Unhandled => {}
                }
            }
            let pushed = if self.native_context.is_some() {
                observer.push_with_input(packet, context, &self.input)
            } else {
                observer.push(packet, context)
            };
            match pushed {
                Ok(()) => self.native_active = Some(index),
                Err(rejected) => {
                    track.refuse(rejected.error);
                    self.native_update_at = Some(index);
                }
            }
            return Ok(false);
        }
        Ok(true)
    }

    fn native_source_context(&self) -> SourceContext {
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
        let tracks = &mut self.native_tracks;
        let active = &mut self.native_active;
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
            .native_pending
            .as_ref()
            .ok_or(Error::Invalid("native packet missing"))?;
        let projector = self
            .native_packet_clock
            .as_mut()
            .ok_or(Error::Invalid("native TS packet before program discovery"))?;
        let (pid, epochs, header) = match packet {
            NativePacket::Owned(packet) => (packet.pid, packet.source_epochs, packet.time_header()),
            NativePacket::Compact(packet) => {
                (packet.pid, packet.source_epochs, packet.time_header())
            }
        };
        let Some(clock) = projector.project_header(pid, epochs, header, work, cancel)? else {
            return T::project(TsPayloadStep::Yield);
        };
        let packet = self
            .native_pending
            .take()
            .ok_or(Error::Invalid("native packet missing"))?;
        T::project(TsPayloadStep::Packet { packet, clock })
    }

    // Keep backend-specific polling workspaces out of the shared dispatcher.
    #[inline(never)]
    fn poll<T: TsStepProjection>(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<T> {
        if let Some(at) = self.native_update_at.take()
            && let Some((index, pid, update)) = self
                .native_tracks
                .as_mut_slice()
                .iter_mut()
                .enumerate()
                .skip(at)
                .find_map(|(i, track)| track.update.take().map(|update| (i, track.pid(), update)))
        {
            self.native_update_at = Some(index + 1);
            let event = match update {
                TsNativeUpdate::Configuration(configuration) => {
                    TsPayloadStep::Configuration(configuration)
                }
                TsNativeUpdate::Refusal(error) => TsPayloadStep::Refusal {
                    track: TrackKey::Ts {
                        program: self.program,
                        pid,
                    },
                    error,
                    source_event_ordinal: self.event_ordinal,
                },
            };
            return T::project(event);
        }
        if self.native_pending.is_some() {
            return self.project_pending_packet(w, c);
        }
        while let Some(index) = self.native_active {
            let Some(track) = self.native_tracks.as_mut_slice().get_mut(index) else {
                self.native_active = None;
                break;
            };
            if let Some(observer) = track.observer.as_mut() {
                let result = if observer.uses_compact_payloads() {
                    let step = if self.native_context.is_some() {
                        observer.poll_compact_payload_with_input(&self.input, w, c)
                    } else {
                        observer.poll_compact_payload(w, c)
                    };
                    step.map(|step| map_payload(step, NativePacket::Compact))
                } else {
                    let step = if self.native_context.is_some() {
                        observer.poll_with_input(&self.input, w, c)
                    } else {
                        observer.poll(w, c)
                    };
                    step.map(|step| map_payload(step, NativePacket::Owned))
                };
                match result {
                    Ok(ts::AssemblyStep::Output(packet)) => {
                        self.native_pending = Some(packet);
                        if matches!(
                            self.segment_discovery,
                            Some(
                                SegmentDiscovery::Progressive
                                    | SegmentDiscovery::CompactProgressive
                            )
                        ) {
                            return self.project_pending_packet(w, c);
                        }
                        return T::project(TsPayloadStep::Yield);
                    }
                    Ok(ts::AssemblyStep::Yield) => return T::project(TsPayloadStep::Yield),
                    Ok(ts::AssemblyStep::NeedPayload | ts::AssemblyStep::Finished) => {}
                    Err(error @ (Error::Backpressure { .. } | Error::Cancelled)) => {
                        return Err(error);
                    }
                    Err(error) => {
                        track.refuse(error);
                        self.native_update_at = Some(index);
                        self.native_active = self.draining.then_some(index + 1);
                        return T::project(TsPayloadStep::Yield);
                    }
                }
            }
            self.native_active = self.draining.then_some(index + 1);
        }
        let progressive = matches!(
            self.segment_discovery,
            Some(SegmentDiscovery::Progressive | SegmentDiscovery::CompactProgressive)
        );
        let feed_only = matches!(
            self.segment_discovery,
            Some(
                SegmentDiscovery::Progressive
                    | SegmentDiscovery::CompactProgressive
                    | SegmentDiscovery::AvcAacFeed
                    | SegmentDiscovery::MediaFeed
            )
        );
        if self.ended {
            return T::project(TsPayloadStep::End);
        }
        if self.boundary.is_some() {
            return T::project(TsPayloadStep::AwaitEntity);
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
                    Step::NeedInput(r) => return T::project(TsPayloadStep::NeedInput(r)),
                    Step::Yield => return T::project(TsPayloadStep::Yield),
                    Step::End(_) => {
                        if self.draining {
                            return Err(Error::Invalid("TS repeated undrained entity End"));
                        }
                        if self.segmented {
                            if let Some(end) =
                                self.stream.media_entity_end(&mut self.input, w, c)?
                            {
                                self.boundary = Some(end.clone());
                                return T::project(TsPayloadStep::EntityEnd(end));
                            }
                            return T::project(TsPayloadStep::Yield);
                        }
                        if self.segment_discovery.is_some() && self.pcr_pid.is_none() {
                            return Err(Error::Invalid("TS segment ended without an active PMT"));
                        }
                        self.stream.end_of_source()?;
                        self.draining = true;
                        for track in self.native_tracks.as_mut_slice() {
                            if let Some(observer) = track.observer.as_mut() {
                                observer.end_of_source()?;
                            }
                        }
                        self.native_active = Some(0);
                        return T::project(TsPayloadStep::Yield);
                    }
                }
            }
            if (feed_only || !matches!(self.pending, Some(TraceEvent::Pes(_)))) && !w.try_charge(1)
            {
                return T::project(TsPayloadStep::Yield);
            }
            let event = self
                .pending
                .as_ref()
                .ok_or(Error::Invalid("TS event missing"))?;
            if let TraceEvent::Diagnostic(fault) = event
                && fault.kind == ts::FaultKind::Transport
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
                self.native_prefix_failed = true;
                for track in self.native_tracks.as_mut_slice() {
                    track.refuse(Error::Unsupported("native M2TS carrier grid lost"));
                }
                self.native_update_at = Some(0);
            }
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
                if self.segment_discovery.is_some() && self.native_map_next == 0 {
                    self.native_tracks = self.discover_tracks(map)?;
                }
                if !self.native_prefix_failed
                    && let Some(track) = self.native_tracks.as_slice().get(self.native_map_next)
                {
                    let observer = self.native_observer(track.selection, map, *configuration_epoch);
                    let track = &mut self.native_tracks.as_mut_slice()[self.native_map_next];
                    match observer {
                        Ok((mut observer, carriage)) => {
                            if let Some(context) = self.native_context.as_ref() {
                                observer.bind_input(context, &self.input)?;
                            }
                            track.observer = Some(observer);
                            track.update = (self.segment_discovery.is_some()
                                || track.selection.carriage == TsNativeCarriage::AudioFromPmt)
                                .then_some(TsNativeUpdate::Configuration(TsNativeConfiguration {
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
                    self.native_map_next += 1;
                    if self.native_map_next < self.native_tracks.as_slice().len() {
                        return T::project(TsPayloadStep::Yield);
                    }
                }
                self.facts
                    .try_reserve_exact(map.streams.len().saturating_sub(self.facts.len()))?;
                self.facts.clear();
                for e in map.streams.as_slice() {
                    let kind = match e.stream_type {
                        0x1b | 0x24 | 0x02 => TrackKind::Video,
                        0x0f | 0x11 | 0x03 | 0x04 | 0x81 | 0x87 => TrackKind::Audio,
                        // HDMV presentation graphics (PGS) in BDAV and TS.
                        0x90 => TrackKind::Subtitle,
                        // DVB subtitling or teletext descriptors in a DVB namespace.
                        0x06 if matches!(
                            e.codec(),
                            Some(
                                triergon_core::container::CodecId::DvbSubtitle
                                    | triergon_core::container::CodecId::DvbTeletext
                            )
                        ) =>
                        {
                            TrackKind::Subtitle
                        }
                        0x06 => self
                            .native_tracks
                            .index(e.pid)
                            .map(|i| &self.native_tracks.as_slice()[i])
                            .filter(|track| track.observer.is_some())
                            .map_or(TrackKind::Other, |track| match track.selection.carriage {
                                TsNativeCarriage::PrivateAudio(_)
                                | TsNativeCarriage::AudioFromPmt => TrackKind::Audio,
                                TsNativeCarriage::DvbSubtitles => TrackKind::Subtitle,
                                TsNativeCarriage::Registered(_) => TrackKind::Other,
                            }),
                        _ => TrackKind::Other,
                    };
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
                self.native_map_next = 0;
                self.native_update_at = self
                    .native_tracks
                    .as_slice()
                    .iter()
                    .position(|track| track.update.is_some());
                self.pcr_pid = Some(map.pcr_pid);
            }
            if let TraceEvent::Pat(table) = event {
                if self.program == 0 {
                    let program = self.stream.selected_program();
                    if !feed_only {
                        self.native = Some(DemuxClock::new(program)?);
                    }
                    self.native_packet_clock = Some(DemuxClock::new(program)?);
                    self.program = program;
                }
                self.tsid = Some(table.header.extension);
            }
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
                        if let Some(index) = self.native_tracks.index(packet.pid) {
                            let context = self.native_source_context();
                            drained =
                                self.offer_native_transport(index, packet, context, Some((w, c)))?;
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
                    return T::project(TsPayloadStep::Yield);
                }
                continue;
            }
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
            let mut native_clock = None;
            if let TraceEvent::Pes(pes) = event {
                if let Some(key) = key {
                    source_pts = Some(source_timestamp(pes.header.pts_raw33, key, self.policy)?);
                    source_dts = Some(source_timestamp(pes.header.dts_raw33, key, self.policy)?);
                }
                // Native clock work remains separately charged and may yield without taking
                // the source event. No native time is copied into source fields.
                let projector = self
                    .native
                    .as_mut()
                    .ok_or(Error::Invalid("TS PES before program discovery"))?;
                native_clock = projector.project(pes, w, c)?;
                if native_clock.is_none() {
                    return T::project(TsPayloadStep::Yield);
                }
            }
            if let TraceEvent::Transport(packet) = event
                && let Some(index) = self.native_tracks.index(packet.pid)
                && let Some(observer) = self.native_tracks.as_mut_slice()[index].observer.as_mut()
            {
                let context = SourceContext {
                    source_event_ordinal: self.event_ordinal,
                    epochs: self.stream.source_epochs(),
                    byte_run_epoch: None,
                };
                match observer.append_ready_borrowed_continuation(
                    packet,
                    context,
                    &self.input,
                    w,
                    c,
                )? {
                    ReadyContinuation::Buffered => {}
                    ReadyContinuation::Complete => self.native_active = Some(index),
                    ReadyContinuation::Unhandled => {
                        self.offer_native_transport(index, packet.clone(), context, None)?;
                    }
                }
            }
            let event = self
                .pending
                .take()
                .ok_or(Error::Invalid("TS event missing"))?;
            self.event_ordinal = self.event_ordinal.checked_add(1).ok_or(Error::Overflow)?;
            if matches!(event, TraceEvent::Finished) {
                self.ended = true;
            }
            return T::project(TsPayloadStep::Event(TsEvent {
                source: event,
                source_pts,
                source_dts,
                native_clock,
                clock_key: key,
            }));
        }
    }
}
impl RawEvent {
    pub fn track(&self) -> Option<TrackKey> {
        match self {
            Self::NativeTs { packet, .. } => Some(TrackKey::Ts {
                program: packet.program,
                pid: packet.pid,
            }),
            Self::Matroska(MkvEvent::Packet(p)) => p
                .configuration
                .tracks()
                .get(p.packet.track)
                .map(|t| TrackKey::Matroska(t.number)),
            Self::Mp4(Mp4Event::Packet(p)) => Some(TrackKey::Mp4(p.configuration.track.track_id)),
            Self::Mp4(Mp4Event::NativeShortPacket(p)) => Some(TrackKey::Mp4(p.native.track_id)),
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
    pub fn native_pts(&self) -> Result<Option<Time>> {
        match self {
            Self::NativeTs { clock, .. } => clock.pts90.map(|t| Time::new(t, 90_000)).transpose(),
            Self::Mp4(Mp4Event::Packet(p)) => p
                .native
                .as_ref()
                .map(|n| {
                    n.demux
                        .pts
                        .map(|ticks| Time::from_ticks(i128::from(ticks), n.media_clock.demux_base))
                        .transpose()
                })
                .transpose()
                .map(Option::flatten),
            Self::Mp4(Mp4Event::NativeShortPacket(p)) => p
                .native
                .demux
                .pts
                .map(|ticks| Time::from_ticks(i128::from(ticks), p.native.media_clock.demux_base))
                .transpose(),
            Self::Ts(e) => e
                .native_clock
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
    pub fn payload_bytes(&self) -> usize {
        match self {
            Self::NativeTs { packet, .. } => packet.payload_len,
            Self::Matroska(MkvEvent::Packet(p)) => p.packet.data.len(),
            Self::Matroska(MkvEvent::Envelope(
                triergon_matroska::incremental::ElementEvent::Leaf { data, .. },
            )) => data.len(),
            Self::Mp4(Mp4Event::Packet(p)) => p.source.payload.len(),
            Self::Mp4(Mp4Event::NativeShortPacket(p)) => p.prefix.payload.len(),
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
            native_clock: Some(DemuxTimeView {
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
        let step = TsPayloadStep::project(TsPayloadStep::Event(event)).unwrap();
        assert_eq!(step.payload_bytes(), Some(0));
        let BackendStep::Event(RawEvent::Ts(event)) = BackendStep::project(step).unwrap() else {
            panic!("source event was not delivered");
        };
        assert_eq!(format!("{event:?}"), expected);

        let track = TrackKey::Ts {
            program: 7,
            pid: 257,
        };
        let error = Error::Unsupported("test carriage");
        let step = TsPayloadStep::project(TsPayloadStep::Refusal {
            track,
            error,
            source_event_ordinal: 109,
        })
        .unwrap();
        assert_eq!(step.payload_bytes(), Some(0));
        assert!(
            matches!(BackendStep::project(step), Ok(BackendStep::Event(RawEvent::NativeRefusal {
            track: actual_track, error: actual_error, source_event_ordinal: 109,
        })) if actual_track == track && actual_error == error)
        );
        assert_eq!(TsPayloadStep::End.payload_bytes(), None);
        assert_eq!(TsPayloadStep::AwaitEntity.payload_bytes(), None);
    }
}
