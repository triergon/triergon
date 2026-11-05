//! Player handoff for the pinned MOV global seek over complete range sources.

use super::{ClockChoice, Player, SeekIntent, Selection, TrackKey};
use crate::{
    Engine, Result,
    catalogue::{CatalogueCommit, Mp4Catalogue, ProgressiveSeek, RetiredCatalogue},
    engines::Mp4RangeLanding,
    input::Source,
    mp4_native_replay::NativeMp4Replay,
    mp4_open::{MetadataReadScope, Movie},
    mp4_plan::{Mp4View, PreparedMp4},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, Time, WorkBudget};
use triergon_isobmff::{
    BuildStatus, Mov9EncodedEditPlanV2, Mov9MediaClock, Mov9NativeGlobalSeekJob,
    Mov9NativeSeekLimits, Mov9NativeSeekReceipt, Mov9Profile, Mov9Track,
};

#[derive(Clone, Copy, Debug)]
pub struct Mp4NativeSeekRequest {
    pub video: u32,
    /// Signed ticks in the reference track's native timebase.
    pub timestamp: i64,
    pub limits: Mov9NativeSeekLimits,
}

#[derive(Clone, Copy, Default)]
struct Anchor {
    track: u32,
    occurrence: u64,
    seen: u64,
    row: usize,
}

pub struct Mp4NativeSeekJob<'a> {
    plan: &'a PreparedMp4,
    global: Option<Mov9NativeGlobalSeekJob<'a>>,
    receipt: Option<Mov9NativeSeekReceipt>,
    request: Mp4NativeSeekRequest,
    target: Time,
    selection: Selection,
    generation: u64,
    progressive_generation: Option<u64>,
    anchors: [Anchor; 64],
    tracks: usize,
    pending: usize,
    row: usize,
    replay: Option<NativeMp4Replay>,
    failure: Option<Error>,
}

/// A generation-bound native seek with the original reference index entry.
pub struct Mp4NativeSeekCommand {
    pool: Budget,
    generation: u64,
    progressive_generation: Option<u64>,
    video: u32,
    target: Time,
    selection: Selection,
    replay: NativeMp4Replay,
    receipt: Mov9NativeSeekReceipt,
    offset: u64,
}

impl Mp4NativeSeekCommand {
    pub fn receipt(&self) -> &Mov9NativeSeekReceipt {
        &self.receipt
    }

    pub fn start_offset(&self) -> u64 {
        self.offset
    }

    pub(crate) fn intent(&self) -> SeekIntent {
        SeekIntent {
            track: TrackKey::Mp4(self.video),
            target: self.target,
            clock: ClockChoice::ExplicitNativeProjection,
            replay_from_start: false,
        }
    }
}

impl<S: Source> Player<S> {
    /// Reuse the complete Movie from the same open transaction. The seek job
    /// uses its compact sample tables and never reads payload bytes.
    pub fn prepare_mp4_native_seek<'a>(
        &'a self,
        movie: &'a Movie,
        request: Mp4NativeSeekRequest,
        selection: Selection,
    ) -> Result<Mp4NativeSeekJob<'a>> {
        let Some(engine @ Engine::Mp4(_)) = self.engine.as_ref() else {
            return Err(Error::Invalid("native seek requires MP4").into());
        };
        self.check_native_seek_clock()?;
        Mp4NativeSeekJob::new(engine, movie, request, selection, None)
    }

    /// Plan native replay after switching a progressive source to an
    /// independently opened range view of the same stable byte entity.
    pub fn prepare_mp4_progressive_native_seek<'a>(
        &self,
        next: &'a Mp4Catalogue<S>,
        movie: &'a Movie,
        request: Mp4NativeSeekRequest,
        selection: Selection,
    ) -> Result<Mp4NativeSeekJob<'a>> {
        self.check_native_seek_clock()?;
        let engine = self.progressive_seek_candidate(next)?;
        Mp4NativeSeekJob::new(engine, movie, request, selection, Some(self.generation))
    }

    pub fn apply_mp4_progressive_native_seek(
        &mut self,
        next: &mut Option<Mp4Catalogue<S>>,
        command: &Mp4NativeSeekCommand,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<RetiredCatalogue>> {
        self.prepare_mp4_progressive_native_commit(next, command, work, cancel)?
            .map(CatalogueCommit::commit)
            .transpose()
    }

    pub(crate) fn prepare_mp4_progressive_native_commit<'a>(
        &'a mut self,
        next: &'a mut Option<Mp4Catalogue<S>>,
        command: &'a Mp4NativeSeekCommand,
        work: &mut WorkBudget,
        cancel: &'a AtomicBool,
    ) -> Result<Option<CatalogueCommit<'a, S>>> {
        self.prepare_progressive_commit(
            next,
            ProgressiveSeek {
                pool: &command.pool,
                active_generation: command.progressive_generation,
                candidate_generation: command.generation,
                selection: command.selection,
                intent: command.intent(),
                landing: Mp4RangeLanding::Native(&command.replay),
            },
            work,
            cancel,
        )
    }

    fn check_native_seek_clock(&self) -> Result<()> {
        if self.clock != ClockChoice::ExplicitNativeProjection {
            return Err(Error::Unsupported("native seek needs complete native range MP4").into());
        }
        Ok(())
    }

    pub fn apply_mp4_native_seek(
        &mut self,
        command: &Mp4NativeSeekCommand,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if command.progressive_generation.is_some() {
            return Err(Error::Invalid("progressive seek needs its candidate catalogue").into());
        }
        if !self.pool.same_pool(&command.pool) {
            return Err(Error::ForeignPool.into());
        }
        if self.generation != command.generation {
            return Err(Error::StaleGeneration {
                expected: self.generation,
                received: command.generation,
            }
            .into());
        }
        if self.clock != ClockChoice::ExplicitNativeProjection {
            return Err(Error::Invalid("native seek clock changed").into());
        }
        self.apply_seek(
            TrackKey::Mp4(command.video),
            command.target,
            command.selection,
            Some(Mp4RangeLanding::Native(&command.replay)),
            work,
            cancel,
        )
    }
}

impl<'a> Mp4NativeSeekJob<'a> {
    fn new(
        engine: &'a Engine,
        movie: &'a Movie,
        request: Mp4NativeSeekRequest,
        selection: Selection,
        progressive_generation: Option<u64>,
    ) -> Result<Self> {
        let Engine::Mp4(mp4) = engine else {
            return Err(Error::Invalid("native seek requires MP4").into());
        };
        let plan = &mp4.plan;
        if plan.view != Mp4View::PinnedMov9NoParseNoFillin
            || plan.read_scope != MetadataReadScope::CompleteEntity
            || plan.native_recovery
            || !mp4.supports_range_replay()
        {
            return Err(Error::Unsupported("native seek needs complete native range MP4").into());
        }
        if !movie.pool.same_pool(&plan.pool) {
            return Err(Error::ForeignPool.into());
        }
        if movie.source != plan.source
            || movie.generation != plan.opening_generation
            || movie.read_scope != plan.read_scope
        {
            return Err(Error::SourceMismatch.into());
        }
        selection.validate(engine)?;
        let selected = plan.track(request.video)?;
        if selected.snapshot.handler != *b"vide"
            || !selected.snapshot.enabled
            || !selection.includes(TrackKey::Mp4(request.video))
        {
            return Err(Error::Invalid("native seek requires selected MP4 video").into());
        }
        let base = Mov9MediaClock::from_source(selected.snapshot.base.require()?)?.native_base;
        let target = Time::from_ticks(i128::from(request.timestamp), base)?;
        let global = native_global_seek(
            movie,
            plan.encoded_edits.as_ref().map(BudgetVec::as_slice),
            request,
        )?;
        Ok(Self {
            plan,
            global: Some(global),
            receipt: None,
            request,
            target,
            selection,
            generation: engine.generation(),
            progressive_generation,
            anchors: [Anchor::default(); 64],
            tracks: 0,
            pending: 0,
            row: 0,
            replay: None,
            failure: None,
        })
    }
}

pub(super) fn native_global_seek<'a>(
    movie: &'a Movie,
    edits: Option<&'a [Option<Mov9EncodedEditPlanV2>]>,
    request: Mp4NativeSeekRequest,
) -> Result<Mov9NativeGlobalSeekJob<'a>> {
    let tracks = movie.tracks();
    let maximum = request.limits.index.packets.max_tracks.min(64);
    if tracks.len() > maximum {
        return Err(Error::Limit {
            requested: tracks.len(),
            maximum,
        }
        .into());
    }
    let mut native = BudgetVec::new(&movie.pool);
    native.try_reserve_exact(tracks.len())?;
    let movie_base = movie.header().time_base()?;
    for track in tracks {
        native
            .try_push(Mov9Track {
                track_id: track.header().track_id,
                table: track.table(),
                fragments: track.fragments(),
                media_base: track.header().time_base()?,
                edits: track.edits.as_ref().map(|edits| (edits, movie_base)),
            })
            .map_err(|error| error.error)?;
    }
    Ok(if let Some(plans) = edits {
        Mov9NativeGlobalSeekJob::new_encoded_regular_v2_rescaled(
            &movie.pool,
            native.as_slice(),
            movie.media.as_slice(),
            (request.video, request.timestamp),
            request.limits,
            plans,
        )?
    } else {
        Mov9NativeGlobalSeekJob::new_complete_source_rescaled(
            &movie.pool,
            native.as_slice(),
            movie.media.as_slice(),
            request.video,
            request.timestamp,
            request.limits,
            Mov9Profile::PinnedNoParseNoFillin,
        )?
    })
}

impl Mp4NativeSeekJob<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if let Some(error) = self.failure {
            return Err(error.into());
        }
        let result = self.scan(work, cancel);
        if let Err(error) = result {
            self.failure = Some(error);
            self.global = None;
        }
        result.map_err(Into::into)
    }

    fn scan(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> triergon_core::Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.replay.is_some() {
            return Ok(true);
        }
        if let Some(global) = &mut self.global {
            if global.poll(work, cancel)? == BuildStatus::Yield {
                return Ok(false);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let global = self
                .global
                .take()
                .ok_or(Error::Invalid("missing native seek planner"))?
                .finish()?;
            self.receipt = Some(global.receipt().clone());
            self.tracks = global.anchors().len();
            self.pending = self.tracks;
            for (slot, anchor) in self.anchors.iter_mut().zip(global.anchors()) {
                *slot = Anchor {
                    track: anchor.track_id,
                    occurrence: anchor.occurrence,
                    seen: 0,
                    row: usize::MAX,
                };
            }
        }
        let rows = self.plan.native_rows();
        while self.pending != 0 {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            if self.row as u64 >= self.request.limits.index.max_entries {
                return Err(Error::Limit {
                    requested: self.row.saturating_add(1),
                    maximum: usize::try_from(self.request.limits.index.max_entries)
                        .map_err(|_| Error::Overflow)?,
                });
            }
            let row = rows.get(self.row).ok_or(Error::Invalid(
                "native seek occurrence absent from playback plan",
            ))?;
            let anchor = self.anchors[..self.tracks]
                .iter_mut()
                .find(|anchor| anchor.track == row.track_id)
                .ok_or(Error::Invalid(
                    "native seek track absent from playback plan",
                ))?;
            if anchor.row == usize::MAX {
                if anchor.seen == anchor.occurrence {
                    anchor.row = self.row;
                    self.pending -= 1;
                } else {
                    anchor.seen = anchor.seen.checked_add(1).ok_or(Error::Overflow)?;
                }
            }
            self.row += 1;
        }
        let mut tracks = [0; 64];
        let mut starts = [0; 64];
        let receipt = self
            .receipt
            .as_ref()
            .ok_or(Error::Invalid("native seek receipt missing"))?;
        for (i, anchor) in self.anchors[..self.tracks].iter().enumerate() {
            let row = &rows[anchor.row];
            if anchor.track == self.request.video
                && (row.source != receipt.selected.occurrence.source
                    || row.native != receipt.selected.occurrence.native
                    || row.edit != receipt.selected.occurrence.edit)
            {
                return Err(Error::Invalid(
                    "native reference occurrence differs from playback plan",
                ));
            }
            tracks[i] = anchor.track;
            starts[i] = anchor.row;
        }
        self.replay = Some(NativeMp4Replay::new(
            &tracks[..self.tracks],
            &starts[..self.tracks],
            rows,
        )?);
        Ok(true)
    }

    pub fn finish(self) -> Result<Mp4NativeSeekCommand> {
        if let Some(error) = self.failure {
            return Err(error.into());
        }
        let replay = self
            .replay
            .ok_or(Error::Invalid("native playback seek still pending"))?;
        let offset = self.plan.native_rows()[replay.first_row()]
            .source
            .range
            .start;
        Ok(Mp4NativeSeekCommand {
            pool: self.plan.pool.clone(),
            generation: self.generation,
            progressive_generation: self.progressive_generation,
            video: self.request.video,
            target: self.target,
            selection: self.selection,
            replay,
            receipt: self
                .receipt
                .ok_or(Error::Invalid("native seek receipt missing"))?,
            offset,
        })
    }
}
