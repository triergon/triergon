//! Range seeks through the checked physical plan in the exact source clock.

use super::{ClockChoice, Player, SeekIntent, Selection, TrackKey};
use crate::{
    Engine, Result,
    catalogue::{CatalogueCommit, Mp4Catalogue, ProgressiveSeek, RetiredCatalogue},
    engines::{Mp4RangeLanding, Mp4SyncLanding},
    input::Source,
    mp4_plan::{Mp4View, PreparedMp4, SourceRow},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, Rounding, Time, TimeBase, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct Mp4SyncSeekRequest {
    pub video: u32,
    pub source_time: Time,
    /// Preserve this much audio decode time before the video sync point.
    pub audio_preroll: Time,
    /// Rows inspected across both the video and prefix scans.
    pub maximum_scanned_rows: usize,
}

#[derive(Clone, Copy)]
struct SyncPoint {
    row: usize,
    cts: i128,
}

enum Phase {
    Video,
    Prefix { sync_row: usize, audio_from: Time },
    Ready(Mp4SyncLanding),
}

/// Borrows the existing checked rows and uses constant scratch. No input
/// requests are issued before the completed command is applied to the player.
pub struct Mp4SyncSeekJob<'a> {
    plan: &'a PreparedMp4,
    request: Mp4SyncSeekRequest,
    selection: Selection,
    generation: u64,
    progressive_generation: Option<u64>,
    video_base: TimeBase,
    target_ticks: i128,
    phase: Phase,
    row: usize,
    scanned: usize,
    previous_video_ordinal: u64,
    description: Option<u32>,
    previous_sync: Option<SyncPoint>,
    target: Option<(i128, Option<SyncPoint>)>,
    failure: Option<Error>,
}

/// A checked landing in one player's current generation.
pub struct Mp4SyncSeekCommand {
    pool: Budget,
    generation: u64,
    progressive_generation: Option<u64>,
    selection: Selection,
    request: Mp4SyncSeekRequest,
    landing: Mp4SyncLanding,
    offset: u64,
    sync_ordinal: u64,
}

impl Mp4SyncSeekCommand {
    pub fn start_offset(&self) -> u64 {
        self.offset
    }
    pub fn sync_ordinal(&self) -> u64 {
        self.sync_ordinal
    }

    pub(crate) fn intent(&self) -> SeekIntent {
        SeekIntent {
            track: TrackKey::Mp4(self.request.video),
            target: self.request.source_time,
            clock: ClockChoice::SourceExact,
            replay_from_start: false,
        }
    }
}

impl<S: Source> Player<S> {
    pub fn prepare_mp4_sync_seek(
        &self,
        request: Mp4SyncSeekRequest,
        selection: Selection,
    ) -> Result<Mp4SyncSeekJob<'_>> {
        let Some(engine @ Engine::Mp4(_)) = self.engine.as_ref() else {
            return Err(Error::Invalid("indexed seek requires MP4"));
        };
        self.check_sync_seek_clock()?;
        Mp4SyncSeekJob::new(engine, request, selection, None)
    }

    /// Plan a range seek against an independently opened complete index of
    /// the same stable entity. The active progressive cursor stays readable.
    pub fn prepare_mp4_progressive_sync_seek<'a>(
        &self,
        next: &'a Mp4Catalogue<S>,
        request: Mp4SyncSeekRequest,
        selection: Selection,
    ) -> Result<Mp4SyncSeekJob<'a>> {
        self.check_sync_seek_clock()?;
        let engine = self.progressive_seek_candidate(next)?;
        Mp4SyncSeekJob::new(engine, request, selection, Some(self.generation))
    }

    /// Commit the prepared range landing and publish it as a seek. Reopen the
    /// byte-identical entity with an independent Source owner and the same
    /// SourceId. Only the old transport is closed on commit.
    pub fn apply_mp4_progressive_sync_seek(
        &mut self,
        next: &mut Option<Mp4Catalogue<S>>,
        command: &Mp4SyncSeekCommand,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<RetiredCatalogue>> {
        self.prepare_mp4_progressive_sync_commit(next, command, work, cancel)?
            .map(CatalogueCommit::commit)
            .transpose()
    }

    pub(crate) fn prepare_mp4_progressive_sync_commit<'a>(
        &'a mut self,
        next: &'a mut Option<Mp4Catalogue<S>>,
        command: &'a Mp4SyncSeekCommand,
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
                landing: Mp4RangeLanding::Source(command.landing),
            },
            work,
            cancel,
        )
    }

    fn check_sync_seek_clock(&self) -> Result<()> {
        if self.clock != ClockChoice::SourceExact {
            return Err(Error::Unsupported(
                "indexed seek needs exact range-backed MP4",
            ));
        }
        Ok(())
    }

    pub fn apply_mp4_sync_seek(
        &mut self,
        command: &Mp4SyncSeekCommand,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if command.progressive_generation.is_some() {
            return Err(Error::Invalid(
                "progressive seek needs its candidate catalogue",
            ));
        }
        if !self.pool.same_pool(&command.pool) {
            return Err(Error::ForeignPool);
        }
        if self.generation != command.generation {
            return Err(Error::StaleGeneration {
                expected: self.generation,
                received: command.generation,
            });
        }
        self.apply_seek(
            TrackKey::Mp4(command.request.video),
            command.request.source_time,
            command.selection,
            Some(Mp4RangeLanding::Source(command.landing)),
            work,
            cancel,
        )
    }
}

impl<'a> Mp4SyncSeekJob<'a> {
    fn new(
        engine: &'a Engine,
        request: Mp4SyncSeekRequest,
        selection: Selection,
        progressive_generation: Option<u64>,
    ) -> Result<Self> {
        let Engine::Mp4(mp4) = engine else {
            return Err(Error::Invalid("indexed seek requires MP4"));
        };
        if mp4.plan.view != Mp4View::SourcePhysical || !mp4.supports_range_replay() {
            return Err(Error::Unsupported(
                "indexed seek needs exact range-backed MP4",
            ));
        }
        if request.audio_preroll.numerator() < 0 || request.maximum_scanned_rows == 0 {
            return Err(Error::Invalid("indexed seek policy"));
        }
        selection.validate(engine)?;
        if !selection.includes(TrackKey::Mp4(request.video)) {
            return Err(Error::Invalid("indexed seek needs the selected video"));
        }
        let track = mp4.plan.track(request.video)?;
        if track.snapshot.handler != *b"vide" || !track.snapshot.enabled {
            return Err(Error::Invalid("indexed seek track is not enabled video"));
        }
        let video_base = track.snapshot.base.require()?;
        Ok(Self {
            plan: &mp4.plan,
            request,
            selection,
            generation: engine.generation(),
            progressive_generation,
            video_base,
            target_ticks: request.source_time.to_ticks(video_base, Rounding::Floor)?,
            phase: Phase::Video,
            row: 0,
            scanned: 0,
            previous_video_ordinal: 0,
            description: None,
            previous_sync: None,
            target: None,
            failure: None,
        })
    }
}

impl Mp4SyncSeekJob<'_> {
    /// Returns true once the landing is ready. A cancelled or failed plan
    /// cannot publish a command.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.scan(work, cancel);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }

    fn scan(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> triergon_core::Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let rows = self.plan.source_rows();
            match self.phase {
                Phase::Video => {
                    if let Some(row) = rows.get(self.row) {
                        if !self.charge_row(work)? {
                            return Ok(false);
                        }
                        if row.track_id == self.request.video {
                            self.observe_video(row)?;
                        }
                        self.row += 1;
                        continue;
                    }
                    let (_, Some(sync)) = self
                        .target
                        .ok_or(Error::Unsupported("no MP4 picture at or before seek time"))?
                    else {
                        return Err(Error::Unsupported("no MP4 sync in target configuration"));
                    };
                    let sync_time = Time::from_ticks(sync.cts, self.video_base)?;
                    let audio_from = sync_time
                        .min(self.request.source_time)
                        .checked_sub(self.request.audio_preroll)?;
                    self.row = 0;
                    self.phase = Phase::Prefix {
                        sync_row: sync.row,
                        audio_from,
                    };
                }
                Phase::Prefix {
                    sync_row,
                    audio_from,
                } => {
                    if self.row < sync_row {
                        if !self.charge_row(work)? {
                            return Ok(false);
                        }
                        if !self.keep_prefix(&rows[self.row], audio_from)? {
                            self.row += 1;
                            continue;
                        }
                    }
                    self.phase = Phase::Ready(Mp4SyncLanding {
                        first_row: self.row,
                        sync_row,
                    });
                }
                Phase::Ready(_) => return Ok(true),
            }
        }
    }

    fn charge_row(&mut self, work: &mut WorkBudget) -> triergon_core::Result<bool> {
        if !work.try_charge(1) {
            return Ok(false);
        }
        self.scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
        if self.scanned > self.request.maximum_scanned_rows {
            return Err(Error::Limit {
                requested: self.scanned,
                maximum: self.request.maximum_scanned_rows,
            });
        }
        Ok(true)
    }

    fn observe_video(&mut self, row: &SourceRow) -> triergon_core::Result<()> {
        let sample = &row.sample;
        if self.previous_video_ordinal.checked_add(1) != Some(sample.ordinal) {
            return Err(Error::Unsupported(
                "MP4 indexed seek needs video decode order",
            ));
        }
        self.previous_video_ordinal = sample.ordinal;
        let cts = sample
            .cts_ticks
            .ok_or(Error::Unsupported("MP4 seek video time unknown"))?;
        if self.description != Some(sample.description_index) {
            self.previous_sync = None;
            self.description = Some(sample.description_index);
        }
        if sample.is_container_sync() {
            self.previous_sync = Some(SyncPoint { row: self.row, cts });
        }
        if cts <= self.target_ticks && self.target.is_none_or(|(old, _)| cts > old) {
            self.target = Some((cts, self.previous_sync));
        }
        Ok(())
    }

    fn keep_prefix(&self, row: &SourceRow, audio_from: Time) -> triergon_core::Result<bool> {
        if row.track_id == self.request.video
            || !self.selection.includes(TrackKey::Mp4(row.track_id))
        {
            return Ok(false);
        }
        let track = self
            .plan
            .tracks()
            .iter()
            .find(|track| track.snapshot.track_id == row.track_id)
            .ok_or(Error::Invalid("MP4 seek prefix track"))?;
        let sample = &row.sample;
        // Preserve prior selected subtitle and secondary video state, and
        // audio whose decode extent cannot be located exactly.
        let (Some(cts), Ok(base)) = (sample.cts_ticks, track.snapshot.base.require()) else {
            return Ok(true);
        };
        if track.snapshot.handler != *b"soun"
            || sample.dts_ticks != Some(cts)
            || sample.duration_ticks == 0
        {
            return Ok(true);
        }
        let end = cts
            .checked_add(i128::from(sample.duration_ticks))
            .ok_or(Error::Overflow)?;
        Ok(Time::from_ticks(end, base)? > audio_from)
    }

    pub fn finish(self) -> Result<Mp4SyncSeekCommand> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let Phase::Ready(landing) = self.phase else {
            return Err(Error::Invalid("MP4 indexed seek still pending"));
        };
        let rows = self.plan.source_rows();
        Ok(Mp4SyncSeekCommand {
            pool: self.plan.pool.clone(),
            generation: self.generation,
            progressive_generation: self.progressive_generation,
            selection: self.selection,
            request: self.request,
            landing,
            offset: rows[landing.first_row].sample.range.start,
            sync_ordinal: rows[landing.sync_row].sample.ordinal,
        })
    }
}
