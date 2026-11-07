//! Native range playback over borrowed MP4 sample tables and charged track heads.

use super::{
    ClockChoice, Mp4NativeSeekRequest, Output, Player, PlayerPolicy, Progress, Selection, TrackKey,
    mp4_native_seek::native_global_seek,
};
use crate::{
    Engine, Result,
    engines::Mp4RangeLanding,
    input::Source,
    io::SourceOwner,
    mp4_open::Movie,
    mp4_plan::{Mp4IndexJob, PlanLimits},
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, InputCursor, Time, WorkBudget};
use triergon_isobmff::{
    BuildStatus,
    compat::mov::{
        clock::MediaClock,
        edits::EncodedEditPlan,
        packets::{CursorStatus, PacketCursor},
        seek::{GlobalSeekJob, RangeSeekJob, SeekLanding},
    },
};

type Edits<'a> = &'a [Option<EncodedEditPlan>];

/// Prepare range playback from a complete, checked MP4 open transaction.
/// The native cursor checks every source extent and projected occurrence
/// before publishing data.
pub struct Mp4RangeOpen<'a> {
    movie: &'a Movie,
    edits: Option<Edits<'a>>,
    index: Mp4IndexJob<'a>,
}

impl<'a> Mp4RangeOpen<'a> {
    pub fn new(movie: &'a Movie, edits: Option<Edits<'a>>, limits: PlanLimits) -> Result<Self> {
        Ok(Self {
            movie,
            edits,
            index: Mp4IndexJob::new_native_range(movie, edits, limits)?,
        })
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        self.index.poll(work, cancel)
    }

    /// Move the source and input from the same open transaction into playback.
    pub fn finish<S: Source>(
        self,
        input: InputCursor,
        source: SourceOwner<S>,
        maximum_packet: usize,
        selection: Selection,
        policy: PlayerPolicy,
    ) -> Result<Mp4RangePlayer<'a, S>> {
        if !input.capabilities().random_access {
            return Err(Error::Unsupported(
                "native range playback needs random access",
            ));
        }
        let (plan, mut cursor, duration_us) = self.index.finish_native_range()?;
        // Leave space for packet/input staging and selection validation.
        // The fact vector grows over at most 64 tracks, including its old
        // allocation during a growth step.
        let fact_bytes = 128 * size_of::<crate::engines::TrackFact>();
        if input.budget().available() < maximum_packet.saturating_mul(2).max(fact_bytes) {
            cursor.discard_seek_checkpoints();
        }
        let engine = Engine::mp4(input, plan, maximum_packet)?;
        let mut player = Player::new_direct(engine, source, selection, policy)?;
        player.set_clock_view(ClockChoice::ExplicitNativeProjection)?;
        Ok(Mp4RangePlayer {
            movie: self.movie,
            edits: self.edits,
            cursor,
            player,
            duration_us,
        })
    }
}

/// Direct-output range player. Sample geometry and clocks are produced in
/// native occurrence order from the checked tables, with one head per track.
/// Payload reads and output generations use the ordinary player and MP4 engine.
pub struct Mp4RangePlayer<'a, S: Source> {
    movie: &'a Movie,
    edits: Option<Edits<'a>>,
    cursor: PacketCursor<'a>,
    player: Player<S>,
    duration_us: i64,
}

impl<'a, S: Source> Mp4RangePlayer<'a, S> {
    pub fn duration_us(&self) -> i64 {
        self.duration_us
    }

    pub fn pause(&mut self, paused: bool) -> Result<()> {
        self.player.pause(paused)
    }

    pub fn current(&self, output: &Output) -> bool {
        self.player.current(output)
    }

    /// Let an input adapter release optional seek state when it needs the
    /// same pool for range I/O or a retained output.
    pub fn discard_seek_checkpoints(&mut self) -> bool {
        self.cursor.discard_seek_checkpoints()
    }

    pub fn read_output(
        &mut self,
        output: &mut Option<Output>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Progress> {
        let result = self
            .player
            .read_output_inner(output, work, cancel, Some(&mut self.cursor))?;
        if matches!(result, Progress::Backpressure { .. })
            && output.is_none()
            && self.discard_seek_checkpoints()
        {
            return self
                .player
                .read_output_inner(output, work, cancel, Some(&mut self.cursor));
        }
        Ok(result)
    }

    /// Plan a seek from the same checked tables. This reads no payload and
    /// leaves the active generation and any pending input usable until apply.
    pub fn prepare_seek(&self, request: Mp4NativeSeekRequest) -> Result<Mp4RangeSeekJob<'a>> {
        let Some(Engine::Mp4(engine)) = self.player.engine.as_ref() else {
            return Err(Error::Closed);
        };
        let selected = engine.plan.track(request.video)?;
        if selected.snapshot.handler != *b"vide"
            || !selected.snapshot.enabled
            || !self.player.selection.includes(TrackKey::Mp4(request.video))
        {
            return Err(Error::Invalid("native seek requires selected MP4 video"));
        }
        let base = MediaClock::from_source(selected.snapshot.base.require()?)?.demux_base;
        let checked = self.cursor.prepare_checked_range_seek(
            request.video,
            request.timestamp,
            request.limits,
        )?;
        let (global, replay_seed) = if checked.is_some() {
            (None, None)
        } else {
            (
                Some(native_global_seek(self.movie, self.edits, request)?),
                Some(self.cursor.fork_checked_replay()?),
            )
        };
        Ok(Mp4RangeSeekJob {
            scope: SeekScope {
                pool: self.player.pool.clone(),
                session: self.player.session,
                generation: self.player.generation,
                video: request.video,
                target: Time::from_ticks(i128::from(request.timestamp), base)?,
            },
            checked,
            global,
            replay: None,
            replay_seed,
            maximum_skipped: request.limits.max_replayed_occurrences,
            failure: None,
        })
    }

    /// Apply once. A zero-work yield keeps both the active source and command.
    pub fn apply_seek(
        &mut self,
        command: &mut Mp4RangeSeek<'a>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let scope = &command.scope;
        if !self.player.pool.same_pool(&scope.pool) {
            return Err(Error::ForeignPool);
        }
        if self.player.session != scope.session {
            return Err(Error::SourceMismatch);
        }
        if self.player.generation != scope.generation {
            return Err(Error::StaleGeneration {
                expected: self.player.generation,
                received: scope.generation,
            });
        }
        let replacement = command
            .cursor
            .as_mut()
            .ok_or(Error::Invalid("native range seek command already applied"))?;
        let mut apply = |work: &mut WorkBudget| {
            self.player.apply_seek(
                TrackKey::Mp4(scope.video),
                scope.target,
                self.player.selection,
                Some(Mp4RangeLanding::NativeCursor(command.offset)),
                work,
                cancel,
            )
        };
        let applied = match apply(work) {
            Err(Error::Backpressure { .. })
                if {
                    // Only the selection/fact allocation can return backpressure,
                    // before the engine starts the seek transaction.
                    let current = self.cursor.discard_seek_checkpoints();
                    replacement.discard_seek_checkpoints() || current
                } =>
            {
                apply(work)?
            }
            result => result?,
        };
        if applied {
            self.cursor = command.cursor.take().expect("checked range seek cursor");
        }
        Ok(applied)
    }
}

struct SeekScope {
    pool: Budget,
    session: u64,
    generation: u64,
    video: u32,
    target: Time,
}

pub struct Mp4RangeSeekJob<'a> {
    scope: SeekScope,
    checked: Option<RangeSeekJob<'a>>,
    global: Option<GlobalSeekJob<'a>>,
    replay: Option<(PacketCursor<'a>, SeekLanding)>,
    replay_seed: Option<PacketCursor<'a>>,
    maximum_skipped: u64,
    failure: Option<Error>,
}

impl<'a> Mp4RangeSeekJob<'a> {
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }

    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if let Some(checked) = &mut self.checked {
            if checked.poll(work, cancel)? == BuildStatus::Yield || !work.try_charge(1) {
                return Ok(false);
            }
            self.replay = Some(self.checked.take().expect("polled checked seek").finish()?);
        }
        if let Some(global) = &mut self.global {
            if global.poll(work, cancel)? == BuildStatus::Yield || !work.try_charge(1) {
                return Ok(false);
            }
            let plan = self.global.take().expect("polled global seek").finish()?;
            let skipped = plan.anchors().iter().try_fold(0u64, |count, anchor| {
                count.checked_add(anchor.occurrence).ok_or(Error::Overflow)
            })?;
            if skipped > self.maximum_skipped {
                return Err(Error::Limit {
                    requested: usize::try_from(skipped).map_err(|_| Error::Overflow)?,
                    maximum: usize::try_from(self.maximum_skipped).map_err(|_| Error::Overflow)?,
                });
            }
            let cursor = self
                .replay_seed
                .take()
                .ok_or(Error::Invalid("native range replay seed missing"))?
                .resume_checked_replay(plan.anchors())?;
            self.replay = Some((cursor, plan.landing().clone()));
        }
        let (cursor, _) = self
            .replay
            .as_mut()
            .ok_or(Error::Invalid("native range replay missing"))?;
        match cursor.poll(work, cancel)? {
            CursorStatus::Ready => Ok(true),
            CursorStatus::Yield => Ok(false),
            CursorStatus::End => Err(Error::Invalid("native range seek landed at end")),
        }
    }

    pub fn finish(self) -> Result<Mp4RangeSeek<'a>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let (cursor, receipt) = self
            .replay
            .ok_or(Error::Invalid("native range seek still pending"))?;
        let offset = cursor
            .occurrence()
            .ok_or(Error::Invalid("native range seek still pending"))?
            .source
            .range
            .start;
        Ok(Mp4RangeSeek {
            scope: self.scope,
            cursor: Some(cursor),
            receipt,
            offset,
        })
    }
}

pub struct Mp4RangeSeek<'a> {
    scope: SeekScope,
    cursor: Option<PacketCursor<'a>>,
    receipt: SeekLanding,
    offset: u64,
}

impl Mp4RangeSeek<'_> {
    pub fn receipt(&self) -> &SeekLanding {
        &self.receipt
    }

    pub fn start_offset(&self) -> u64 {
        self.offset
    }
}
