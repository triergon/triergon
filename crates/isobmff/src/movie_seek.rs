//! Exact source movie/edit point mapping and recovery-aware source range plans.
//! Apple QTFF point conversion/CTS and original compact indexes; no native
//! occurrence projection, display-duration inference or packet sorting.
use crate::{
    BuildStatus, EditEntry, EditList, RecoveryKind, RecoveryLimits, RecoveryLookup, RecoveryScope,
    RecoverySelection, Sample, SampleStep, SourceSeekLimits, SourceSeekPlan, SourceSeekPlanner,
    SourceSeekRequest, SourceTrack, TrackSampleCursor,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Error, Result, Time, TimeBase, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct MovieSeekClocks {
    pub movie_base: TimeBase,
    pub media_base: TimeBase,
    /// Caller-established identity-timeline end, not inferred CTS/display
    /// coverage or an interpretation of a raw unknown-duration sentinel.
    pub media_duration: Option<Time>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovieSourcePoint {
    pub requested_movie_time: Time,
    pub edit_ordinal: Option<u32>,
    pub edit_entry: Option<EditEntry>,
    pub movie_start: Time,
    pub movie_end: Option<Time>,
    pub source_time: Time,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovieEmptyEdit {
    pub requested_movie_time: Time,
    pub edit_ordinal: u32,
    pub entry: EditEntry,
    pub movie_range: Range<Time>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoviePoint {
    Media(MovieSourcePoint),
    Empty(MovieEmptyEdit),
    Outside { requested_movie_time: Time },
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
    pub fn finish(mut self) -> Result<MoviePoint> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("source movie edit lookup not ready"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompositionTarget {
    Target(Sample),
    BeforeFirst,
    UnknownSourceTime {
        ordinal: u64,
    },
    Ambiguous {
        first_ordinal: u64,
        second_ordinal: u64,
        source_time: Time,
    },
}
#[derive(Debug)]
pub struct SourceCompositionLookup<'a> {
    cursor: TrackSampleCursor<'a>,
    requested: Time,
    media_base: TimeBase,
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
                SampleStep::Yield => return Ok(BuildStatus::Yield),
                SampleStep::End => {
                    self.done = true;
                    continue;
                }
                SampleStep::Sample(sample) => sample,
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

#[derive(Clone, Copy, Debug)]
pub struct MovieSeekLimits {
    pub max_edits: u32,
    pub max_scanned_samples: u64,
    pub max_recovery_scopes: usize,
    pub source: SourceSeekLimits,
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
#[derive(Clone, Copy, Debug)]
pub struct MovieSourceSeekContext<'a> {
    pub source: SourceTrack<'a>,
    pub edits: Option<&'a EditList>,
    pub clocks: MovieSeekClocks,
    pub media_extents: &'a [Range<u64>],
    pub recovery_kind: RecoveryKind,
    pub recovery_scopes: &'a [RecoveryScope<'a>],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovieSeekStatus {
    Planned,
    EmptyEdit {
        edit_ordinal: u32,
    },
    OutsideTimeline,
    BeforeFirstSample,
    UnknownSourceTime {
        ordinal: u64,
    },
    AmbiguousComposition {
        first_ordinal: u64,
        second_ordinal: u64,
    },
}
#[derive(Debug)]
pub struct MovieSeekResult<'a> {
    pub status: MovieSeekStatus,
    pub point: Option<MovieSourcePoint>,
    pub gap: Option<MovieEmptyEdit>,
    pub recovery: Option<RecoverySelection>,
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

/// Composes existing jobs without another sample/index array. The initial
/// source plan identifies the sync sample; recovery then informs the final plan.
/// A zero requirement reuses the validated initial plan without a second pass.
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
                        self.sync_ordinal = plan.receipt().sync_sample.ordinal;
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
