//! Demux-clock seeks of a complete MP4 source: the MOV demux view's seek across
//! all tracks, planned from the tables and applied to a player.

use super::{ClockChoice, Player, SeekIntent, Selection, TrackKey};
use crate::{
    Engine, Result, Source,
    engine::Mp4RangeLanding,
    engine::mp4::demux_replay::DemuxReplay,
    mp4::catalogue::{CatalogueCommit, Mp4Catalogue, RangeSeek, RetiredCatalogue},
    mp4::open::{MetadataReadScope, Movie},
    mp4::plan::{PlanView, PreparedMp4, include_media_track},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, Time, WorkBudget};
use triergon_isobmff::{
    BuildStatus,
    compat::mov::{
        clock::MediaClock,
        edits::EncodedEditPlan,
        packets::{DemuxProfile, DemuxTrack},
        seek::{GlobalSeekJob, SeekLanding, SeekLimits},
    },
};

/// A demux-clock seek of an MP4 movie to `timestamp` on track `video`.
#[derive(Clone, Copy, Debug)]
pub struct Mp4DemuxSeekRequest {
    /// The track ID of the reference video track.
    pub video: u32,
    /// The target, in ticks of the reference track's demux time base.
    pub timestamp: i64,
    /// Bounds of the index search.
    pub limits: SeekLimits,
}

#[derive(Clone, Copy, Debug, Default)]
struct Anchor {
    track: u32,
    occurrence: u64,
    seen: u64,
    row: usize,
}

/// Plans a demux-clock seek from the sample tables, within work budgets.
#[derive(Debug)]
pub struct Mp4DemuxSeekJob<'a> {
    plan: &'a PreparedMp4,
    global: Option<GlobalSeekJob<'a>>,
    receipt: Option<SeekLanding>,
    request: Mp4DemuxSeekRequest,
    target: Time,
    selection: Selection,
    generation: u64,
    progressive_generation: Option<u64>,
    anchors: [Anchor; 64],
    tracks: usize,
    pending: usize,
    row: usize,
    replay: Option<DemuxReplay>,
    failure: Option<Error>,
}

/// A planned demux-clock seek, valid for the generation it was planned in.
#[derive(Debug)]
pub struct Mp4DemuxSeekCommand {
    pool: Budget,
    generation: u64,
    progressive_generation: Option<u64>,
    video: u32,
    target: Time,
    selection: Selection,
    replay: DemuxReplay,
    receipt: SeekLanding,
    offset: u64,
}

impl Mp4DemuxSeekCommand {
    /// The index entry the seek lands on.
    pub fn receipt(&self) -> &SeekLanding {
        &self.receipt
    }

    /// The byte offset of the first sample the seek reads.
    pub fn start_offset(&self) -> u64 {
        self.offset
    }

    pub(crate) fn seek(&self) -> RangeSeek<'_> {
        RangeSeek {
            pool: &self.pool,
            active_generation: self.progressive_generation,
            candidate_generation: self.generation,
            selection: self.selection,
            intent: SeekIntent {
                track: TrackKey::Mp4(self.video),
                target: self.target,
                clock: ClockChoice::Demux,
                replay_from_start: false,
            },
            landing: Mp4RangeLanding::Demux(&self.replay),
        }
    }
}

impl<S: Source> Player<S> {
    /// Reuse the complete Movie from the same open transaction. The seek job
    /// uses its compact sample tables and never reads payload bytes.
    pub fn prepare_mp4_demux_seek<'a>(
        &'a self,
        movie: &'a Movie,
        request: Mp4DemuxSeekRequest,
        selection: Selection,
    ) -> Result<Mp4DemuxSeekJob<'a>> {
        let Some(engine @ Engine::Mp4(_)) = self.engine.as_ref() else {
            return Err(Error::Invalid("demux seek requires MP4"));
        };
        self.check_native_seek_clock()?;
        Mp4DemuxSeekJob::new(engine, movie, request, selection, None)
    }

    /// Plans a demux-clock seek for a progressive player from `next`, a complete
    /// range index of the same entity opened separately, which the seek switches to.
    pub fn prepare_mp4_progressive_demux_seek<'a>(
        &self,
        next: &'a Mp4Catalogue<S>,
        movie: &'a Movie,
        request: Mp4DemuxSeekRequest,
        selection: Selection,
    ) -> Result<Mp4DemuxSeekJob<'a>> {
        self.check_native_seek_clock()?;
        let engine = self.progressive_seek_candidate(next)?;
        Mp4DemuxSeekJob::new(engine, movie, request, selection, Some(self.generation))
    }

    /// Applies `command` by replacing the progressive player's catalogue with `next`;
    /// `None` when `work` had no work left.
    pub fn apply_mp4_progressive_demux_seek(
        &mut self,
        next: &mut Option<Mp4Catalogue<S>>,
        command: &Mp4DemuxSeekCommand,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<RetiredCatalogue>> {
        self.prepare_progressive_commit(next, command.seek(), work, cancel)?
            .map(CatalogueCommit::commit)
            .transpose()
    }

    fn check_native_seek_clock(&self) -> Result<()> {
        if self.clock != ClockChoice::Demux {
            return Err(Error::Unsupported(
                "demux seek requires a complete MP4 range plan",
            ));
        }
        Ok(())
    }

    /// Applies `command`; `false` when `work` ran out before the seek started.
    pub fn apply_mp4_demux_seek(
        &mut self,
        command: &Mp4DemuxSeekCommand,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        self.apply_range_seek(command.seek(), work, cancel)
    }
}

impl<'a> Mp4DemuxSeekJob<'a> {
    fn new(
        engine: &'a Engine,
        movie: &'a Movie,
        request: Mp4DemuxSeekRequest,
        selection: Selection,
        progressive_generation: Option<u64>,
    ) -> Result<Self> {
        let Engine::Mp4(mp4) = engine else {
            return Err(Error::Invalid("demux seek requires MP4"));
        };
        let plan = &mp4.plan;
        if plan.view != PlanView::Demux
            || plan.read_scope != MetadataReadScope::CompleteEntity
            || plan.demux_recovery
            || !mp4.supports_range_replay()
        {
            return Err(Error::Unsupported(
                "demux seek requires a complete MP4 range plan",
            ));
        }
        if !movie.pool().same_pool(&plan.pool) {
            return Err(Error::ForeignPool);
        }
        if movie.source() != plan.source
            || movie.generation() != plan.opening_generation
            || movie.read_scope() != plan.read_scope
        {
            return Err(Error::SourceMismatch);
        }
        selection.validate(engine)?;
        let selected = plan.track(request.video)?;
        if selected.snapshot.handler != *b"vide"
            || !selected.snapshot.enabled
            || !selection.includes(TrackKey::Mp4(request.video))
        {
            return Err(Error::Invalid(
                "demux seek requires a selected MP4 video track",
            ));
        }
        let base = MediaClock::from_source(selected.snapshot.base.require()?)?.demux_base;
        let target = Time::from_ticks(i128::from(request.timestamp), base)?;
        let global = native_global_seek(
            movie,
            plan.encoded_edits.as_ref().map(BudgetVec::as_slice),
            request,
            false,
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
    edits: Option<&'a [Option<EncodedEditPlan>]>,
    request: Mp4DemuxSeekRequest,
    media_tracks_only: bool,
) -> Result<GlobalSeekJob<'a>> {
    let tracks = movie.tracks();
    let maximum = request.limits.index.packets.max_tracks.min(64);
    if tracks.len() > maximum {
        return Err(Error::Limit {
            requested: tracks.len(),
            maximum,
        });
    }
    let mut native = BudgetVec::new(movie.pool());
    native.try_reserve_exact(tracks.len())?;
    let movie_base = movie.header().time_base()?;
    for track in tracks {
        native
            .try_push(DemuxTrack {
                track_id: track.header().track_id,
                table: track.table(),
                fragments: track.fragments(),
                media_base: track.header().time_base()?,
                edits: track.edits().map(|edits| (edits, movie_base)),
            })
            .map_err(|error| error.error)?;
    }
    Ok(if media_tracks_only {
        GlobalSeekJob::new_projected(
            movie.pool(),
            native.as_slice(),
            movie.media(),
            (request.video, request.timestamp),
            request.limits,
            edits,
            |ordinal| include_media_track(movie, ordinal),
        )?
    } else if let Some(plans) = edits {
        GlobalSeekJob::new_encoded_regular_rescaled(
            movie.pool(),
            native.as_slice(),
            movie.media(),
            (request.video, request.timestamp),
            request.limits,
            plans,
        )?
    } else {
        GlobalSeekJob::new_complete_source_rescaled(
            movie.pool(),
            native.as_slice(),
            movie.media(),
            request.video,
            request.timestamp,
            request.limits,
            DemuxProfile::NoParseNoFillin,
        )?
    })
}

impl Mp4DemuxSeekJob<'_> {
    /// Plans within `work`, and returns whether the plan is complete. A failure is final.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.scan(work, cancel);
        if let Err(error) = result {
            self.failure = Some(error);
            self.global = None;
        }
        result
    }

    fn scan(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
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
                .ok_or(Error::Invalid("demux seek planner missing"))?
                .finish()?;
            self.receipt = Some(global.landing().clone());
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
        let rows = self.plan.demux_rows();
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
                "demux seek occurrence absent from playback plan",
            ))?;
            let anchor = self.anchors[..self.tracks]
                .iter_mut()
                .find(|anchor| anchor.track == row.track_id)
                .ok_or(Error::Invalid("demux seek track absent from playback plan"))?;
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
            .ok_or(Error::Invalid("demux seek receipt missing"))?;
        for (i, anchor) in self.anchors[..self.tracks].iter().enumerate() {
            let row = &rows[anchor.row];
            if anchor.track == self.request.video
                && (row.source != receipt.selected.occurrence.source
                    || row.demux != receipt.selected.occurrence.demux
                    || row.edit != receipt.selected.occurrence.edit)
            {
                return Err(Error::Invalid(
                    "demux seek landing differs from playback plan",
                ));
            }
            tracks[i] = anchor.track;
            starts[i] = anchor.row;
        }
        self.replay = Some(DemuxReplay::new(
            &tracks[..self.tracks],
            &starts[..self.tracks],
            rows,
        )?);
        Ok(true)
    }

    /// The seek command. Fails with the job's failure, or with `Invalid` before the plan
    /// is complete.
    pub fn finish(self) -> Result<Mp4DemuxSeekCommand> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let replay = self
            .replay
            .ok_or(Error::Invalid("playback demux seek still pending"))?;
        let offset = self.plan.demux_rows()[replay.first_row()]
            .source
            .range
            .start;
        Ok(Mp4DemuxSeekCommand {
            pool: self.plan.pool.clone(),
            generation: self.generation,
            progressive_generation: self.progressive_generation,
            video: self.request.video,
            target: self.target,
            selection: self.selection,
            replay,
            receipt: self
                .receipt
                .ok_or(Error::Invalid("demux seek receipt missing"))?,
            offset,
        })
    }
}
