//! Seek plans by movie time. [`MovieEditLocator`] maps a movie time through a track's
//! edit list to a media time, [`SourceCompositionLookup`] finds the sample presented
//! then, and [`MovieSourceSeekPlanner`] plans a seek to it with the preroll the sync
//! sample's recovery group states. Times are exact; samples are not repeated, reordered
//! or given display durations as a demuxer would.
//!
//! Specification: Apple QuickTime File Format (edit lists and composition times).
use crate::{
    BuildStatus, Next, Sample,
    interleave::{SourceTrack, TrackSampleCursor},
    movie::{EditEntry, EditList},
    sample_groups::{
        RecoveryKind, RecoveryLimits, RecoveryLookup, RecoveryScope, RecoverySelection,
    },
    seek::{SourceSeekLimits, SourceSeekPlan, SourceSeekPlanner, SourceSeekRequest},
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Error, Result, Rounding, Time, TimeBase, WorkBudget};

/// The time bases a movie-time seek converts between.
#[derive(Clone, Copy, Debug)]
pub struct MovieSeekClocks {
    /// The movie timescale (`mvhd`), in which edit durations count.
    pub movie_base: TimeBase,
    /// The track's media timescale (`mdhd`), in which edit media times and sample
    /// times count.
    pub media_base: TimeBase,
    /// End of the timeline of a track without an edit list, as the caller establishes
    /// it; must not be negative. Movie times at or after it are outside, and `None`
    /// leaves the timeline open. It is not derived from sample times or a stated
    /// duration.
    pub media_duration: Option<Time>,
}
/// A movie time that falls on media: the edit it falls in and the media time it maps
/// to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovieSourcePoint {
    /// The movie time asked for.
    pub requested_movie_time: Time,
    /// 1-based `elst` row of the edit the time falls in; `None` without an edit list.
    pub edit_ordinal: Option<u32>,
    /// That edit; `None` without an edit list.
    pub edit_entry: Option<EditEntry>,
    /// Movie time at which that edit starts; zero without an edit list.
    pub movie_start: Time,
    /// Movie time at which that edit ends; without an edit list,
    /// [`media_duration`](MovieSeekClocks::media_duration).
    pub movie_end: Option<Time>,
    /// The media time the movie time maps to at rate one: the edit's `media_time`
    /// plus the offset into the edit, or the movie time itself without an edit list.
    pub source_time: Time,
}
/// A movie time that falls in an empty edit (`media_time` −1), where the track
/// presents nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovieEmptyEdit {
    /// The movie time asked for.
    pub requested_movie_time: Time,
    /// 1-based `elst` row of the empty edit.
    pub edit_ordinal: u32,
    /// The empty edit.
    pub entry: EditEntry,
    /// The movie times the empty edit covers.
    pub movie_range: Range<Time>,
}
/// Where a movie time falls on one track's timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoviePoint {
    /// In an edit that presents media, or on the media timeline of a track without an
    /// edit list.
    Media(MovieSourcePoint),
    /// In an empty edit.
    Empty(MovieEmptyEdit),
    /// Before zero, after the last edit, or at or after
    /// [`media_duration`](MovieSeekClocks::media_duration) without an edit list.
    Outside {
        /// The movie time asked for.
        requested_movie_time: Time,
    },
}
fn count_limit(value: u64, maximum: u64) -> Result<()> {
    if value > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(value).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}

/// Finds where a movie time falls on one track's timeline, walking its edit list one
/// edit per work unit.
#[derive(Debug)]
pub struct MovieEditLocator<'a> {
    edits: Option<&'a EditList>,
    clocks: MovieSeekClocks,
    requested: Time,
    row: u32,
    movie_cursor: Time,
    ready: Option<MoviePoint>,
    failed: Option<Error>,
}
impl<'a> MovieEditLocator<'a> {
    /// A locator for the movie time `requested`; without `edits` the movie time is the
    /// media time. Fails with `Error::Limit` for more edits than `max_edits` and
    /// `Error::Invalid` for a negative
    /// [`media_duration`](MovieSeekClocks::media_duration).
    pub fn new(
        edits: Option<&'a EditList>,
        clocks: MovieSeekClocks,
        requested: Time,
        max_edits: u32,
    ) -> Result<Self> {
        if let Some(edits) = edits {
            count_limit(u64::from(edits.len()), u64::from(max_edits))?;
        }
        let zero = Time::new(0, 1)?;
        if clocks.media_duration.is_some_and(|end| end < zero) {
            return Err(Error::Invalid("negative identity timeline bound"));
        }
        Ok(Self {
            edits,
            clocks,
            requested,
            row: 0,
            movie_cursor: zero,
            ready: None,
            failed: None,
        })
    }
    /// Advances the walk. Fails with `Error::Unsupported` for an edit list with no
    /// entries, a zero-duration edit at or before the time, or a rate other than one on the
    /// edit it falls in. An error is kept and returned by every later poll.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn outside(&mut self) {
        self.ready = Some(MoviePoint::Outside {
            requested_movie_time: self.requested,
        });
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.ready.is_some() {
                return Ok(BuildStatus::Ready);
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            if self.requested < Time::new(0, 1)? {
                self.outside();
                continue;
            }
            let Some(edits) = self.edits else {
                if self
                    .clocks
                    .media_duration
                    .is_some_and(|end| self.requested >= end)
                {
                    self.outside();
                } else {
                    self.ready = Some(MoviePoint::Media(MovieSourcePoint {
                        requested_movie_time: self.requested,
                        edit_ordinal: None,
                        edit_entry: None,
                        movie_start: self.movie_cursor,
                        movie_end: self.clocks.media_duration,
                        source_time: self.requested,
                    }));
                }
                continue;
            };
            if edits.is_empty() {
                return Err(Error::Unsupported(
                    "present-empty edit list timeline remains unresolved",
                ));
            }
            if self.row == edits.len() {
                self.outside();
                continue;
            }
            let entry = edits.entry(self.row)?;
            if entry.segment_duration == 0 {
                return Err(Error::Unsupported(
                    "zero-duration edit timeline remains unresolved",
                ));
            }
            let end = self
                .movie_cursor
                .checked_add(entry.duration(self.clocks.movie_base)?)?;
            if self.requested >= end {
                self.movie_cursor = end;
                self.row = self.row.checked_add(1).ok_or(Error::Overflow)?;
                continue;
            }
            let ordinal = self.row.checked_add(1).ok_or(Error::Overflow)?;
            if entry.is_empty() {
                self.ready = Some(MoviePoint::Empty(MovieEmptyEdit {
                    requested_movie_time: self.requested,
                    edit_ordinal: ordinal,
                    entry,
                    movie_range: self.movie_cursor..end,
                }));
            } else {
                if entry.rate_integer != 1 || entry.rate_fraction != 0 {
                    return Err(Error::Unsupported("non-unit movie seek rate"));
                }
                let source_start =
                    Time::from_ticks(i128::from(entry.media_time), self.clocks.media_base)?;
                let source_time =
                    source_start.checked_add(self.requested.checked_sub(self.movie_cursor)?)?;
                self.ready = Some(MoviePoint::Media(MovieSourcePoint {
                    requested_movie_time: self.requested,
                    edit_ordinal: Some(ordinal),
                    edit_entry: Some(entry),
                    movie_start: self.movie_cursor,
                    movie_end: Some(end),
                    source_time,
                }));
            }
        }
    }
    /// The point, once [`poll`](Self::poll) has returned `Ready`; `Error::Invalid`
    /// before that, or the error a poll returned.
    pub fn finish(mut self) -> Result<MoviePoint> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("source movie edit lookup not ready"))
    }
}

/// The sample a media time selects: the one presented latest at or before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompositionTarget {
    /// The selected sample.
    Target(Sample),
    /// Every sample is presented after the time, or the track has no samples.
    BeforeFirst,
    /// A sample's presentation time is unknown, as in a fragment without `tfdt`, so no
    /// sample can be selected.
    UnknownSourceTime {
        /// 1-based ordinal of the first sample with an unknown presentation time.
        ordinal: u64,
    },
    /// Two samples share the latest presentation time at or before the time.
    Ambiguous {
        /// 1-based ordinal of the first sample with that presentation time.
        first_ordinal: u64,
        /// 1-based ordinal of the next sample with that presentation time.
        second_ordinal: u64,
        /// Their presentation time, on the media timeline.
        source_time: Time,
    },
}
/// Finds the sample of a track presented latest at or before a media time, scanning
/// every sample in ordinal order.
#[derive(Debug)]
pub struct SourceCompositionLookup<'a> {
    cursor: TrackSampleCursor<'a>,
    requested: Time,
    media_base: TimeBase,
    /// The latest tick at or before the request where whole media ticks are
    /// exact times, so that they compare as integers.
    latest_tick: Option<i128>,
    best_tick: Option<i128>,
    maximum: u64,
    scanned: u64,
    last_ordinal: Option<u64>,
    best: Option<Sample>,
    best_time: Option<Time>,
    tied: Option<u64>,
    unknown: Option<u64>,
    done: bool,
    failed: Option<Error>,
}
impl<'a> SourceCompositionLookup<'a> {
    /// A lookup for the media time `requested` on `source`, whose samples count in
    /// `media_base`. Fails with `Error::Invalid` for a sample table whose timing runs
    /// do not cover its samples exactly; polling fails with `Error::Limit` past
    /// `max_samples` scanned samples.
    pub fn new(
        source: SourceTrack<'a>,
        requested: Time,
        media_base: TimeBase,
        max_samples: u64,
    ) -> Result<Self> {
        source.require_exact_timing()?;
        Ok(Self {
            cursor: TrackSampleCursor::new(source),
            requested,
            media_base,
            latest_tick: (media_base.numerator() == 1)
                .then(|| requested.to_ticks(media_base, Rounding::Floor).ok())
                .flatten(),
            best_tick: None,
            maximum: max_samples,
            scanned: 0,
            last_ordinal: None,
            best: None,
            best_time: None,
            tied: None,
            unknown: None,
            done: false,
            failed: None,
        })
    }
    /// Advances the scan. Fails with `Error::Invalid` when sample ordinals do not
    /// increase from 1; an error is kept and returned by every later poll.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.done {
                return Ok(BuildStatus::Ready);
            }
            let sample = match self.cursor.poll(work)? {
                Next::Yield => return Ok(BuildStatus::Yield),
                Next::End => {
                    self.done = true;
                    continue;
                }
                Next::Item(sample) => sample,
            };
            let scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
            count_limit(scanned, self.maximum)?;
            if sample.ordinal == 0 || self.last_ordinal.is_some_and(|last| sample.ordinal <= last) {
                return Err(Error::Invalid("composition source ordinal order"));
            }
            self.scanned = scanned;
            self.last_ordinal = Some(sample.ordinal);
            let Some(ticks) = sample.cts_ticks else {
                if self.unknown.is_none() {
                    self.unknown = Some(sample.ordinal);
                }
                continue;
            };
            if let Some(latest) = self.latest_tick {
                // Ticks of a unit-numerator base order as their times do.
                if ticks > latest {
                    continue;
                }
                match self.best_tick {
                    Some(best) if ticks < best => {}
                    Some(best) if ticks == best => {
                        if self.tied.is_none() {
                            self.tied = Some(sample.ordinal);
                        }
                    }
                    _ => {
                        self.best_tick = Some(ticks);
                        self.best = Some(sample);
                        self.tied = None;
                    }
                }
                continue;
            }
            let cts = Time::from_ticks(ticks, self.media_base)?;
            if cts > self.requested {
                continue;
            }
            match self.best_time {
                Some(best) if cts < best => {}
                Some(best) if cts == best => {
                    if self.tied.is_none() {
                        self.tied = Some(sample.ordinal);
                    }
                }
                _ => {
                    self.best_time = Some(cts);
                    self.best = Some(sample);
                    self.tied = None;
                }
            }
        }
    }
    /// The selection, once [`poll`](Self::poll) has returned `Ready`; `Error::Invalid`
    /// before that, or the error a poll returned. An unknown presentation time takes
    /// precedence over any selected sample.
    pub fn finish(mut self) -> Result<CompositionTarget> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.done {
            return Err(Error::Invalid("source composition lookup not ready"));
        }
        if let Some(ordinal) = self.unknown {
            return Ok(CompositionTarget::UnknownSourceTime { ordinal });
        }
        let Some(sample) = self.best.take() else {
            return Ok(CompositionTarget::BeforeFirst);
        };
        if let Some(ticks) = self.best_tick {
            self.best_time = Some(Time::from_ticks(ticks, self.media_base)?);
        }
        if let Some(second_ordinal) = self.tied {
            return Ok(CompositionTarget::Ambiguous {
                first_ordinal: sample.ordinal,
                second_ordinal,
                source_time: self
                    .best_time
                    .ok_or(Error::Invalid("missing composition candidate time"))?,
            });
        }
        Ok(CompositionTarget::Target(sample))
    }
}

/// Bounds on the work of a [`MovieSourceSeekPlanner`]; exceeding one fails with
/// `Error::Limit`. The defaults allow 4096 edits, 1,000,000 scanned samples and 4096
/// recovery scopes, with the default [`SourceSeekLimits`] and [`RecoveryLimits`].
#[derive(Clone, Copy, Debug)]
pub struct MovieSeekLimits {
    /// Most edit list entries walked.
    pub max_edits: u32,
    /// Most samples scanned to find the target and its sync sample.
    pub max_scanned_samples: u64,
    /// Most sample group scopes searched for recovery groups.
    pub max_recovery_scopes: usize,
    /// Bounds of the source seek plan the planner builds.
    pub source: SourceSeekLimits,
    /// Bounds of reading the recovery groups.
    pub recovery: RecoveryLimits,
}
impl Default for MovieSeekLimits {
    fn default() -> Self {
        Self {
            max_edits: 4096,
            max_scanned_samples: 1_000_000,
            max_recovery_scopes: 4096,
            source: SourceSeekLimits::default(),
            recovery: RecoveryLimits::default(),
        }
    }
}
/// The track a [`MovieSourceSeekPlanner`] seeks on, with its edits, clocks and
/// recovery groups.
#[derive(Clone, Copy, Debug)]
pub struct MovieSourceSeekContext<'a> {
    /// The track's sample table and fragments.
    pub source: SourceTrack<'a>,
    /// The track's edit list; `None` when it has none.
    pub edits: Option<&'a EditList>,
    /// The movie and media time bases.
    pub clocks: MovieSeekClocks,
    /// Absolute byte ranges of the file's media, in which every planned sample must
    /// lie.
    pub media_extents: &'a [Range<u64>],
    /// The recovery group, `roll` or `prol`, whose distance sets the preroll.
    pub recovery_kind: RecoveryKind,
    /// The scopes of the track's recovery groups: the sample table and each fragment.
    /// Scopes of another kind are skipped.
    pub recovery_scopes: &'a [RecoveryScope<'a>],
}
/// How a movie-time seek ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovieSeekStatus {
    /// A plan was made.
    Planned,
    /// The time falls in an empty edit.
    EmptyEdit {
        /// 1-based `elst` row of the empty edit.
        edit_ordinal: u32,
    },
    /// The time is outside the track's timeline.
    OutsideTimeline,
    /// Every sample is presented after the media time the movie time maps to.
    BeforeFirstSample,
    /// A sample's presentation time is unknown, so no sample can be selected.
    UnknownSourceTime {
        /// 1-based ordinal of the first sample with an unknown presentation time.
        ordinal: u64,
    },
    /// Two samples share the latest presentation time at or before the media time.
    AmbiguousComposition {
        /// 1-based ordinal of the first sample with that presentation time.
        first_ordinal: u64,
        /// 1-based ordinal of the next sample with that presentation time.
        second_ordinal: u64,
    },
}
/// The outcome of a movie-time seek.
#[derive(Debug)]
pub struct MovieSeekResult<'a> {
    /// How the seek ended.
    pub status: MovieSeekStatus,
    /// The media point the movie time maps to; `None` for an empty edit or a time
    /// outside the timeline.
    pub point: Option<MovieSourcePoint>,
    /// The empty edit, when the status is [`EmptyEdit`](MovieSeekStatus::EmptyEdit).
    pub gap: Option<MovieEmptyEdit>,
    /// The recovery group entry at the sync sample, which set the preroll; present
    /// when the status is [`Planned`](MovieSeekStatus::Planned).
    pub recovery: Option<RecoverySelection>,
    /// The seek plan, present when the status is
    /// [`Planned`](MovieSeekStatus::Planned).
    pub plan: Option<SourceSeekPlan<'a>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Edit,
    Composition,
    InitialSeek,
    Scopes,
    Recovery,
    FinalSeek,
    Ready,
}

/// Plans a seek to a movie time on one track. It maps the time through the edit list,
/// finds the sample presented then, plans a seek to it, and reads the recovery group
/// entry of the sync sample it lands on. A distance of zero, or no entry, keeps that
/// plan; otherwise the plan is made again with as many preroll samples as the distance
/// states.
#[derive(Debug)]
pub struct MovieSourceSeekPlanner<'a> {
    context: MovieSourceSeekContext<'a>,
    limits: MovieSeekLimits,
    phase: Phase,
    edit: Option<MovieEditLocator<'a>>,
    composition: Option<SourceCompositionLookup<'a>>,
    seek: Option<SourceSeekPlanner<'a>>,
    recovery_lookup: Option<RecoveryLookup<'a>>,
    provisional: Option<SourceSeekPlan<'a>>,
    point: Option<MovieSourcePoint>,
    recovery: Option<RecoverySelection>,
    target_ordinal: u64,
    sync_ordinal: u64,
    scope: usize,
    saw_family: bool,
    chosen_scope: Option<RecoveryScope<'a>>,
    result: Option<MovieSeekResult<'a>>,
    failed: Option<Error>,
}
impl<'a> MovieSourceSeekPlanner<'a> {
    /// A planner for the movie time `requested`. Fails with `Error::Invalid` for a
    /// sample table whose timing runs do not cover its samples exactly, `Error::Limit`
    /// for more recovery scopes than `limits` allow, and as [`MovieEditLocator::new`].
    pub fn new(
        context: MovieSourceSeekContext<'a>,
        requested: Time,
        limits: MovieSeekLimits,
    ) -> Result<Self> {
        context.source.require_exact_timing()?;
        if context.recovery_scopes.len() > limits.max_recovery_scopes {
            return Err(Error::Limit {
                requested: context.recovery_scopes.len(),
                maximum: limits.max_recovery_scopes,
            });
        }
        let edit =
            MovieEditLocator::new(context.edits, context.clocks, requested, limits.max_edits)?;
        Ok(Self {
            context,
            limits,
            phase: Phase::Edit,
            edit: Some(edit),
            composition: None,
            seek: None,
            recovery_lookup: None,
            provisional: None,
            point: None,
            recovery: None,
            target_ordinal: 0,
            sync_ordinal: 0,
            scope: 0,
            saw_family: false,
            chosen_scope: None,
            result: None,
            failed: None,
        })
    }
    /// Advances the plan. Fails as the jobs it runs do, with `Error::Unsupported` when
    /// the sync sample lies outside every scope of the recovery kind or its distance is
    /// positive, and with `Error::Invalid` when two such scopes hold it. An error is
    /// kept and returned by every later poll.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn settle(
        &mut self,
        status: MovieSeekStatus,
        gap: Option<MovieEmptyEdit>,
        plan: Option<SourceSeekPlan<'a>>,
    ) {
        self.result = Some(MovieSeekResult {
            status,
            point: self.point.take(),
            gap,
            recovery: self.recovery.take(),
            plan,
        });
        self.phase = Phase::Ready;
    }
    fn apply_recovery(&mut self, selection: RecoverySelection) -> Result<()> {
        let preceding = selection.preceding_samples()?;
        self.recovery = Some(selection);
        if preceding == 0 {
            let plan = self
                .provisional
                .take()
                .ok_or(Error::Invalid("missing provisional source plan"))?;
            self.settle(MovieSeekStatus::Planned, None, Some(plan));
        } else {
            self.seek = Some(SourceSeekPlanner::new(
                self.context.source,
                SourceSeekRequest {
                    target_ordinal: self.target_ordinal,
                    preroll_samples: preceding,
                },
                self.context.media_extents,
                self.limits.source,
            )?);
            self.provisional = None;
            self.phase = Phase::FinalSeek;
        }
        Ok(())
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Edit => {
                    if self
                        .edit
                        .as_mut()
                        .ok_or(Error::Invalid("missing movie point job"))?
                        .poll(work, cancel)?
                        == BuildStatus::Yield
                    {
                        return Ok(BuildStatus::Yield);
                    }
                    match self
                        .edit
                        .take()
                        .ok_or(Error::Invalid("missing movie point job"))?
                        .finish()?
                    {
                        MoviePoint::Media(point) => {
                            self.composition = Some(SourceCompositionLookup::new(
                                self.context.source,
                                point.source_time,
                                self.context.clocks.media_base,
                                self.limits.max_scanned_samples,
                            )?);
                            self.point = Some(point);
                            self.phase = Phase::Composition;
                        }
                        MoviePoint::Empty(gap) => self.settle(
                            MovieSeekStatus::EmptyEdit {
                                edit_ordinal: gap.edit_ordinal,
                            },
                            Some(gap),
                            None,
                        ),
                        MoviePoint::Outside { .. } => {
                            self.settle(MovieSeekStatus::OutsideTimeline, None, None);
                        }
                    }
                }
                Phase::Composition => {
                    if self
                        .composition
                        .as_mut()
                        .ok_or(Error::Invalid("missing composition job"))?
                        .poll(work, cancel)?
                        == BuildStatus::Yield
                    {
                        return Ok(BuildStatus::Yield);
                    }
                    match self
                        .composition
                        .take()
                        .ok_or(Error::Invalid("missing composition job"))?
                        .finish()?
                    {
                        CompositionTarget::Target(sample) => {
                            self.target_ordinal = sample.ordinal;
                            self.seek = Some(SourceSeekPlanner::new(
                                self.context.source,
                                SourceSeekRequest {
                                    target_ordinal: sample.ordinal,
                                    preroll_samples: 0,
                                },
                                self.context.media_extents,
                                self.limits.source,
                            )?);
                            self.phase = Phase::InitialSeek;
                        }
                        CompositionTarget::BeforeFirst => {
                            self.settle(MovieSeekStatus::BeforeFirstSample, None, None);
                        }
                        CompositionTarget::UnknownSourceTime { ordinal } => {
                            self.settle(MovieSeekStatus::UnknownSourceTime { ordinal }, None, None);
                        }
                        CompositionTarget::Ambiguous {
                            first_ordinal,
                            second_ordinal,
                            ..
                        } => self.settle(
                            MovieSeekStatus::AmbiguousComposition {
                                first_ordinal,
                                second_ordinal,
                            },
                            None,
                            None,
                        ),
                    }
                }
                Phase::InitialSeek | Phase::FinalSeek => {
                    if self
                        .seek
                        .as_mut()
                        .ok_or(Error::Invalid("missing source seek job"))?
                        .poll(work, cancel)?
                        == BuildStatus::Yield
                    {
                        return Ok(BuildStatus::Yield);
                    }
                    let plan = self
                        .seek
                        .take()
                        .ok_or(Error::Invalid("missing source seek job"))?
                        .finish()?;
                    if self.phase == Phase::InitialSeek {
                        self.sync_ordinal = plan.landing().sync_sample.ordinal;
                        self.provisional = Some(plan);
                        self.phase = Phase::Scopes;
                    } else {
                        self.settle(MovieSeekStatus::Planned, None, Some(plan));
                    }
                }
                Phase::Scopes => {
                    if let Some(scope) = self.context.recovery_scopes.get(self.scope).copied() {
                        if !work.try_charge(1) {
                            return Ok(BuildStatus::Yield);
                        }
                        if scope.kind == self.context.recovery_kind {
                            scope.validate(self.limits.recovery)?;
                            self.saw_family = true;
                            if self.sync_ordinal >= scope.first_ordinal
                                && self.sync_ordinal < scope.end()?
                            {
                                if self.chosen_scope.is_some() {
                                    return Err(Error::Invalid("overlapping recovery scopes"));
                                }
                                self.chosen_scope = Some(scope);
                            }
                        }
                        self.scope = self.scope.checked_add(1).ok_or(Error::Overflow)?;
                    } else if let Some(scope) = self.chosen_scope {
                        self.recovery_lookup = Some(RecoveryLookup::new(
                            scope,
                            self.sync_ordinal,
                            self.limits.recovery,
                        )?);
                        self.phase = Phase::Recovery;
                    } else {
                        if self.saw_family {
                            return Err(Error::Unsupported("sync sample outside recovery scopes"));
                        }
                        self.apply_recovery(RecoverySelection::absent(self.context.recovery_kind))?;
                    }
                }
                Phase::Recovery => {
                    if self
                        .recovery_lookup
                        .as_mut()
                        .ok_or(Error::Invalid("missing recovery job"))?
                        .poll(work, cancel)?
                        == BuildStatus::Yield
                    {
                        return Ok(BuildStatus::Yield);
                    }
                    let selection = self
                        .recovery_lookup
                        .take()
                        .ok_or(Error::Invalid("missing recovery job"))?
                        .finish()?;
                    self.apply_recovery(selection)?;
                }
                Phase::Ready => return Ok(BuildStatus::Ready),
            }
        }
    }
    /// The result, once [`poll`](Self::poll) has returned `Ready`; `Error::Invalid`
    /// before that, or the error a poll returned.
    pub fn finish(mut self) -> Result<MovieSeekResult<'a>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("movie recovery seek not ready"));
        }
        self.result
            .take()
            .ok_or(Error::Invalid("missing movie recovery seek result"))
    }
}
