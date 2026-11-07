use crate::input::{Source, SourceReply};
use crate::{BackendStep, Engine, RawEvent, Result, io::SourceOwner};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, InputRequest, Time, WorkBudget};
use triergon_isobmff::compat::mov::packets::PacketCursor;

mod mp4_native_seek;
mod mp4_range;
mod mp4_sync_seek;
pub use mp4_native_seek::{Mp4NativeSeekCommand, Mp4NativeSeekJob, Mp4NativeSeekRequest};
pub use mp4_range::{Mp4RangeOpen, Mp4RangePlayer, Mp4RangeSeek, Mp4RangeSeekJob};
pub use mp4_sync_seek::{Mp4SyncSeekCommand, Mp4SyncSeekJob, Mp4SyncSeekRequest};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKey {
    Matroska(u64),
    Mp4(u32),
    Ts { program: u16, pid: u16 },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    Other,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    All,
    Tracks {
        video: Option<TrackKey>,
        audio: Option<TrackKey>,
        subtitle: Option<TrackKey>,
    },
}
impl Selection {
    fn includes(self, key: TrackKey) -> bool {
        match self {
            Self::All => true,
            Self::Tracks {
                video,
                audio,
                subtitle,
            } => [video, audio, subtitle].contains(&Some(key)),
        }
    }
    fn validate(self, engine: &Engine) -> Result<()> {
        let Self::Tracks {
            video,
            audio,
            subtitle,
        } = self
        else {
            return Ok(());
        };
        let facts = engine.facts()?;
        for (key, kind) in [
            (video, TrackKind::Video),
            (audio, TrackKind::Audio),
            (subtitle, TrackKind::Subtitle),
        ] {
            if let Some(key) = key
                && !facts
                    .as_slice()
                    .iter()
                    .any(|f| f.key == key && f.kind == kind && f.enabled)
            {
                return Err(Error::Invalid(
                    "selected source track missing/disabled/kind",
                ));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekIntent {
    pub track: TrackKey,
    pub target: Time,
    pub replay_from_start: bool,
    pub clock: ClockChoice,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockChoice {
    SourceExact,
    ExplicitNativeProjection,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Source,
    Unselected,
    DecodeOnly { until: Time },
    Present,
    UnknownTime,
}
pub(crate) struct OutputScope {
    source: triergon_core::SourceId,
    pool: Budget,
    session: u64,
    epoch: u64,
    generation: u64,
}

impl OutputScope {
    pub(crate) fn matches(&self, output: &Output) -> bool {
        self.source == output.source
            && self.session == output.session
            && self.epoch == output.epoch
            && self.generation == output.generation
            && self.pool.same_pool(output.pool())
    }
}

#[derive(Debug)]
pub struct Output {
    source: triergon_core::SourceId,
    pool: Option<Budget>,
    session: u64,
    epoch: u64,
    generation: u64,
    event: RawEvent,
    delivery: Delivery,
    bytes: usize,
}
impl Output {
    pub(crate) fn into_matroska_replay(mut self) -> Option<MatroskaReplayOutput> {
        let pool = self.pool.take().unwrap_or_else(|| self.pool().clone());
        let RawEvent::Matroska(event) = self.event else {
            return None;
        };
        Some(MatroskaReplayOutput {
            scope: OutputScope {
                source: self.source,
                pool,
                session: self.session,
                epoch: self.epoch,
                generation: self.generation,
            },
            event,
            delivery: self.delivery,
            bytes: self.bytes,
        })
    }

    pub fn source_id(&self) -> triergon_core::SourceId {
        self.source
    }
    pub fn belongs_to(&self, pool: &Budget) -> bool {
        self.pool().same_pool(pool)
    }
    pub(crate) fn pool(&self) -> &Budget {
        if let RawEvent::Matroska(triergon_matroska::typed::Event::Packet(packet)) = &self.event {
            return packet.configuration.budget();
        }
        self.pool
            .as_ref()
            .expect("non-packet output retains its allocation budget")
    }

    pub(crate) fn scope(&self) -> OutputScope {
        OutputScope {
            source: self.source,
            pool: self.pool().clone(),
            session: self.session,
            epoch: self.epoch,
            generation: self.generation,
        }
    }
    pub fn event(&self) -> &RawEvent {
        &self.event
    }
    pub fn delivery(&self) -> Delivery {
        self.delivery
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn into_event(self) -> RawEvent {
        self.event
    }
}
pub(crate) struct MatroskaReplayOutput {
    scope: OutputScope,
    pub event: triergon_matroska::typed::Event,
    delivery: Delivery,
    bytes: usize,
}

impl MatroskaReplayOutput {
    pub fn into_output(self) -> Output {
        Output {
            source: self.scope.source,
            pool: Some(self.scope.pool),
            session: self.scope.session,
            epoch: self.scope.epoch,
            generation: self.scope.generation,
            event: RawEvent::Matroska(self.event),
            delivery: self.delivery,
            bytes: self.bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Paused,
    Ended,
    Stopped,
    Cancelled,
    Failed,
}
#[derive(Clone, Copy, Debug)]
pub struct PlayerPolicy {
    pub slots: usize,
    pub queued_payload_bytes: usize,
    /// Input consumed before the seek reference track reaches the requested time.
    pub seek_input_bytes: u64,
    pub maximum_events: u64,
}
impl Default for PlayerPolicy {
    fn default() -> Self {
        Self {
            slots: 16,
            queued_payload_bytes: 32 * 1024 * 1024,
            seek_input_bytes: 128 * 1024 * 1024,
            maximum_events: 10_000_000,
        }
    }
}
#[derive(Debug)]
pub enum Progress {
    Yield,
    Queued,
    NeedInput(InputRequest),
    Paused,
    Ended,
    Stopped,
    Backpressure { requested: usize, available: usize },
    AwaitEntity,
}
#[derive(Clone, Copy)]
struct ActiveSeek {
    intent: SeekIntent,
    matroska_ceiling_ns: Option<i64>,
    reached_target: bool,
}

pub struct Player<S: Source> {
    engine: Option<Engine>,
    source: Option<SourceOwner<S>>,
    pool: Budget,
    session: u64,
    generation: u64,
    epoch: u64,
    state: State,
    policy: PlayerPolicy,
    selection: Selection,
    clock: ClockChoice,
    queue: BudgetVec<Option<Output>>,
    head: usize,
    count: usize,
    bytes: usize,
    pending: Option<RawEvent>,
    pending_source: Option<triergon_core::SourceId>,
    request: Option<InputRequest>,
    seek: Option<ActiveSeek>,
    seek_read: u64,
    events: u64,
    cancel: AtomicBool,
}
impl<S: Source> Player<S> {
    pub fn new(
        engine: Engine,
        source: SourceOwner<S>,
        selection: Selection,
        policy: PlayerPolicy,
    ) -> Result<Self> {
        Self::with_queue(engine, source, selection, policy, true)
    }

    /// Start without allocating a FIFO for callers using [Self::read_output].
    /// The first FIFO advance reserves its configured slots before reading
    /// the source. Pool pressure at that point is retryable.
    pub fn new_direct(
        engine: Engine,
        source: SourceOwner<S>,
        selection: Selection,
        policy: PlayerPolicy,
    ) -> Result<Self> {
        Self::with_queue(engine, source, selection, policy, false)
    }

    fn with_queue(
        engine: Engine,
        source: SourceOwner<S>,
        selection: Selection,
        policy: PlayerPolicy,
        eager: bool,
    ) -> Result<Self> {
        if policy.slots == 0
            || policy.slots > 64
            || policy.queued_payload_bytes == 0
            || policy.maximum_events == 0
        {
            return Err(Error::Invalid("player policy"));
        }
        engine.validate_binding()?;
        let pool = engine.pool().clone();
        let session = engine.generation();
        let mut queue = BudgetVec::new(&pool);
        if eager {
            Self::allocate_slots(&mut queue, policy.slots)?;
        }
        // MP4 catalogues are already known. Streaming catalogues are validated when
        // emitted, while All always preserves every selected-program source packet.
        if matches!(engine, Engine::Mp4(_)) {
            selection.validate(&engine)?;
        }
        Ok(Self {
            engine: Some(engine),
            source: Some(source),
            pool,
            session,
            generation: session,
            epoch: 0,
            state: State::Running,
            policy,
            selection,
            clock: ClockChoice::SourceExact,
            queue,
            head: 0,
            count: 0,
            bytes: 0,
            pending: None,
            pending_source: None,
            request: None,
            seek: None,
            seek_read: 0,
            events: 0,
            cancel: AtomicBool::new(false),
        })
    }
    fn allocate_slots(queue: &mut BudgetVec<Option<Output>>, slots: usize) -> Result<()> {
        queue.try_reserve_exact(slots)?;
        for _ in 0..slots {
            queue.try_push(None).map_err(|failure| failure.error)?;
        }
        Ok(())
    }

    pub fn set_clock_view(&mut self, clock: ClockChoice) -> Result<()> {
        if self.count != 0 || self.pending.is_some() || self.events != 0 {
            return Err(Error::Invalid("clock view must be frozen before delivery"));
        }
        self.clock = clock;
        Ok(())
    }
    pub fn budget(&self) -> &Budget {
        &self.pool
    }
    pub(crate) fn source_mut(&mut self) -> Option<&mut S> {
        self.source.as_mut().and_then(SourceOwner::source_mut)
    }
    pub fn seek_intent(&self) -> Option<SeekIntent> {
        self.seek.map(|seek| seek.intent)
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn queued(&self) -> usize {
        self.count
    }
    pub fn engine(&self) -> Option<&Engine> {
        self.engine.as_ref()
    }
    pub fn pending_request(&self) -> Option<&InputRequest> {
        self.request.as_ref()
    }
    pub fn current(&self, o: &Output) -> bool {
        !matches!(
            self.state,
            State::Stopped | State::Cancelled | State::Failed
        ) && self.pool.same_pool(o.pool())
            && self.session == o.session
            && self.generation == o.generation
            && self.epoch == o.epoch
    }
    pub fn may_deliver(&self, o: &Output) -> bool {
        self.current(o) && self.state != State::Paused
    }
    pub fn pause(&mut self, paused: bool) -> Result<()> {
        if !matches!(self.state, State::Running | State::Paused) {
            return Err(Error::Closed);
        }
        self.state = if paused {
            State::Paused
        } else {
            State::Running
        };
        Ok(())
    }
    pub fn pop(&mut self) -> Option<Output> {
        if self.count == 0 || self.state == State::Paused {
            return None;
        }
        let out = self.queue.as_mut_slice()[self.head].take()?;
        self.head = self.queue_index(1);
        self.count -= 1;
        self.bytes -= out.bytes;
        Some(out)
    }
    fn queue_index(&self, offset: usize) -> usize {
        let at = self.head + offset;
        if at < self.policy.slots {
            at
        } else {
            at - self.policy.slots
        }
    }
    fn drain(&mut self) {
        for s in self.queue.as_mut_slice() {
            *s = None;
        }
        self.head = 0;
        self.count = 0;
        self.bytes = 0;
        self.pending = None;
        self.pending_source = None;
        self.request = None;
    }
    fn shutdown(&mut self, state: State) {
        if matches!(
            self.state,
            State::Stopped | State::Cancelled | State::Failed
        ) {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(s) = self.source.as_mut() {
            s.abort();
        }
        self.drain();
        self.engine = None;
        if let Some(mut s) = self.source.take() {
            s.close();
        }
        self.state = state;
    }
    pub fn stop(&mut self) {
        self.shutdown(State::Stopped);
    }
    pub fn cancel(&mut self) {
        self.shutdown(State::Cancelled);
    }
    pub fn seek_and_select(
        &mut self,
        track: TrackKey,
        target: Time,
        selection: Selection,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<bool> {
        self.apply_seek(track, target, selection, None, w, c)
    }

    fn apply_seek(
        &mut self,
        track: TrackKey,
        target: Time,
        selection: Selection,
        mp4_landing: Option<crate::engines::Mp4RangeLanding<'_>>,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<bool> {
        if c.load(Ordering::Relaxed) {
            self.cancel();
            return Err(Error::Cancelled);
        }
        if !matches!(self.state, State::Running | State::Paused | State::Ended) {
            return Err(Error::Closed);
        }
        if w.remaining() == 0 {
            return Ok(false);
        }
        let engine = self.engine.as_mut().ok_or(Error::Closed)?;
        selection.validate(engine)?;
        if !engine
            .facts()?
            .as_slice()
            .iter()
            .any(|f| f.key == track && f.kind == TrackKind::Video && f.enabled)
        {
            return Err(Error::Invalid("seek target must be source video"));
        }
        let epoch = self.epoch.checked_add(1).ok_or(Error::Overflow)?;
        let intent = SeekIntent {
            track,
            target,
            clock: self.clock,
            replay_from_start: mp4_landing.is_none() && !matches!(engine, Engine::Matroska(_)),
        };
        if !w.try_charge(1) {
            return Ok(false);
        }
        let before = engine.generation();
        let result = engine.begin_seek(&intent, mp4_landing);
        let after = engine.generation();
        if let Err(e) = result {
            if before != after {
                self.shutdown(State::Failed);
            }
            return Err(e);
        }
        self.source.as_mut().ok_or(Error::Closed)?.invalidate();
        self.drain();
        self.epoch = epoch;
        self.generation = after;
        self.selection = selection;
        self.stage_seek(intent);
        if self.state == State::Ended {
            self.state = State::Running;
        }
        Ok(true)
    }
    pub(crate) fn stage_seek(&mut self, intent: SeekIntent) {
        // Integral packet PTS is below the exact target precisely when it
        // is below its ceiling in nanoseconds. Keep the original time for
        // delivery and the rational comparison for laced or extreme clocks.
        let matroska_ceiling_ns = if matches!(intent.track, TrackKey::Matroska(_))
            && intent.clock == ClockChoice::SourceExact
        {
            intent
                .target
                .to_nanoseconds(triergon_core::Rounding::Ceil)
                .ok()
        } else {
            None
        };
        self.seek = Some(ActiveSeek {
            intent,
            matroska_ceiling_ns,
            reached_target: false,
        });
        self.seek_read = 0;
        self.pending = Some(RawEvent::Seek(intent));
        self.pending_source = self.engine.as_ref().map(Engine::source);
    }

    pub fn apply_movie_seek(
        &mut self,
        command: &crate::mp4_plan::MovieSeekCommand,
        selection: Selection,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<crate::mp4_plan::MovieSeekOutcome> {
        use crate::mp4_plan::MovieSeekOutcome;
        if c.load(Ordering::Relaxed) {
            self.cancel();
            return Err(Error::Cancelled);
        }
        let Some(Engine::Mp4(engine)) = self.engine.as_ref() else {
            return Err(Error::Invalid("movie seek requires MP4 source"));
        };
        if !self.pool.same_pool(&command.pool) {
            return Err(Error::ForeignPool);
        }
        if engine.plan.source != command.source
            || engine.plan.opening_generation != command.generation
        {
            return Err(Error::SourceMismatch);
        }
        match &command.point {
            triergon_isobmff::MoviePoint::Media(point) => {
                let target = if self.clock == ClockChoice::SourceExact {
                    point.source_time
                } else {
                    point.requested_movie_time
                };
                if self.seek_and_select(TrackKey::Mp4(command.track), target, selection, w, c)? {
                    Ok(MovieSeekOutcome::Applied(point.clone()))
                } else {
                    Ok(MovieSeekOutcome::Yield)
                }
            }
            triergon_isobmff::MoviePoint::Empty(point) => {
                Ok(MovieSeekOutcome::Empty(point.clone()))
            }
            triergon_isobmff::MoviePoint::Outside {
                requested_movie_time,
            } => Ok(MovieSeekOutcome::Outside {
                requested_movie_time: *requested_movie_time,
            }),
        }
    }
    pub fn continue_entity(
        &mut self,
        id: triergon_core::SourceId,
        caps: triergon_core::SourceCapabilities,
        next: &mut Option<S>,
        discontinuity: bool,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<bool> {
        if c.load(Ordering::Relaxed) {
            self.cancel();
            return Err(Error::Cancelled);
        }
        if !matches!(self.state, State::Running | State::Paused) {
            return Err(Error::Closed);
        }
        if w.remaining() == 0 {
            return Ok(false);
        }
        if next.is_none() {
            return Err(Error::Invalid("next source owner missing"));
        }
        if !w.try_charge(1) {
            return Ok(false);
        }
        self.engine
            .as_mut()
            .ok_or(Error::Closed)?
            .continue_entity(id, caps, discontinuity)?;
        if let Some(mut old) = self.source.take() {
            old.close();
        }
        self.source = Some(SourceOwner::new(
            next.take()
                .ok_or(Error::Invalid("next source owner missing"))?,
        ));
        self.request = None;
        Ok(true)
    }
    /// Prepare an explicit fresh MP4 catalogue without polling either source.
    /// Refusals (including cancellation of this attempt) preserve the old source,
    /// every cursor/output and the candidate. The distinct SourceId is supplied
    /// by the caller's stable-entity contract, never inferred from metadata.
    pub fn prepare_mp4_catalogue<'a>(
        &'a mut self,
        next: &'a mut Option<crate::catalogue::Mp4Catalogue<S>>,
        selection: Selection,
        w: &mut WorkBudget,
        c: &'a AtomicBool,
    ) -> Result<Option<crate::catalogue::CatalogueCommit<'a, S>>> {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !matches!(self.state, State::Running | State::Paused | State::Ended) {
            return Err(Error::Closed);
        }
        if w.remaining() == 0 {
            return Ok(None);
        }
        let old = self.engine.as_ref().ok_or(Error::Closed)?;
        if !matches!(old, Engine::Mp4(_)) || self.source.is_none() {
            return Err(Error::Invalid("catalogue replacement needs active MP4"));
        }
        let candidate = next.as_ref().ok_or(Error::Invalid("replacement missing"))?;
        candidate.engine.validate_binding()?;
        if !candidate.engine.pool().same_pool(&self.pool) {
            return Err(Error::ForeignPool);
        }
        if candidate.engine.source() == old.source() {
            return Err(Error::SourceMismatch);
        }
        let epoch = self.epoch.checked_add(1).ok_or(Error::Overflow)?;
        if !w.try_charge(1) {
            return Ok(None);
        }
        selection.validate(&candidate.engine)?;
        let queue = self.prepare_retired_queue()?;
        Ok(Some(crate::catalogue::CatalogueCommit {
            player: self,
            next,
            selection,
            epoch,
            queue,
            cancel: c,
            seek: None,
        }))
    }

    fn prepare_retired_queue(&self) -> Result<BudgetVec<Option<Output>>> {
        // Reserve only actual queued envelopes before moving any owners.
        let mut queue = BudgetVec::new(&self.pool);
        queue.try_reserve_exact(self.count)?;
        for _ in 0..self.count {
            queue.try_push(None).map_err(|e| e.error)?;
        }
        Ok(queue)
    }
    fn progressive_seek_candidate<'a>(
        &self,
        next: &'a crate::catalogue::Mp4Catalogue<S>,
    ) -> Result<&'a Engine> {
        let (Some(Engine::Mp4(old)), engine @ Engine::Mp4(candidate)) =
            (self.engine.as_ref(), &next.engine)
        else {
            return Err(Error::Invalid(
                "progressive range seek requires MP4 catalogues",
            ));
        };
        if !old.is_progressive()
            || candidate.plan.read_scope != crate::mp4_open::MetadataReadScope::CompleteEntity
            || !candidate.supports_range_replay()
        {
            return Err(Error::Unsupported(
                "progressive seek requires a complete range index",
            ));
        }
        if !self.pool.same_pool(engine.pool()) {
            return Err(Error::ForeignPool);
        }
        if old.plan.source != candidate.plan.source
            || matches!((old.plan.source_length, candidate.plan.source_length),
                        (Some(old), Some(new)) if old != new)
        {
            return Err(Error::SourceMismatch);
        }
        engine.validate_binding()?;
        Ok(engine)
    }

    fn prepare_progressive_commit<'a>(
        &'a mut self,
        next: &'a mut Option<crate::catalogue::Mp4Catalogue<S>>,
        seek: crate::catalogue::ProgressiveSeek<'a>,
        work: &mut WorkBudget,
        cancel: &'a AtomicBool,
    ) -> Result<Option<crate::catalogue::CatalogueCommit<'a, S>>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !matches!(self.state, State::Running | State::Paused | State::Ended) {
            return Err(Error::Closed);
        }
        if work.remaining() == 0 {
            return Ok(None);
        }
        if !self.pool.same_pool(seek.pool) {
            return Err(Error::ForeignPool);
        }
        let generation = seek.active_generation.ok_or(Error::Invalid(
            "seek command does not promote a progressive catalogue",
        ))?;
        if self.generation != generation {
            return Err(Error::StaleGeneration {
                expected: self.generation,
                received: generation,
            });
        }
        if self.clock != seek.intent.clock {
            return Err(Error::Invalid("progressive seek clock changed"));
        }
        let candidate = next.as_ref().ok_or(Error::Invalid("replacement missing"))?;
        let engine = self.progressive_seek_candidate(candidate)?;
        if engine.generation() != seek.candidate_generation {
            return Err(Error::StaleGeneration {
                expected: engine.generation(),
                received: seek.candidate_generation,
            });
        }
        let epoch = self.epoch.checked_add(1).ok_or(Error::Overflow)?;
        if !work.try_charge(1) {
            return Ok(None);
        }
        let queue = self.prepare_retired_queue()?;
        Ok(Some(crate::catalogue::CatalogueCommit {
            player: self,
            next,
            selection: seek.selection,
            epoch,
            queue,
            cancel,
            seek: Some((seek.intent, seek.landing)),
        }))
    }

    pub fn replace_mp4_catalogue(
        &mut self,
        next: &mut Option<crate::catalogue::Mp4Catalogue<S>>,
        selection: Selection,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<Option<crate::catalogue::RetiredCatalogue>> {
        self.prepare_mp4_catalogue(next, selection, w, c)?
            .map(crate::catalogue::CatalogueCommit::commit)
            .transpose()
    }
    pub(crate) fn commit_mp4_catalogue(
        &mut self,
        next: crate::catalogue::Mp4Catalogue<S>,
        selection: Selection,
        epoch: u64,
        mut queue: BudgetVec<Option<Output>>,
    ) -> crate::catalogue::RetiredCatalogue {
        for (i, slot) in queue.as_mut_slice().iter_mut().enumerate() {
            *slot = self.queue.as_mut_slice()[(self.head + i) % self.policy.slots].take();
        }
        let retired = crate::catalogue::RetiredCatalogue {
            engine: self.engine.replace(next.engine),
            queue,
            head: 0,
            pending: self.pending.take(),
            pending_source: self.pending_source.take(),
            request: self.request.take(),
            seek: self.seek.take().map(|seek| seek.intent),
        };
        if let Some(mut old) = self.source.replace(next.source) {
            old.close();
        }
        self.head = 0;
        self.count = 0;
        self.bytes = 0;
        self.seek_read = 0;
        self.selection = selection;
        self.epoch = epoch;
        self.generation = self
            .engine
            .as_ref()
            .map_or(self.generation, Engine::generation);
        // Clock view and the cumulative event cap remain the caller's original
        // choices. Ordinal/revision restart belongs to the new source, not a repair
        // of the old source clocks/configuration/cursor.
        if self.state == State::Ended {
            self.state = State::Running;
        }
        retired
    }
    pub fn finish_logical_source(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<bool> {
        if c.load(Ordering::Relaxed) {
            self.cancel();
            return Err(Error::Cancelled);
        }
        if w.remaining() == 0 {
            return Ok(false);
        }
        if !w.try_charge(1) {
            return Ok(false);
        }
        self.engine
            .as_mut()
            .ok_or(Error::Closed)?
            .finish_logical_source()?;
        Ok(true)
    }
    pub fn provide(&mut self, reply: &SourceReply) -> Result<bool> {
        let r = self
            .request
            .as_ref()
            .ok_or(Error::Invalid("unsolicited completion"))?;
        if let SourceReply::Data(window) = reply
            && window.bytes().len() > r.maximum
        {
            return Err(Error::Limit {
                requested: window.bytes().len(),
                maximum: r.maximum,
            });
        }
        let accepted = self.engine.as_mut().ok_or(Error::Closed)?.provide(reply)?;
        if accepted {
            if self.seek.is_some_and(|seek| !seek.reached_target)
                && let SourceReply::Data(window) = reply
            {
                self.seek_read = self
                    .seek_read
                    .checked_add(window.bytes().len() as u64)
                    .ok_or(Error::Overflow)?;
                if self.seek_read > self.policy.seek_input_bytes {
                    return Err(Error::Limit {
                        requested: usize::try_from(self.seek_read).map_err(|_| Error::Overflow)?,
                        maximum: usize::try_from(self.policy.seek_input_bytes)
                            .map_err(|_| Error::Overflow)?,
                    });
                }
            }
            self.request = None;
        }
        Ok(accepted)
    }
    pub fn advance(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<Progress> {
        self.drive::<false>(w, c, None, None)
    }

    /// Publish a ready backend event in this call when its queue charge fits.
    /// If work or queue space runs out, retain the original event for retry.
    /// This is the same FIFO and delivery policy as [Self::advance].
    pub fn advance_ready(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<Progress> {
        self.drive::<true>(w, c, None, None)
    }

    /// Drive toward one output in the supplied empty slot. An older queued
    /// output is transferred first. Otherwise, ready events pass directly
    /// through the same work, byte, event and delivery checks as the FIFO.
    /// Queued means the caller's slot received the output. Borrow the slot
    /// when the consumer only reads the event. Pending input is
    /// reported once the source is waiting, so a slow source is not polled
    /// repeatedly in this call.
    pub fn read_output(
        &mut self,
        slot: &mut Option<Output>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Progress> {
        self.read_output_inner(slot, work, cancel, None)
    }

    fn read_output_inner(
        &mut self,
        slot: &mut Option<Output>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        mut cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<Progress> {
        if cancel.load(Ordering::Relaxed) {
            self.cancel();
            return Err(Error::Cancelled);
        }
        if work.remaining() == 0 {
            return Ok(Progress::Yield);
        }
        if slot.is_some() {
            return Ok(Progress::Backpressure {
                requested: 1,
                available: 0,
            });
        }
        if let Some(output) = self.pop() {
            *slot = Some(output);
            return Ok(Progress::Queued);
        }
        loop {
            let before = work.remaining();
            let result = self.drive::<true>(work, cancel, Some(slot), cursor.as_deref_mut())?;
            if work.remaining() == 0 {
                return Ok(result);
            }
            match result {
                Progress::NeedInput(_) => continue,
                Progress::Yield if self.request.is_none() && work.remaining() < before => continue,
                Progress::Yield => {
                    return Ok(self
                        .request
                        .clone()
                        .map_or(Progress::Yield, Progress::NeedInput));
                }
                other => return Ok(other),
            }
        }
    }

    fn drive<const READY: bool>(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
        slot: Option<&mut Option<Output>>,
        cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<Progress> {
        if c.load(Ordering::Relaxed) {
            self.cancel();
            return Err(Error::Cancelled);
        }
        let result = self.inner::<READY>(w, slot, cursor);
        if let Err(Error::Backpressure {
            requested,
            available,
        }) = result
        {
            return Ok(Progress::Backpressure {
                requested,
                available,
            });
        }
        if result.is_err() {
            self.shutdown(State::Failed);
        }
        result
    }
    fn inner<const READY: bool>(
        &mut self,
        w: &mut WorkBudget,
        slot: Option<&mut Option<Output>>,
        cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<Progress> {
        match self.state {
            State::Paused => return Ok(Progress::Paused),
            State::Ended => return Ok(Progress::Ended),
            State::Stopped | State::Cancelled => return Ok(Progress::Stopped),
            State::Failed => return Err(Error::Closed),
            State::Running => {}
        }
        if w.remaining() == 0 {
            return Ok(Progress::Yield);
        }
        if self.count == self.policy.slots {
            return Ok(Progress::Backpressure {
                requested: 1,
                available: 0,
            });
        }
        if slot.is_none() && self.queue.is_empty() {
            Self::allocate_slots(&mut self.queue, self.policy.slots)?;
        }
        if let Some(event) = self.pending.as_ref() {
            let Some((bytes, delivery)) = self.admit(event, w)? else {
                return Ok(Progress::Yield);
            };
            let event = self
                .pending
                .take()
                .ok_or(Error::Invalid("pending event missing"))?;
            let source = self
                .pending_source
                .take()
                .ok_or(Error::Invalid("pending event source binding"))?;
            return Ok(self.publish(source, event, bytes, delivery, slot));
        }
        if let Some(r) = self.request.clone() {
            let reply = self
                .source
                .as_mut()
                .ok_or(Error::Closed)?
                .poll(&r, w, &self.cancel)?;
            self.provide(&reply)?;
            return Ok(Progress::Yield);
        }
        let engine = self.engine.as_mut().ok_or(Error::Closed)?;
        let step = if let Engine::Matroska(matroska) = engine {
            match matroska.step(w, &self.cancel)? {
                triergon_matroska::incremental::ParseStep::Event(event) => {
                    let source = engine.source();
                    return self.accept_event::<READY>(source, RawEvent::Matroska(event), w, slot);
                }
                step => BackendStep::from_matroska(step),
            }
        } else if let Some(cursor) = cursor {
            engine.poll_mp4_range(cursor, w, &self.cancel)?
        } else {
            let step = engine.poll(w, &self.cancel);
            let skipped = engine.take_suppressed_events();
            if skipped != 0 {
                let events = self.events.checked_add(skipped).ok_or(Error::Overflow)?;
                Self::check_event_limit(events, self.policy.maximum_events)?;
                self.events = events;
            }
            step?
        };
        match step {
            BackendStep::Event(event) => {
                let source = engine.source();
                self.accept_event::<READY>(source, event, w, slot)
            }
            BackendStep::NeedInput(r) => {
                self.request = Some(r.clone());
                Ok(Progress::NeedInput(r))
            }
            BackendStep::Yield => Ok(Progress::Yield),
            BackendStep::AwaitEntity => Ok(Progress::AwaitEntity),
            BackendStep::End => {
                self.state = State::Ended;
                Ok(Progress::Ended)
            }
            BackendStep::Backpressure {
                requested,
                available,
            } => Ok(Progress::Backpressure {
                requested,
                available,
            }),
        }
    }

    #[inline]
    fn accept_event<const READY: bool>(
        &mut self,
        source: triergon_core::SourceId,
        event: RawEvent,
        work: &mut WorkBudget,
        slot: Option<&mut Option<Output>>,
    ) -> Result<Progress> {
        let progress = if READY {
            match self.admit(&event, work) {
                Ok(Some((bytes, delivery))) => {
                    return Ok(self.publish(source, event, bytes, delivery, slot));
                }
                Ok(None) => Ok(Progress::Yield),
                Err(error @ Error::Backpressure { .. }) => Err(error),
                Err(error) => return Err(error),
            }
        } else {
            Ok(Progress::Yield)
        };
        self.pending_source = Some(source);
        self.pending = Some(event);
        progress
    }

    fn admit(&self, e: &RawEvent, w: &mut WorkBudget) -> Result<Option<(usize, Delivery)>> {
        let bytes = e.payload_bytes();
        if bytes > self.policy.queued_payload_bytes {
            return Err(Error::Limit {
                requested: bytes,
                maximum: self.policy.queued_payload_bytes,
            });
        }
        let available = self.policy.queued_payload_bytes - self.bytes;
        if bytes > available {
            return Err(Error::Backpressure {
                requested: bytes,
                available,
            });
        }
        if !w.try_charge(1) {
            return Ok(None);
        }
        if matches!(
            e,
            RawEvent::Matroska(triergon_matroska::typed::Event::Configuration(_))
                | RawEvent::Ts(crate::TsEvent {
                    source: triergon_mpegts::TraceEvent::ProgramMap { active: true, .. },
                    ..
                })
        ) {
            self.selection
                .validate(self.engine.as_ref().ok_or(Error::Closed)?)?;
        }
        let facts_only = (self.clock == ClockChoice::SourceExact
            && matches!(e, RawEvent::NativeTs { .. }))
            || (self.clock == ClockChoice::ExplicitNativeProjection
                && matches!(
                    e,
                    RawEvent::Ts(crate::TsEvent {
                        source: triergon_mpegts::TraceEvent::Pes(pes),
                        ..
                    }) if self.engine.as_ref().is_some_and(|engine| {
                        engine.replaces_source_ts_pes(pes.pid)
                    })
                ));
        let delivery = if facts_only {
            Delivery::Source
        } else {
            match e.track() {
                None => Delivery::Source,
                Some(key) if !self.selection.includes(key) => Delivery::Unselected,
                Some(_) => match (self.clock, self.seek, e) {
                    // Matroska already checked and rounded the source PTS.
                    // Before a seek, delivery needs only its knownness.
                    (
                        ClockChoice::SourceExact,
                        None,
                        RawEvent::Matroska(triergon_matroska::typed::Event::Packet(packet)),
                    ) => {
                        if packet.packet.pts_ns.is_some() {
                            Delivery::Present
                        } else {
                            Delivery::UnknownTime
                        }
                    }
                    (
                        ClockChoice::SourceExact,
                        Some(ActiveSeek {
                            intent,
                            matroska_ceiling_ns: Some(ceiling),
                            ..
                        }),
                        RawEvent::Matroska(triergon_matroska::typed::Event::Packet(packet)),
                    ) if packet.packet.timing.frames == 1 => match packet.packet.pts_ns {
                        None => Delivery::UnknownTime,
                        Some(pts) if pts < ceiling => Delivery::DecodeOnly {
                            until: intent.target,
                        },
                        Some(_) => Delivery::Present,
                    },
                    (clock, seek, _) => {
                        let pts = match clock {
                            ClockChoice::SourceExact => e.source_pts()?,
                            ClockChoice::ExplicitNativeProjection => e.native_pts()?,
                        };
                        match (pts, seek) {
                            (None, _) => Delivery::UnknownTime,
                            (Some(pts), Some(s)) if pts < s.intent.target => Delivery::DecodeOnly {
                                until: s.intent.target,
                            },
                            _ => Delivery::Present,
                        }
                    }
                },
            }
        };
        let events = self.events.checked_add(1).ok_or(Error::Overflow)?;
        Self::check_event_limit(events, self.policy.maximum_events)?;
        Ok(Some((bytes, delivery)))
    }

    fn check_event_limit(events: u64, maximum: u64) -> Result<()> {
        if events > maximum {
            return Err(Error::Limit {
                requested: usize::try_from(maximum + 1).map_err(|_| Error::Overflow)?,
                maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
            });
        }
        Ok(())
    }

    // Keep construction at the destination, including for the caller's slot.
    #[inline(always)]
    fn publish(
        &mut self,
        source: triergon_core::SourceId,
        event: RawEvent,
        bytes: usize,
        delivery: Delivery,
        slot: Option<&mut Option<Output>>,
    ) -> Progress {
        if let Some(seek) = self.seek.as_mut().filter(|seek| !seek.reached_target)
            && bytes != 0
            && matches!(delivery, Delivery::Present | Delivery::Unselected)
            && event.track() == Some(seek.intent.track)
            && !matches!(&event, RawEvent::Mp4(crate::Mp4Event::NativeShortPacket(_)))
            && !matches!(&event, RawEvent::NativeTs { packet, .. } if packet.flags & 2 != 0)
        {
            // Reordered packets still need the original presentation target.
            // Only the preroll input accounting ends here.
            seek.reached_target = delivery == Delivery::Present
                || match seek.intent.clock {
                    ClockChoice::SourceExact => event.source_pts(),
                    ClockChoice::ExplicitNativeProjection => event.native_pts(),
                }
                .is_ok_and(|pts| pts.is_some_and(|pts| pts >= seek.intent.target));
        }
        let destination = if let Some(slot) = slot {
            slot
        } else {
            let at = self.queue_index(self.count);
            self.count += 1;
            self.bytes += bytes;
            &mut self.queue.as_mut_slice()[at]
        };
        *destination = Some(Output {
            source,
            pool: if matches!(
                &event,
                RawEvent::Matroska(triergon_matroska::typed::Event::Packet(_))
            ) {
                None
            } else {
                Some(self.pool.clone())
            },
            session: self.session,
            epoch: self.epoch,
            generation: self.generation,
            event,
            delivery,
            bytes,
        });
        self.events += 1;
        Progress::Queued
    }
}
impl<S: Source> Drop for Player<S> {
    fn drop(&mut self) {
        self.shutdown(State::Stopped);
    }
}
