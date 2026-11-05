//! Complete, bounded AVC/AAC feed for one independent TS188 media segment.

use crate::{
    ClockChoice, Engine, Error, Player, PlayerPolicy, Progress, RawEvent, Result, Selection,
    Source, SourceReply, State, TrackKey,
    hls_aac::{HlsAac, HlsAacFrame},
    hls_avc::{AvcRaster, AvcTsAccessUnit, AvcTsLimits, AvcTsReader, AvcTsStep},
    hls_clock::{
        HlsAacClock, HlsParserOnlyAudioClock, parser_only_aac_clocks, parser_only_aac_pts_ns,
        resolve_hls_aac_clocks,
    },
    io::SourceOwner,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error as CoreError, InputCursor, InputRequest, WorkBudget};
use triergon_mpegts::{
    AssemblyStep, FaultKind, ParseLimits, TraceEvent,
    clock::SourceTimePolicy,
    psi::{DescriptorNamespace, ProgramMap},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HlsSegmentAudio {
    Disabled,
    #[default]
    Optional,
    Required,
    Pid(u16),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HlsSegmentTracks {
    pub video_pid: Option<u16>,
    pub audio: HlsSegmentAudio,
}

#[derive(Clone, Copy, Debug)]
pub struct HlsSegmentLimits {
    pub maximum_source_bytes: usize,
    pub maximum_feed_bytes: usize,
    pub maximum_access_units: usize,
    pub transport: ParseLimits,
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

#[derive(Debug)]
pub enum HlsFeedUnit<'a, C = HlsAacClock> {
    Video(&'a AvcTsAccessUnit),
    Audio(&'a HlsAacFrame, &'a C),
}

pub struct HlsSegment<C = HlsAacClock> {
    video: BudgetVec<AvcTsAccessUnit>,
    audio: BudgetVec<HlsAacFrame>,
    clocks: BudgetVec<C>,
    order: BudgetVec<Lane>,
    initial_raster: AvcRaster,
    video_tail90: i64,
    feed_bytes: usize,
}

impl HlsSegment {
    pub fn audio_tail90(&self) -> Option<i64> {
        self.clocks
            .as_slice()
            .last()
            .map(|clock| clock.ffmpeg90.pts_and_dts)
    }

    /// Presentation times for the AAC units in access_units order. This view
    /// matches the application path that fills durations missing from a
    /// parser-only AAC reference using the ADTS header.
    pub fn parser_only_audio_pts_ns(&self, cancel: &AtomicBool) -> Result<BudgetVec<i64>> {
        Ok(parser_only_aac_pts_ns(&self.audio, cancel)?)
    }
}

impl HlsSegment<HlsParserOnlyAudioClock> {
    pub fn audio_tail_ns(&self) -> Option<i64> {
        self.clocks.as_slice().last().map(|clock| clock.pts_ns)
    }
}

impl<C> HlsSegment<C> {
    pub fn initial_raster(&self) -> AvcRaster {
        self.initial_raster
    }

    pub fn video_tail90(&self) -> i64 {
        self.video_tail90
    }

    pub fn feed_bytes(&self) -> usize {
        self.feed_bytes
    }

    pub fn access_units(&self) -> impl ExactSizeIterator<Item = HlsFeedUnit<'_, C>> {
        let mut video = self.video.as_slice().iter();
        let mut audio = self.audio.as_slice().iter().zip(self.clocks.as_slice());
        self.order.as_slice().iter().map(move |lane| match lane {
            Lane::Video => HlsFeedUnit::Video(video.next().expect("sealed video order")),
            Lane::Audio => {
                let (frame, clock) = audio.next().expect("sealed audio order");
                HlsFeedUnit::Audio(frame, clock)
            }
        })
    }
}

#[derive(Debug)]
pub enum HlsSegmentProgress {
    Yield,
    NeedInput(InputRequest),
    Backpressure { requested: usize, available: usize },
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

enum Unit {
    Video(AvcTsAccessUnit),
    Audio(HlsAacFrame),
}

/// Acquire one independent media segment. Completed units stay private until
/// the transport has reported EOF and both assemblers have drained. A failed
/// attempt releases its source and staged output. To retry the transport,
/// construct a fresh reader with a new source owner.
pub struct HlsSegmentReader<S: Source> {
    pool: Budget,
    player: Option<Player<LimitedSource<S>>>,
    limits: HlsSegmentLimits,
    tracks: HlsSegmentTracks,
    map: Option<(TrackKey, Option<TrackKey>, u64)>,
    avc: Option<AvcTsReader>,
    aac: Option<HlsAac>,
    phase: Phase,
    pending: Option<Unit>,
    video: BudgetVec<AvcTsAccessUnit>,
    audio: BudgetVec<HlsAacFrame>,
    order: BudgetVec<Lane>,
    video_tail90: Option<i64>,
    feed_bytes: usize,
}

impl<S: Source> HlsSegmentReader<S> {
    pub fn new(
        input: InputCursor,
        source: S,
        tracks: HlsSegmentTracks,
        limits: HlsSegmentLimits,
    ) -> Result<Self> {
        let owner = SourceOwner::new(LimitedSource {
            source,
            maximum: limits.maximum_source_bytes,
        });
        if limits.maximum_source_bytes < 188
            || limits.maximum_feed_bytes == 0
            || limits.maximum_access_units == 0
        {
            return Err(CoreError::Invalid("HLS segment limits").into());
        }
        if let Some(length) = input.capabilities().length {
            check_extent(length, 0, limits.maximum_source_bytes)?;
        }
        let pool = input.budget().clone();
        let engine = Engine::ts_native_segment_v1(
            input,
            limits.transport,
            SourceTimePolicy::Unanchored,
            DescriptorNamespace::Unspecified,
        )?;
        let mut player = Player::new(
            engine,
            owner,
            Selection::All,
            PlayerPolicy {
                slots: 1,
                queued_payload_bytes: limits.maximum_source_bytes,
                ..PlayerPolicy::default()
            },
        )?;
        player.set_clock_view(ClockChoice::ExplicitNativeProjection)?;
        Ok(Self {
            player: Some(player),
            pool: pool.clone(),
            limits,
            tracks,
            map: None,
            avc: None,
            aac: None,
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
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        validate_reply(reply, self.limits.maximum_source_bytes)?;
        let result = self.player.as_mut().ok_or(Error::Closed)?.provide(reply);
        if matches!(
            result,
            Err(Error::Core(
                CoreError::Transport | CoreError::Truncated | CoreError::Cancelled
            ))
        ) {
            self.fail();
        }
        result
    }

    pub fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<HlsSegmentProgress> {
        if cancel.load(Ordering::Relaxed) {
            self.fail();
            return Err(CoreError::Cancelled.into());
        }
        match self.step(work, cancel) {
            Err(Error::Core(CoreError::Backpressure {
                requested,
                available,
            })) => Ok(HlsSegmentProgress::Backpressure {
                requested,
                available,
            }),
            Err(Error::Core(CoreError::Allocation))
                if !self
                    .player
                    .as_ref()
                    .is_some_and(|player| player.state() == State::Failed) =>
            {
                Err(CoreError::Allocation.into())
            }
            Err(error) => {
                self.fail();
                Err(error)
            }
            other => other,
        }
    }

    /// Returns the sealed segment once. Computing the audio clock is bounded
    /// by maximum_access_units. Pool pressure leaves the sealed data available.
    pub fn take(&mut self, cancel: &AtomicBool) -> Result<Option<HlsSegment>> {
        self.take_with(cancel, |reader| {
            resolve_hls_aac_clocks(
                &reader.pool,
                reader.audio.as_slice(),
                reader.limits.maximum_access_units,
                cancel,
            )
        })
    }

    /// Returns the sealed segment with the parser-only AAC delivery clock.
    /// Exact source clocks remain on each frame. Work and retry follow take.
    pub fn take_parser_only(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<Option<HlsSegment<HlsParserOnlyAudioClock>>> {
        self.take_with(cancel, |reader| {
            parser_only_aac_clocks(&reader.audio, cancel, |clock| clock)
        })
    }

    fn take_with<C>(
        &mut self,
        cancel: &AtomicBool,
        resolve: impl FnOnce(&Self) -> triergon_core::Result<BudgetVec<C>>,
    ) -> Result<Option<HlsSegment<C>>> {
        if cancel.load(Ordering::Relaxed) {
            self.fail();
            return Err(CoreError::Cancelled.into());
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
                if !matches!(
                    error,
                    CoreError::Backpressure { .. } | CoreError::Allocation
                ) {
                    self.fail();
                }
                return Err(error.into());
            }
        };
        let initial_raster = self.video.as_slice()[0].raster;
        let video_tail90 = self.video_tail90.expect("sealed video clock");
        self.phase = Phase::Taken;
        Ok(Some(HlsSegment {
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
                match player.advance(work, cancel)? {
                    Progress::Yield => {}
                    Progress::Queued => {
                        let output = player.pop().ok_or(CoreError::Invalid("HLS player queue"))?;
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
                        let avc = self
                            .avc
                            .as_mut()
                            .ok_or(CoreError::Invalid("HLS segment has no AVC configuration"))?;
                        avc.finish();
                        if let Some(aac) = &mut self.aac {
                            aac.finish();
                        }
                        self.phase = Phase::EndVideo;
                    }
                    _ => return Err(CoreError::Invalid("unexpected HLS player state").into()),
                }
            }
            Phase::Video | Phase::EndVideo => {
                let avc = self
                    .avc
                    .as_mut()
                    .ok_or(CoreError::Invalid("HLS AVC assembler unavailable"))?;
                match avc.step(work, cancel)? {
                    AvcTsStep::Ready(unit) => self.pending = Some(Unit::Video(unit)),
                    AvcTsStep::Yield => {}
                    AvcTsStep::NeedInput if self.phase == Phase::Video => self.phase = Phase::Read,
                    AvcTsStep::End if self.phase == Phase::EndVideo => {
                        self.avc = None;
                        self.phase = Phase::EndAudio;
                    }
                    _ => return Err(CoreError::Invalid("unexpected HLS AVC state").into()),
                }
            }
            Phase::Audio | Phase::EndAudio => {
                if let Some(aac) = &mut self.aac {
                    match aac.poll(work, cancel)? {
                        AssemblyStep::Output(unit) => self.pending = Some(Unit::Audio(unit)),
                        AssemblyStep::Yield => {}
                        AssemblyStep::NeedPayload if self.phase == Phase::Audio => {
                            self.phase = Phase::Read
                        }
                        AssemblyStep::Finished if self.phase == Phase::EndAudio => {
                            self.aac = None;
                            self.seal()?;
                        }
                        _ => return Err(CoreError::Invalid("unexpected HLS AAC state").into()),
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

    fn on_event(&mut self, event: RawEvent) -> Result<()> {
        match event {
            RawEvent::Ts(event) => match event.source {
                TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch,
                } => {
                    if self.map.is_some() {
                        return Err(CoreError::Unsupported(
                            "HLS segment changes active program map",
                        )
                        .into());
                    }
                    self.map = Some(select_tracks(&map, self.tracks, configuration_epoch)?);
                }
                TraceEvent::Diagnostic(fault)
                    if fault.kind == FaultKind::Transport
                        || fault.pid.is_some_and(|pid| self.selected_pid(pid)) =>
                {
                    return Err(fault.error.into());
                }
                _ => {}
            },
            RawEvent::NativeTsConfiguration(config) => {
                let (video, audio, epoch) = self
                    .map
                    .ok_or(CoreError::Invalid("HLS configuration precedes PMT"))?;
                if config.configuration_epoch != epoch {
                    return Err(CoreError::Invalid("HLS configuration epoch").into());
                }
                if config.track == video {
                    self.avc = Some(AvcTsReader::new(&self.pool, config, self.limits.avc)?);
                } else if Some(config.track) == audio {
                    self.aac = Some(HlsAac::new(&self.pool, config, 8191)?);
                }
            }
            RawEvent::NativeTs { packet, clock } => {
                let (video, audio, _) = self
                    .map
                    .ok_or(CoreError::Invalid("HLS packet precedes PMT"))?;
                let track = TrackKey::Ts {
                    program: packet.program,
                    pid: packet.pid,
                };
                let mut mailbox = Some(packet);
                if track == video {
                    let reader = self
                        .avc
                        .as_mut()
                        .ok_or(CoreError::Invalid("HLS AVC packet precedes configuration"))?;
                    if !reader.push(&mut mailbox, clock)? {
                        return Err(CoreError::Invalid("HLS AVC mailbox still busy").into());
                    }
                    self.phase = Phase::Video;
                } else if Some(track) == audio {
                    let reader = self
                        .aac
                        .as_mut()
                        .ok_or(CoreError::Invalid("HLS AAC packet precedes configuration"))?;
                    if !reader.push(&mut mailbox, clock)? {
                        return Err(CoreError::Invalid("HLS AAC mailbox still busy").into());
                    }
                    self.phase = Phase::Audio;
                }
            }
            RawEvent::NativeRefusal { track, error, .. }
                if self
                    .map
                    .is_some_and(|(video, audio, _)| track == video || Some(track) == audio) =>
            {
                return Err(error.into());
            }
            _ => {}
        }
        Ok(())
    }

    fn selected_pid(&self, pid: u16) -> bool {
        let Some((video, audio, _)) = self.map else {
            return false;
        };
        std::iter::once(video)
            .chain(audio)
            .any(|track| matches!(track, TrackKey::Ts { pid: current, .. } if current == pid))
    }

    fn commit_unit(&mut self) -> Result<()> {
        let unit = self
            .pending
            .as_ref()
            .ok_or(CoreError::Invalid("HLS pending unit"))?;
        let (lane, bytes, video_pts) = match unit {
            Unit::Video(unit) => (
                Lane::Video,
                unit.feed_spans().map(<[u8]>::len).sum(),
                Some(
                    unit.clock
                        .and_then(|clock| clock.pts90)
                        .ok_or(CoreError::Unsupported("HLS video picture without PTS"))?,
                ),
            ),
            Unit::Audio(frame) => (Lane::Audio, frame.bytes.len(), None),
        };
        let total = self
            .feed_bytes
            .checked_add(bytes)
            .ok_or(CoreError::Overflow)?;
        if total > self.limits.maximum_feed_bytes {
            return Err(CoreError::Limit {
                requested: total,
                maximum: self.limits.maximum_feed_bytes,
            }
            .into());
        }
        reserve_one(&mut self.order, self.limits.maximum_access_units)?;
        match lane {
            Lane::Video => reserve_one(&mut self.video, self.limits.maximum_access_units)?,
            Lane::Audio => reserve_one(&mut self.audio, self.limits.maximum_access_units)?,
        }
        match self
            .pending
            .take()
            .ok_or(CoreError::Invalid("HLS pending unit"))?
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
            return Err(CoreError::Invalid("HLS segment produced no video pictures").into());
        }
        if matches!(
            self.tracks.audio,
            HlsSegmentAudio::Required | HlsSegmentAudio::Pid(_)
        ) && self.audio.is_empty()
        {
            return Err(CoreError::Invalid("HLS segment produced no required AAC frames").into());
        }
        self.phase = Phase::Sealed;
        Ok(())
    }

    fn fail(&mut self) {
        self.player = None;
        self.avc = None;
        self.aac = None;
        self.pending = None;
        self.video = BudgetVec::new(&self.pool);
        self.audio = BudgetVec::new(&self.pool);
        self.order = BudgetVec::new(&self.pool);
        self.phase = Phase::Failed;
    }
}

fn select_tracks(
    map: &ProgramMap,
    selection: HlsSegmentTracks,
    epoch: u64,
) -> triergon_core::Result<(TrackKey, Option<TrackKey>, u64)> {
    let find = |stream_type, pid: Option<u16>| {
        let mut matches = map.streams.as_slice().iter().filter(|stream| {
            stream.stream_type == stream_type && pid.is_none_or(|pid| stream.pid == pid)
        });
        let first = matches.next().map(|stream| TrackKey::Ts {
            program: map.header.extension,
            pid: stream.pid,
        });
        if matches.next().is_some() {
            return Err(CoreError::Unsupported(
                "HLS segment requires explicit PID selection",
            ));
        }
        Ok(first)
    };
    let video = find(0x1b, selection.video_pid)?.ok_or(CoreError::Unsupported(
        "HLS segment has no selected AVC track",
    ))?;
    let audio = match selection.audio {
        HlsSegmentAudio::Disabled => None,
        HlsSegmentAudio::Optional => find(0x0f, None)?,
        HlsSegmentAudio::Required => Some(find(0x0f, None)?.ok_or(CoreError::Unsupported(
            "HLS segment has no required AAC track",
        ))?),
        HlsSegmentAudio::Pid(pid) => Some(find(0x0f, Some(pid))?.ok_or(CoreError::Unsupported(
            "HLS segment has no requested AAC PID",
        ))?),
    };
    Ok((video, audio, epoch))
}

fn reserve_one<T>(list: &mut BudgetVec<T>, maximum: usize) -> triergon_core::Result<()> {
    let needed = list.len().checked_add(1).ok_or(CoreError::Overflow)?;
    if needed > maximum {
        return Err(CoreError::Limit {
            requested: needed,
            maximum,
        });
    }
    if needed > list.capacity() {
        let target = list.capacity().saturating_mul(2).max(4).min(maximum);
        match list.try_reserve_exact(target - list.len()) {
            Err(CoreError::Backpressure { .. } | CoreError::Allocation) if target > needed => {
                list.try_reserve_exact(1)?
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
    ) -> triergon_core::Result<SourceReply> {
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

fn limit_request(request: &mut InputRequest, maximum: usize) -> triergon_core::Result<()> {
    check_extent(request.offset, 0, maximum)?;
    // At the bound, one byte distinguishes definitive EOF from excess input.
    request.maximum = request
        .maximum
        .min((maximum - request.offset as usize).max(1));
    Ok(())
}

fn validate_reply(reply: &SourceReply, maximum: usize) -> triergon_core::Result<()> {
    if let SourceReply::Data(window) = reply {
        check_extent(window.offset, window.bytes().len(), maximum)?;
    }
    Ok(())
}

fn check_extent(offset: u64, count: usize, maximum: usize) -> triergon_core::Result<()> {
    let end = offset
        .checked_add(count as u64)
        .ok_or(CoreError::Overflow)?;
    if end > maximum as u64 {
        return Err(CoreError::Limit {
            requested: usize::try_from(end).unwrap_or(usize::MAX),
            maximum,
        });
    }
    Ok(())
}
