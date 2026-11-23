//! Complete, bounded playback feed for one independent TS188 media segment.

use crate::{
    aac::{HlsAac, HlsAacFrame},
    avc::{AvcRaster, AvcTsAccessUnit, AvcTsLimits, AvcTsReader},
    clock::{HlsAacClock, HlsAudioClockNs, ns_aac_clocks, ns_audio_clocks, resolve_hls_aac_clocks},
    packet::UnitPacket,
    pmt::select_pid,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, InputCursor, InputRequest, Result, WorkBudget};
use triergon_mpegts::psi::{ElementaryStream, ProgramMap, descriptor_tag, stream_type};
use triergon_mpegts::stream::{FaultKind, TraceEvent};
use triergon_mpegts::{
    AssemblyStep, PacketFormat, ParseLimits,
    compat::pes::{PrivateAudioCarriage, PrivateAudioDescriptors},
};
use triergon_playback::compat::pes::AdmittedCarriage;
use triergon_playback::{
    ClockChoice, Event, Player, PlayerPolicy, Progress, Selection, Source, SourceOwner,
    SourceReply, State, TrackKey, pes_feed::SpentLists,
};

mod lanes;
use lanes::{AudioAssembler, LaneAssembler, VideoAssembler, VideoPicture};
pub use lanes::{HlsMediaAudioUnit, HlsMediaVideoUnit};

/// The AAC track an [`HlsSegmentReader`] reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HlsSegmentAudio {
    /// No audio.
    Disabled,
    /// The program's one AAC track, if it has one.
    #[default]
    Optional,
    /// The program's one AAC track; a segment without one fails with `Unsupported`.
    Required,
    /// The AAC track on this PID; a segment without it fails with `Unsupported`.
    Pid(u16),
}

/// Track selection, with explicit PIDs for ambiguous programs.
#[derive(Clone, Copy, Debug, Default)]
pub struct HlsSegmentTracks {
    /// The PID of the AVC track; `None` takes the program's one AVC track.
    pub video_pid: Option<u16>,
    /// The AAC track.
    pub audio: HlsSegmentAudio,
}

/// Bounds of one segment read.
#[derive(Clone, Copy, Debug)]
pub struct HlsSegmentLimits {
    /// The most source bytes (64 MiB by default); at least 188.
    pub maximum_source_bytes: usize,
    /// The most bytes of completed units (64 MiB by default); nonzero.
    pub maximum_feed_bytes: usize,
    /// The most access units of both tracks together (8,192 by default); nonzero.
    pub maximum_access_units: usize,
    /// Transport stream parse bounds.
    pub transport: ParseLimits,
    /// AVC access unit bounds.
    pub avc: AvcTsLimits,
}

impl Default for HlsSegmentLimits {
    fn default() -> Self {
        Self {
            maximum_source_bytes: 64 * 1024 * 1024,
            maximum_feed_bytes: 64 * 1024 * 1024,
            maximum_access_units: 8192,
            transport: ParseLimits::default(),
            avc: AvcTsLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Lane {
    Video,
    Audio,
}

/// The tracks a segment reads, as the active PMT of its program names them.
#[derive(Clone, Copy, Debug)]
struct ProgramTracks {
    program: u16,
    video: u16,
    audio: Option<u16>,
    configuration_epoch: u64,
}

impl ProgramTracks {
    /// The lane of the track on `pid` of `program`, if it is one of these.
    fn lane(self, program: u16, pid: u16) -> Option<Lane> {
        if program != self.program {
            None
        } else if pid == self.video {
            Some(Lane::Video)
        } else if Some(pid) == self.audio {
            Some(Lane::Audio)
        } else {
            None
        }
    }

    /// The lane of `track`, if it is one of these.
    fn lane_of(self, track: TrackKey) -> Option<Lane> {
        match track {
            TrackKey::Ts { program, pid } => self.lane(program, pid),
            _ => None,
        }
    }
}

/// One access unit of a segment.
#[derive(Debug)]
pub enum HlsFeedUnit<'a, C = HlsAacClock, V = AvcTsAccessUnit, A = HlsAacFrame> {
    /// A video unit.
    Video(&'a V),
    /// An audio frame with its segment clock.
    Audio(&'a A, &'a C),
}

/// The access units of one complete segment, in feed order, with their clocks.
#[derive(Debug)]
pub struct HlsSegment<C = HlsAacClock, V = AvcTsAccessUnit, A = HlsAacFrame> {
    video_pid: u16,
    audio_pid: Option<u16>,
    video: BudgetVec<V>,
    audio: BudgetVec<A>,
    clocks: BudgetVec<C>,
    order: BudgetVec<Lane>,
    initial_raster: AvcRaster,
    video_tail90: i64,
    feed_bytes: usize,
}

impl HlsSegment {
    /// The 90 kHz tick of the last audio frame, if any.
    pub fn audio_tail90(&self) -> Option<i64> {
        self.clocks
            .as_slice()
            .last()
            .map(|clock| clock.demux90.pts_and_dts)
    }

    /// Borrows this segment's unit as a packet with its PID and segment clock.
    /// Fails with `Invalid` for audio when the segment has no audio track,
    /// or `Overflow` when a timestamp cannot fit its packet clock.
    pub fn packet<'a>(&self, unit: &HlsFeedUnit<'a>) -> Result<UnitPacket<'a>> {
        match *unit {
            HlsFeedUnit::Video(unit) => UnitPacket::video(self.video_pid, unit),
            HlsFeedUnit::Audio(frame, clock) => UnitPacket::audio(
                self.audio_pid
                    .ok_or(Error::Invalid("HLS segment has no audio track"))?,
                frame,
                clock.pts,
                Some(clock.dts),
            ),
        }
    }
}

impl<V, A> HlsSegment<HlsAudioClockNs, V, A> {
    /// The presentation time of the last audio frame in nanoseconds, if any.
    pub fn audio_tail_ns(&self) -> Option<i64> {
        self.clocks.as_slice().last().map(|clock| clock.pts_ns)
    }
}

impl<C, V, A> HlsSegment<C, V, A> {
    /// The raster of the first video unit.
    pub fn initial_raster(&self) -> AvcRaster {
        self.initial_raster
    }

    /// The latest video presentation time, in 90 kHz ticks.
    pub fn video_tail90(&self) -> i64 {
        self.video_tail90
    }

    /// The bytes of every unit together.
    pub fn feed_bytes(&self) -> usize {
        self.feed_bytes
    }

    /// The units in feed order.
    pub fn access_units(&self) -> impl ExactSizeIterator<Item = HlsFeedUnit<'_, C, V, A>> {
        FeedUnits {
            order: self.order.as_slice().iter(),
            video: self.video.as_slice().iter(),
            audio: self.audio.as_slice().iter().zip(self.clocks.as_slice()),
        }
    }
}

/// The units of a segment in feed order: its lane order names, one by one, the next
/// stored unit of each lane.
struct FeedUnits<'a, C, V, A> {
    order: std::slice::Iter<'a, Lane>,
    video: std::slice::Iter<'a, V>,
    audio: std::iter::Zip<std::slice::Iter<'a, A>, std::slice::Iter<'a, C>>,
}

impl<'a, C, V, A> Iterator for FeedUnits<'a, C, V, A> {
    type Item = HlsFeedUnit<'a, C, V, A>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(match self.order.next()? {
            Lane::Video => HlsFeedUnit::Video(self.video.next()?),
            Lane::Audio => {
                let (frame, clock) = self.audio.next()?;
                HlsFeedUnit::Audio(frame, clock)
            }
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.order.size_hint()
    }
}

impl<C, V, A> ExactSizeIterator for FeedUnits<'_, C, V, A> {}

/// One step of a segment reader.
#[derive(Debug)]
pub enum HlsSegmentProgress {
    /// The work budget ran out: advance again with more work.
    Yield,
    /// The source is pending on this request: provide a reply or advance again.
    NeedInput(InputRequest),
    /// The pool is full of output the caller still holds: release some, then
    /// advance again.
    Backpressure {
        /// Bytes the step needed.
        requested: usize,
        /// Bytes the pool had free.
        available: usize,
    },
    /// The segment is complete: take it.
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Read,
    Video,
    Audio,
    EndVideo,
    EndAudio,
    Sealed,
    Taken,
    Failed,
}

enum Unit<V, A> {
    Video(V),
    Audio(A),
}

type AssembledSegment<C, V, A> =
    HlsSegment<C, <V as LaneAssembler>::Unit, <A as LaneAssembler>::Unit>;

/// Reads an independent segment through EOF and assembler drain.
/// Failure releases the source and staged units. Retry with a fresh reader.
#[derive(Debug)]
pub struct HlsSegmentReader<S: Source> {
    inner: SegmentReader<S, AvcTsReader, HlsAac>,
}

/// The video codec of a media segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HlsVideoCodec {
    /// H.264, stream type 0x1b.
    Avc,
    /// H.265, stream type 0x24.
    Hevc,
}

/// The audio codec of a media segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HlsAudioCodec {
    /// ADTS AAC.
    Aac,
    /// AC-3.
    Ac3,
    /// E-AC-3.
    Eac3,
}

/// The negotiated codecs for one independent segment. A supplied audio codec
/// requires one unambiguous matching track and at least one complete frame.
#[derive(Clone, Copy, Debug)]
pub struct HlsMediaProfile {
    /// The video codec; the program has one track of it.
    pub video: HlsVideoCodec,
    /// The audio codec, or `None` to read no audio.
    pub audio: Option<HlsAudioCodec>,
}

/// The units of one media segment, with nanosecond audio clocks.
pub type HlsMediaSegment = HlsSegment<HlsAudioClockNs, HlsMediaVideoUnit, HlsMediaAudioUnit>;

/// Reads one complete media segment: AVC or HEVC video and optional AAC, AC-3 or
/// E-AC-3 audio, as [`HlsSegmentReader`] does.
#[derive(Debug)]
pub struct HlsMediaSegmentReader<S: Source> {
    inner: SegmentReader<S, VideoAssembler, AudioAssembler>,
}

#[derive(Clone, Copy)]
enum TrackSelection {
    AvcAac(HlsSegmentTracks),
    Media(HlsMediaProfile),
}

impl TrackSelection {
    fn requires_audio(self) -> bool {
        match self {
            Self::AvcAac(tracks) => {
                matches!(
                    tracks.audio,
                    HlsSegmentAudio::Required | HlsSegmentAudio::Pid(_)
                )
            }
            Self::Media(profile) => profile.audio.is_some(),
        }
    }
}

struct SegmentReader<S: Source, V: LaneAssembler, A: LaneAssembler> {
    pool: Budget,
    player: Option<Player<LimitedSource<S>>>,
    limits: HlsSegmentLimits,
    selection: TrackSelection,
    tracks: Option<ProgramTracks>,
    video_assembler: Option<V>,
    audio_assembler: Option<A>,
    phase: Phase,
    pending: Option<Unit<V::Unit, A::Unit>>,
    video: BudgetVec<V::Unit>,
    audio: BudgetVec<A::Unit>,
    order: BudgetVec<Lane>,
    video_tail90: Option<i64>,
    feed_bytes: usize,
}

impl<S: Source, V: LaneAssembler, A: LaneAssembler> std::fmt::Debug for SegmentReader<S, V, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentReader")
            .field("phase", &self.phase)
            .field("limits", &self.limits)
            .field("feed_bytes", &self.feed_bytes)
            .finish_non_exhaustive()
    }
}

impl<S: Source> HlsSegmentReader<S> {
    /// Fails with `Invalid` for invalid limits, or `Limit` for known lengths
    /// above `maximum_source_bytes`.
    pub fn new(
        input: InputCursor,
        source: S,
        tracks: HlsSegmentTracks,
        limits: HlsSegmentLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: SegmentReader::new(input, source, TrackSelection::AvcAac(tracks), limits)?,
        })
    }

    /// Applies a source reply. Returns `false` for a stale reply.
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        self.inner.provide(reply)
    }

    /// Reads and assembles within `work`. A failure other than backpressure retires the
    /// reader.
    pub fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<HlsSegmentProgress> {
        self.inner.advance(work, cancel)
    }

    /// Computes the original AAC demux clock without changing source facts.
    pub fn take(&mut self, cancel: &AtomicBool) -> Result<Option<HlsSegment>> {
        self.inner.take_with(cancel, |reader| {
            resolve_hls_aac_clocks(
                &reader.pool,
                reader.audio.as_slice(),
                reader.limits.maximum_access_units,
                cancel,
            )
        })
    }

    /// The completed segment with nanosecond audio clocks; `None` until it is complete
    /// and after it was taken. Fails with `Closed` after a failure.
    pub fn take_with_ns_clocks(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<Option<HlsSegment<HlsAudioClockNs>>> {
        self.inner
            .take_with(cancel, |reader| ns_aac_clocks(&reader.audio, cancel))
    }
}

impl<S: Source> HlsMediaSegmentReader<S> {
    /// A reader of one segment from `source`. Fails as [`HlsSegmentReader::new`] does.
    pub fn new(
        input: InputCursor,
        source: S,
        profile: HlsMediaProfile,
        limits: HlsSegmentLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: SegmentReader::new(input, source, TrackSelection::Media(profile), limits)?,
        })
    }

    /// Applies a source reply. Returns `false` for a stale reply.
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        self.inner.provide(reply)
    }

    /// Reads and assembles within `work`. A failure other than backpressure retires the
    /// reader.
    pub fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<HlsSegmentProgress> {
        self.inner.advance(work, cancel)
    }

    /// The completed segment; `None` until it is complete and after it was taken. Fails
    /// with `Closed` after a failure.
    pub fn take(&mut self, cancel: &AtomicBool) -> Result<Option<HlsMediaSegment>> {
        self.inner.take_with(cancel, |reader| {
            ns_audio_clocks(&reader.audio, cancel, |frame| {
                (frame.pts.or(frame.dts), frame.duration)
            })
        })
    }
}

impl<S: Source, V: LaneAssembler, A: LaneAssembler> SegmentReader<S, V, A>
where
    V::Unit: VideoPicture,
{
    fn new(
        input: InputCursor,
        source: S,
        selection: TrackSelection,
        limits: HlsSegmentLimits,
    ) -> Result<Self> {
        let owner = SourceOwner::new(LimitedSource {
            source,
            maximum: limits.maximum_source_bytes,
        });
        if limits.maximum_source_bytes < PacketFormat::Ts188.bytes()
            || limits.maximum_feed_bytes == 0
            || limits.maximum_access_units == 0
        {
            return Err(Error::Invalid("HLS segment limits"));
        }
        if let Some(length) = input.capabilities().length {
            check_extent(length, 0, limits.maximum_source_bytes)?;
        }
        let pool = input.budget().clone();
        let engine = triergon_playback::pes_feed::open_segment_feed(
            input,
            limits.transport,
            matches!(selection, TrackSelection::Media(_)),
        )?;
        let mut player = Player::new_direct(
            engine,
            owner,
            Selection::All,
            PlayerPolicy {
                slots: 1,
                queued_payload_bytes: limits.maximum_source_bytes,
                ..PlayerPolicy::default()
            },
        )?;
        player.set_clock_view(ClockChoice::Demux)?;
        Ok(Self {
            player: Some(player),
            pool: pool.clone(),
            limits,
            selection,
            tracks: None,
            video_assembler: None,
            audio_assembler: None,
            phase: Phase::Read,
            pending: None,
            video: BudgetVec::new(&pool),
            audio: BudgetVec::new(&pool),
            order: BudgetVec::new(&pool),
            video_tail90: None,
            feed_bytes: 0,
        })
    }

    /// The caller may supply an asynchronous reply here or let Source::poll
    /// provide it. Stale replies leave the active attempt intact.
    fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        validate_reply(reply, self.limits.maximum_source_bytes)?;
        let result = self.player.as_mut().ok_or(Error::Closed)?.provide(reply);
        if matches!(
            result,
            Err(Error::Transport | Error::Truncated | Error::Cancelled)
        ) {
            self.fail();
        }
        result
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<HlsSegmentProgress> {
        if cancel.load(Ordering::Relaxed) {
            self.fail();
            return Err(Error::Cancelled);
        }
        match self.step(work, cancel) {
            Err(Error::Backpressure(triergon_core::Backpressure {
                requested,
                available,
            })) => Ok(HlsSegmentProgress::Backpressure {
                requested,
                available,
            }),
            Err(Error::Allocation)
                if !self
                    .player
                    .as_ref()
                    .is_some_and(|player| player.state() == State::Failed) =>
            {
                Err(Error::Allocation)
            }
            Err(error) => {
                self.fail();
                Err(error)
            }
            other => other,
        }
    }

    fn take_with<C>(
        &mut self,
        cancel: &AtomicBool,
        resolve: impl FnOnce(&Self) -> Result<BudgetVec<C>>,
    ) -> Result<Option<AssembledSegment<C, V, A>>> {
        if cancel.load(Ordering::Relaxed) {
            self.fail();
            return Err(Error::Cancelled);
        }
        if self.phase == Phase::Failed {
            return Err(Error::Closed);
        }
        if self.phase != Phase::Sealed {
            return Ok(None);
        }
        let clocks = match resolve(self) {
            Ok(clocks) => clocks,
            Err(error) => {
                if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                    self.fail();
                }
                return Err(error);
            }
        };
        let initial_raster = self.video.as_slice()[0].raster();
        let video_tail90 = self
            .video_tail90
            .ok_or(Error::Invalid("sealed video clock"))?;
        let tracks = self.tracks.ok_or(Error::Invalid("sealed segment tracks"))?;
        self.phase = Phase::Taken;
        Ok(Some(HlsSegment {
            video_pid: tracks.video,
            audio_pid: tracks.audio,
            initial_raster,
            video_tail90,
            feed_bytes: self.feed_bytes,
            video: std::mem::replace(&mut self.video, BudgetVec::new(&self.pool)),
            audio: std::mem::replace(&mut self.audio, BudgetVec::new(&self.pool)),
            order: std::mem::replace(&mut self.order, BudgetVec::new(&self.pool)),
            clocks,
        }))
    }

    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<HlsSegmentProgress> {
        if self.pending.is_some() {
            if work.try_charge(1) {
                self.commit_unit()?;
            }
            return Ok(HlsSegmentProgress::Yield);
        }
        match self.phase {
            Phase::Read => {
                let player = self.player.as_mut().ok_or(Error::Closed)?;
                let mut slot = None;
                match player.read_output(&mut slot, work, cancel)? {
                    Progress::Yield => {}
                    Progress::Queued => {
                        let output = slot.ok_or(Error::Invalid("HLS player output"))?;
                        self.on_event(output.into_event())?;
                    }
                    Progress::NeedInput(mut request) => {
                        limit_request(&mut request, self.limits.maximum_source_bytes)?;
                        return Ok(HlsSegmentProgress::NeedInput(request));
                    }
                    Progress::Backpressure {
                        requested,
                        available,
                    } => {
                        return Ok(HlsSegmentProgress::Backpressure {
                            requested,
                            available,
                        });
                    }
                    Progress::Ended => {
                        self.player = None;
                        let video = self
                            .video_assembler
                            .as_mut()
                            .ok_or(Error::Invalid("HLS segment has no video configuration"))?;
                        video.finish();
                        if let Some(audio) = &mut self.audio_assembler {
                            audio.finish();
                        }
                        self.phase = Phase::EndVideo;
                    }
                    _ => return Err(Error::Invalid("unexpected HLS player state")),
                }
            }
            Phase::Video | Phase::EndVideo => {
                let video = self
                    .video_assembler
                    .as_mut()
                    .ok_or(Error::Invalid("HLS video assembler unavailable"))?;
                match video.poll(work, cancel)? {
                    AssemblyStep::Output(unit) => self.pending = Some(Unit::Video(unit)),
                    AssemblyStep::Yield => {}
                    AssemblyStep::NeedPayload if self.phase == Phase::Video => {
                        let lists = video.take_spent();
                        self.reuse_pes_lists(self.tracks.map(|tracks| tracks.video), lists);
                        self.phase = Phase::Read;
                    }
                    AssemblyStep::Finished if self.phase == Phase::EndVideo => {
                        self.video_assembler = None;
                        self.phase = Phase::EndAudio;
                    }
                    _ => return Err(Error::Invalid("unexpected HLS video state")),
                }
            }
            Phase::Audio | Phase::EndAudio => {
                if let Some(audio) = &mut self.audio_assembler {
                    match audio.poll(work, cancel)? {
                        AssemblyStep::Output(unit) => self.pending = Some(Unit::Audio(unit)),
                        AssemblyStep::Yield => {}
                        AssemblyStep::NeedPayload if self.phase == Phase::Audio => {
                            let lists = audio.take_spent();
                            self.reuse_pes_lists(
                                self.tracks.and_then(|tracks| tracks.audio),
                                lists,
                            );
                            self.phase = Phase::Read;
                        }
                        AssemblyStep::Finished if self.phase == Phase::EndAudio => {
                            self.audio_assembler = None;
                            self.seal()?;
                        }
                        _ => return Err(Error::Invalid("unexpected HLS audio state")),
                    }
                } else {
                    self.seal()?;
                }
            }
            Phase::Sealed => return Ok(HlsSegmentProgress::Ready),
            Phase::Taken | Phase::Failed => return Err(Error::Closed),
        }
        if self.phase == Phase::Sealed {
            Ok(HlsSegmentProgress::Ready)
        } else {
            Ok(HlsSegmentProgress::Yield)
        }
    }

    /// The observer of a track allocates nothing for its next PES when it gets
    /// back the lists of the one a reader has just consumed.
    fn reuse_pes_lists(&mut self, pid: Option<u16>, lists: SpentLists) {
        if let (Some(player), Some(pid)) = (self.player.as_mut(), pid) {
            player.reuse_pes_lists(pid, lists);
        }
    }

    fn on_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Ts(event) => match event.source {
                TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch,
                } => self.bind_program(&map, configuration_epoch)?,
                TraceEvent::Diagnostic(fault)
                    if fault.kind == FaultKind::Transport
                        || fault.pid.is_some_and(|pid| self.selected_pid(pid)) =>
                {
                    return Err(fault.error);
                }
                _ => {}
            },
            Event::PesCarriage(config) => self.configure(config)?,
            Event::DemuxPes { packet, clock } => {
                let tracks = self
                    .tracks
                    .ok_or(Error::Invalid("HLS packet precedes PMT"))?;
                let lane = tracks.lane(packet.program, packet.pid);
                let mut mailbox = Some(packet);
                match lane {
                    Some(Lane::Video) => {
                        let reader = self
                            .video_assembler
                            .as_mut()
                            .ok_or(Error::Invalid("HLS video packet precedes configuration"))?;
                        if !reader.push(&mut mailbox, clock)? {
                            return Err(Error::Invalid("HLS video mailbox still busy"));
                        }
                        self.phase = Phase::Video;
                    }
                    Some(Lane::Audio) => {
                        let reader = self
                            .audio_assembler
                            .as_mut()
                            .ok_or(Error::Invalid("HLS audio packet precedes configuration"))?;
                        if !reader.push(&mut mailbox, clock)? {
                            return Err(Error::Invalid("HLS audio mailbox still busy"));
                        }
                        self.phase = Phase::Audio;
                    }
                    None => {}
                }
            }
            Event::PesRefusal { track, error, .. }
                if self
                    .tracks
                    .is_some_and(|tracks| tracks.lane_of(track).is_some()) =>
            {
                return Err(error);
            }
            _ => {}
        }
        Ok(())
    }

    /// Selects the tracks of the program's PMT, once per segment.
    #[cold]
    fn bind_program(&mut self, map: &ProgramMap, configuration_epoch: u64) -> Result<()> {
        if self.tracks.is_some() {
            return Err(Error::Unsupported("HLS segment changes active program map"));
        }
        self.tracks = Some(select_tracks(
            map,
            self.selection,
            configuration_epoch,
            self.limits.transport,
        )?);
        Ok(())
    }

    /// Makes the assembler of a configured track, once per track.
    #[cold]
    fn configure(&mut self, config: AdmittedCarriage) -> Result<()> {
        let tracks = self
            .tracks
            .ok_or(Error::Invalid("HLS configuration precedes PMT"))?;
        if config.configuration_epoch != tracks.configuration_epoch {
            return Err(Error::Invalid("HLS configuration epoch"));
        }
        match tracks.lane_of(config.track) {
            Some(Lane::Video) => {
                self.video_assembler = Some(V::bind(&self.pool, config, self.limits)?);
            }
            Some(Lane::Audio) => {
                self.audio_assembler = Some(A::bind(&self.pool, config, self.limits)?);
            }
            None => {}
        }
        Ok(())
    }

    fn selected_pid(&self, pid: u16) -> bool {
        self.tracks
            .is_some_and(|tracks| tracks.video == pid || tracks.audio == Some(pid))
    }

    fn commit_unit(&mut self) -> Result<()> {
        let unit = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("HLS pending unit"))?;
        let (lane, bytes, video_pts) = match unit {
            Unit::Video(unit) => (
                Lane::Video,
                V::unit_bytes(unit),
                Some(
                    unit.pts90()
                        .ok_or(Error::Unsupported("HLS video picture without PTS"))?,
                ),
            ),
            Unit::Audio(frame) => (Lane::Audio, A::unit_bytes(frame), None),
        };
        let total = self.feed_bytes.checked_add(bytes).ok_or(Error::Overflow)?;
        if total > self.limits.maximum_feed_bytes {
            return Err(Error::Limit {
                requested: total,
                maximum: self.limits.maximum_feed_bytes,
            });
        }
        reserve_one(&mut self.order, self.limits.maximum_access_units)?;
        match lane {
            Lane::Video => reserve_one(&mut self.video, self.limits.maximum_access_units)?,
            Lane::Audio => reserve_one(&mut self.audio, self.limits.maximum_access_units)?,
        }
        match self
            .pending
            .take()
            .ok_or(Error::Invalid("HLS pending unit"))?
        {
            Unit::Video(unit) => self.video.try_push(unit).map_err(|error| error.error)?,
            Unit::Audio(unit) => self.audio.try_push(unit).map_err(|error| error.error)?,
        }
        self.order.try_push(lane).map_err(|error| error.error)?;
        self.video_tail90 = self.video_tail90.max(video_pts);
        self.feed_bytes = total;
        Ok(())
    }

    fn seal(&mut self) -> Result<()> {
        if self.video.is_empty() {
            return Err(Error::Invalid("HLS segment produced no video pictures"));
        }
        if self.selection.requires_audio() && self.audio.is_empty() {
            return Err(Error::Invalid(
                "HLS segment produced no required audio frames",
            ));
        }
        self.phase = Phase::Sealed;
        Ok(())
    }

    fn fail(&mut self) {
        self.player = None;
        self.video_assembler = None;
        self.audio_assembler = None;
        self.pending = None;
        self.video = BudgetVec::new(&self.pool);
        self.audio = BudgetVec::new(&self.pool);
        self.order = BudgetVec::new(&self.pool);
        self.phase = Phase::Failed;
    }
}

fn select_tracks(
    map: &ProgramMap,
    selection: TrackSelection,
    configuration_epoch: u64,
    limits: ParseLimits,
) -> Result<ProgramTracks> {
    const AMBIGUOUS: &str = "HLS segment requires explicit PID selection";
    let (video, audio) = match selection {
        TrackSelection::AvcAac(tracks) => {
            let video = select_pid(
                map,
                tracks.video_pid,
                |stream| Ok(stream.stream_type == stream_type::H264),
                AMBIGUOUS,
            )?
            .ok_or(Error::Unsupported("HLS segment has no selected AVC track"))?;
            let aac = |pid| {
                select_pid(
                    map,
                    pid,
                    |stream| Ok(stream.stream_type == stream_type::AAC_ADTS),
                    AMBIGUOUS,
                )
            };
            let audio = match tracks.audio {
                HlsSegmentAudio::Disabled => None,
                HlsSegmentAudio::Optional => aac(None)?,
                HlsSegmentAudio::Required => Some(
                    aac(None)?
                        .ok_or(Error::Unsupported("HLS segment has no required AAC track"))?,
                ),
                HlsSegmentAudio::Pid(pid) => Some(
                    aac(Some(pid))?
                        .ok_or(Error::Unsupported("HLS segment has no requested AAC PID"))?,
                ),
            };
            (video, audio)
        }
        TrackSelection::Media(profile) => {
            let video_type = match profile.video {
                HlsVideoCodec::Avc => stream_type::H264,
                HlsVideoCodec::Hevc => stream_type::HEVC,
            };
            let video = select_pid(
                map,
                None,
                |stream| Ok(stream.stream_type == video_type),
                AMBIGUOUS,
            )?
            .ok_or(Error::Unsupported(
                "HLS segment has no matching video track",
            ))?;
            let audio = if let Some(codec) = profile.audio {
                Some(
                    select_pid(
                        map,
                        None,
                        |stream| media_audio_matches(map, stream, codec, limits),
                        AMBIGUOUS,
                    )?
                    .ok_or(Error::Unsupported(
                        "HLS segment has no matching audio track",
                    ))?,
                )
            } else {
                None
            };
            (video, audio)
        }
    };
    Ok(ProgramTracks {
        program: map.header.extension,
        video,
        audio,
        configuration_epoch,
    })
}

fn media_audio_matches(
    map: &ProgramMap,
    stream: &ElementaryStream,
    codec: HlsAudioCodec,
    limits: ParseLimits,
) -> Result<bool> {
    use PrivateAudioCarriage as Dolby;
    if codec == HlsAudioCodec::Aac {
        return Ok(stream.stream_type == stream_type::AAC_ADTS);
    }
    match stream.stream_type {
        stream_type::ATSC_AC3 if codec == HlsAudioCodec::Ac3 => {}
        stream_type::ATSC_EAC3 if codec == HlsAudioCodec::Eac3 => {}
        stream_type::PES_PRIVATE_DATA
            if stream.descriptors().any(|entry| {
                entry.is_ok_and(|descriptor| {
                    matches!(
                        descriptor.tag,
                        descriptor_tag::DVB_AC3 | descriptor_tag::DVB_EAC3
                    )
                })
            }) => {}
        _ => return Ok(false),
    }
    let carriage = Dolby::from_hls_pmt(
        stream.stream_type,
        PrivateAudioDescriptors {
            program: map.program_descriptors.as_ref(),
            elementary: stream.descriptor_bytes.as_ref(),
        },
        limits,
    )?;
    Ok(matches!(
        (codec, carriage),
        (
            HlsAudioCodec::Ac3,
            Dolby::DvbAc3Tag | Dolby::PrivateType81Ac3
        ) | (
            HlsAudioCodec::Eac3,
            Dolby::DvbEac3Tag | Dolby::PrivateType87Eac3
        )
    ))
}

fn reserve_one<T>(list: &mut BudgetVec<T>, maximum: usize) -> Result<()> {
    let needed = list.len().checked_add(1).ok_or(Error::Overflow)?;
    if needed > maximum {
        return Err(Error::Limit {
            requested: needed,
            maximum,
        });
    }
    if needed > list.capacity() {
        let target = list.capacity().saturating_mul(2).max(4).min(maximum);
        match list.try_reserve_exact(target - list.len()) {
            Err(Error::Backpressure { .. } | Error::Allocation) if target > needed => {
                list.try_reserve_exact(1)?;
            }
            other => other?,
        }
    }
    Ok(())
}

struct LimitedSource<S> {
    source: S,
    maximum: usize,
}

impl<S: Source> Source for LimitedSource<S> {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SourceReply> {
        check_extent(request.offset, 0, self.maximum)?;
        let mut bounded;
        let request = if request.maximum > (self.maximum - request.offset as usize).max(1) {
            bounded = request.clone();
            limit_request(&mut bounded, self.maximum)?;
            &bounded
        } else {
            request
        };
        let reply = self.source.poll(request, work, cancel)?;
        validate_reply(&reply, self.maximum)?;
        Ok(reply)
    }

    fn invalidate(&mut self) {
        self.source.invalidate();
    }

    fn abort(&mut self) {
        self.source.abort();
    }

    fn close(&mut self) {
        self.source.close();
    }
}

fn limit_request(request: &mut InputRequest, maximum: usize) -> Result<()> {
    check_extent(request.offset, 0, maximum)?;
    // At the bound, one byte distinguishes definitive EOF from excess input.
    request.maximum = request
        .maximum
        .min((maximum - request.offset as usize).max(1));
    Ok(())
}

fn validate_reply(reply: &SourceReply, maximum: usize) -> Result<()> {
    if let SourceReply::Data(window) = reply {
        check_extent(window.offset, window.bytes().len(), maximum)?;
    }
    Ok(())
}

fn check_extent(offset: u64, count: usize, maximum: usize) -> Result<()> {
    let end = offset.checked_add(count as u64).ok_or(Error::Overflow)?;
    if end > maximum as u64 {
        return Err(Error::Limit {
            requested: usize::try_from(end).unwrap_or(usize::MAX),
            maximum,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_core::{Bytes, SourceCapabilities, SourceId};
    use triergon_playback::ByteSource;

    #[test]
    fn borrowed_media_spans_match_contiguous_feed_after_reader_drop() {
        let fixture = include_bytes!("../../playback/tests/fixtures/hls/baseline-stereo.ts");
        let pool = Budget::new(1024 * 1024).unwrap();
        let source = SourceId(1);
        let cursor = InputCursor::new(
            &pool,
            source,
            SourceCapabilities {
                length: Some(fixture.len() as u64),
                random_access: false,
            },
            4096,
        )
        .unwrap();
        let bytes = Bytes::copy_from_slice(&pool, fixture).unwrap();
        let mut reader = HlsMediaSegmentReader::new(
            cursor,
            ByteSource::new(source, bytes, 4096).unwrap(),
            HlsMediaProfile {
                video: HlsVideoCodec::Avc,
                audio: Some(HlsAudioCodec::Aac),
            },
            HlsSegmentLimits::default(),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        for _ in 0..10_000 {
            if matches!(
                reader.advance(&mut WorkBudget::new(4096), &cancel).unwrap(),
                HlsSegmentProgress::Ready
            ) {
                break;
            }
        }
        let segment = reader.take(&cancel).unwrap().unwrap();
        drop(reader);
        let foreign_pool = Budget::new(4096).unwrap();
        let mut video_units = 0;
        for unit in segment.access_units() {
            if let HlsFeedUnit::Video(unit) = unit {
                let borrowed = unit.feed_spans().flatten().copied().collect::<Vec<_>>();
                assert_eq!(borrowed, unit.contiguous_feed(&pool).unwrap().as_ref());
                assert!(matches!(
                    unit.contiguous_feed(&foreign_pool),
                    Err(Error::ForeignPool)
                ));
                video_units += 1;
            }
        }
        assert!(video_units > 0);
        drop(segment);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn suppressed_source_events_still_charge_the_player_event_limit() {
        let fixture = include_bytes!("../../playback/tests/fixtures/hls/baseline-stereo.ts");
        let pool = Budget::new(1024 * 1024).unwrap();
        let source = SourceId(1);
        let cursor = InputCursor::new(
            &pool,
            source,
            SourceCapabilities {
                length: Some(fixture.len() as u64),
                random_access: false,
            },
            4096,
        )
        .unwrap();
        let bytes = Bytes::copy_from_slice(&pool, fixture).unwrap();
        let engine =
            triergon_playback::pes_feed::open_segment_feed(cursor, ParseLimits::default(), false)
                .unwrap();
        let mut player = Player::new(
            engine,
            SourceOwner::new(ByteSource::new(source, bytes, 4096).unwrap()),
            Selection::All,
            PlayerPolicy {
                maximum_events: 1,
                ..PlayerPolicy::default()
            },
        )
        .unwrap();
        let mut slot = None;
        assert!(matches!(
            player.read_output(
                &mut slot,
                &mut WorkBudget::new(4096),
                &AtomicBool::new(false)
            ),
            Err(Error::Limit {
                requested: 2,
                maximum: 1
            })
        ));
        assert!(slot.is_none());
    }
}
