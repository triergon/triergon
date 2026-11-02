//! Pinned MOV n9.0 compatibility occurrence view for the observed strict,
//! nonreordered, positive-delta, rate-one regular sample profile.
//!
//! Apple/Adobe establish edit point mapping, gaps and repetition. Native key-end
//! postroll and expanded-index terminal duration are separate observation rules.
//! This cursor never changes the unique source index or injects expected packets.
use crate::{
    Access, EditList, Mov9ClockField, Mov9Edit, Mov9PacketView, Mov9View, Sample, SampleCursor,
    SampleStep, SampleTable,
};
use triergon_core::{Error, Result, Rounding, Time, TimeBase, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct Mov9EditLimits {
    pub max_occurrences: u64,
    pub max_scanned_samples: u64,
}
impl Default for Mov9EditLimits {
    fn default() -> Self {
        Self {
            max_occurrences: 1_000_000,
            max_scanned_samples: 1_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9DurationBasis {
    TerminalEmptyEndpointObservation,
    /// Pinned native packet ordinal still consumes the original decode table.
    OriginalDecodeTableOrdinal,
    FollowingOccurrenceDts,
    SourceDeltaAtBackwardJump,
    SourceDeltaAtTerminal,
    /// Provisional pinned-output rule, not a source duration or standards rule.
    ExpandedTerminalEndObservation,
}

#[derive(Clone, Debug)]
pub struct Mov9Occurrence {
    pub sequence: u64,
    pub edit_ordinal: u32,
    pub source: Sample,
    pub native: Mov9PacketView,
    pub duration_basis: Mov9DurationBasis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9CursorStatus {
    Yield,
    Ready,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Advance,
    Find,
    Emit,
    Done,
}

/// Allocates no occurrence/sample array. Source scans and generated occurrences
/// have separate cumulative limits; each poll obeys the shared work budget.
/// The caller associates this table/edit list with the same source track.
#[derive(Debug)]
pub struct Mov9EditCursor<'a> {
    table: &'a SampleTable,
    edits: &'a EditList,
    media_base: TimeBase,
    movie_base: TimeBase,
    view: Mov9View,
    limits: Mov9EditLimits,
    phase: Phase,
    scan: SampleCursor<'a>,
    duration_scan: SampleCursor<'a>,
    staging: Option<Mov9Occurrence>,
    after_emit: Phase,
    next_edit: u32,
    movie_cursor: Time,
    native_movie_ticks: i128,
    native_source_start: Time,
    staged_empty_endpoint: bool,
    active: Option<Mov9Edit>,
    source_start: Time,
    source_end: Time,
    last_source_end: Option<Time>,
    preceding_sync: Option<u64>,
    first: u64,
    generated: u64,
    scanned: u64,
    pending: Option<Mov9Occurrence>,
    ready: Option<Mov9Occurrence>,
    failed: Option<Error>,
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
impl<'a> Mov9EditCursor<'a> {
    pub fn new(
        table: &'a SampleTable,
        edits: &'a EditList,
        media_base: TimeBase,
        movie_base: TimeBase,
        limits: Mov9EditLimits,
    ) -> Result<Self> {
        if table.sample_count() == 0 || edits.is_empty() || table.sync_entry_count() == Some(0) {
            return Err(Error::Unsupported(
                "native edited empty source/list/sync profile",
            ));
        }
        let zero = Time::new(0, 1)?;
        Ok(Self {
            table,
            edits,
            media_base,
            movie_base,
            view: Mov9View::new(media_base, table.sync_entry_count()),
            limits,
            phase: Phase::Advance,
            scan: table.cursor(),
            duration_scan: table.cursor(),
            staging: None,
            after_emit: Phase::Emit,
            next_edit: 0,
            movie_cursor: zero,
            native_movie_ticks: 0,
            native_source_start: zero,
            staged_empty_endpoint: false,
            active: None,
            source_start: zero,
            source_end: zero,
            last_source_end: None,
            preceding_sync: None,
            first: 0,
            generated: 0,
            scanned: 0,
            pending: None,
            ready: None,
            failed: None,
        })
    }
    pub fn take_occurrence(&mut self) -> Result<Mov9Occurrence> {
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready native occurrence"))
    }
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Mov9CursorStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn scan_sample(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        let step = self.scan.poll(work)?;
        if let SampleStep::Sample(sample) = &step {
            self.scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
            count_limit(self.scanned, self.limits.max_scanned_samples)?;
            if sample.composition_offset.is_some()
                || sample.duration_ticks == 0
                || sample.partial_sync
                || sample.dependency.is_some()
                || sample.access == Access::Unknown
                || sample.dts_ticks.is_none()
                || sample.dts_ticks != sample.cts_ticks
            {
                return Err(Error::Unsupported(
                    "native edit reordered/zero-delta/recovery profile",
                ));
            }
        }
        Ok(step)
    }
    fn begin_emit(&mut self) -> Result<()> {
        self.first = self.preceding_sync.ok_or(Error::Unsupported(
            "native edit has no preceding declared sync",
        ))?;
        self.scan = self.table.cursor();
        self.phase = Phase::Emit;
        Ok(())
    }
    fn terminal(&self, mut occurrence: Mov9Occurrence) -> Result<Mov9Occurrence> {
        if self.generated > self.table.sample_count() {
            let end = self.native_movie_ticks;
            let dts = i128::from(
                occurrence
                    .native
                    .dts
                    .ok_or(Error::Unsupported("native terminal DTS unknown"))?,
            );
            let full_end = dts
                .checked_add(i128::from(occurrence.native.duration))
                .ok_or(Error::Overflow)?;
            occurrence.native.duration = if end <= dts {
                0
            } else if end >= full_end {
                occurrence.native.duration
            } else {
                i64::try_from(end - dts).map_err(|_| Error::Overflow)?
            };
            occurrence.duration_basis = Mov9DurationBasis::ExpandedTerminalEndObservation;
        }
        Ok(occurrence)
    }
    fn poll_inner(&mut self, work: &mut WorkBudget) -> Result<Mov9CursorStatus> {
        if self.ready.is_some() {
            return Ok(Mov9CursorStatus::Ready);
        }
        loop {
            match self.phase {
                Phase::Advance => {
                    if self.next_edit == self.edits.len() {
                        self.phase = Phase::Done;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
                    }
                    let entry = self.edits.entry(self.next_edit)?;
                    if entry.rate_integer != 1 || entry.rate_fraction != 0 {
                        return Err(Error::Unsupported(
                            "native non-unit edit occurrence profile",
                        ));
                    }
                    // Source movie_cursor remains the exact rational edit sum.
                    // Native edits accumulate separately rounded media ticks.
                    let movie_start = Time::from_ticks(self.native_movie_ticks, self.media_base)?;
                    let duration = entry.duration(self.movie_base)?;
                    let native_delta = duration.to_ticks(self.media_base, Rounding::NearestAway)?;
                    self.native_movie_ticks = self
                        .native_movie_ticks
                        .checked_add(native_delta)
                        .ok_or(Error::Overflow)?;
                    self.native_source_start = Time::new(0, 1)?;
                    self.movie_cursor = self.movie_cursor.checked_add(duration)?;
                    self.next_edit = self.next_edit.checked_add(1).ok_or(Error::Overflow)?;
                    if entry.is_empty() {
                        if self.generated == 0 {
                            continue;
                        }
                        // Observed expanded terminal gap appends a native-only
                        // endpoint using the retained source identity. The raw
                        // empty edit is still an exact source gap. Other empty
                        // edit/recovery combinations remain unreviewed.
                        let previous = self
                            .pending
                            .as_ref()
                            .ok_or(Error::Invalid("native gap has no retained predecessor"))?;
                        let active = self
                            .active
                            .ok_or(Error::Invalid("native gap has no media edit"))?;
                        if self.next_edit != self.edits.len()
                            || self.generated <= self.table.sample_count()
                            || !previous.source.is_container_sync()
                            || previous.source.dts_ticks
                                != Some(i128::from(active.entry.media_time))
                        {
                            return Err(Error::Unsupported(
                                "native unreviewed nonleading empty edit profile",
                            ));
                        }
                        let mut endpoint = previous.clone();
                        self.generated = self.generated.checked_add(1).ok_or(Error::Overflow)?;
                        count_limit(self.generated, self.limits.max_occurrences)?;
                        endpoint.sequence = self.generated;
                        endpoint.edit_ordinal = self.next_edit;
                        endpoint.duration_basis =
                            Mov9DurationBasis::TerminalEmptyEndpointObservation;
                        let field = Mov9ClockField::from_exact_native_ticks(
                            movie_start.to_ticks(self.media_base, Rounding::NearestAway)?,
                        )?;
                        endpoint.native.dts_field = field;
                        endpoint.native.pts_field = field;
                        endpoint.native.dts = field.api_ticks();
                        endpoint.native.pts = field.api_ticks();
                        endpoint.native.duration = i64::from(endpoint.source.duration_ticks);
                        self.staging = Some(endpoint);
                        self.staged_empty_endpoint = true;
                        self.after_emit = Phase::Advance;
                        self.phase = Phase::Emit;
                        continue;
                    }
                    if entry.media_time < 0 || entry.segment_duration == 0 {
                        return Err(Error::Unsupported(
                            "native negative/zero-duration media edit profile",
                        ));
                    }
                    self.source_start =
                        Time::from_ticks(i128::from(entry.media_time), self.media_base)?;
                    self.source_end = self.source_start.checked_add(duration)?;
                    self.active = Some(Mov9Edit {
                        entry,
                        movie_start,
                        movie_base: self.movie_base,
                    });
                    self.scan = self.table.cursor();
                    self.last_source_end = None;
                    self.preceding_sync = None;
                    self.phase = Phase::Find;
                }
                Phase::Find => match self.scan_sample(work)? {
                    SampleStep::Yield => return Ok(Mov9CursorStatus::Yield),
                    SampleStep::End => {
                        if self
                            .last_source_end
                            .is_none_or(|end| self.source_start >= end)
                        {
                            return Err(Error::Unsupported(
                                "native edit starts outside known source samples",
                            ));
                        }
                        self.begin_emit()?;
                    }
                    SampleStep::Sample(sample) => {
                        let dts = Time::from_ticks(
                            sample
                                .dts_ticks
                                .ok_or(Error::Invalid("missing regular DTS"))?,
                            self.media_base,
                        )?;
                        if dts <= self.source_start {
                            self.native_source_start = dts;
                        }
                        self.last_source_end = Some(dts.checked_add(Time::from_ticks(
                            i128::from(sample.duration_ticks),
                            self.media_base,
                        )?)?);
                        if dts > self.source_start {
                            self.begin_emit()?;
                        } else if sample.is_container_sync() {
                            self.preceding_sync = Some(sample.ordinal);
                        }
                    }
                },
                Phase::Emit => {
                    if self.staging.is_none() {
                        let source = match self.scan_sample(work)? {
                            SampleStep::Yield => return Ok(Mov9CursorStatus::Yield),
                            SampleStep::End => {
                                self.phase = Phase::Advance;
                                continue;
                            }
                            SampleStep::Sample(source) => source,
                        };
                        if source.ordinal < self.first {
                            continue;
                        }
                        let active = self.active.ok_or(Error::Invalid("missing active edit"))?;
                        let end_ticks = source
                            .dts_ticks
                            .ok_or(Error::Invalid("missing source DTS"))?
                            .checked_add(i128::from(source.duration_ticks))
                            .ok_or(Error::Overflow)?;
                        self.after_emit = if source.is_container_sync()
                            && Time::from_ticks(end_ticks, self.media_base)? >= self.source_end
                        {
                            Phase::Advance
                        } else {
                            Phase::Emit
                        };
                        let native = self.view.project_native_edit(
                            &source,
                            active,
                            self.native_source_start,
                        )?;
                        self.staged_empty_endpoint = false;
                        self.generated = self.generated.checked_add(1).ok_or(Error::Overflow)?;
                        count_limit(self.generated, self.limits.max_occurrences)?;
                        self.staging = Some(Mov9Occurrence {
                            sequence: self.generated,
                            edit_ordinal: self.next_edit,
                            source,
                            native,
                            duration_basis: Mov9DurationBasis::SourceDeltaAtTerminal,
                        });
                    }
                    // One original decode-table row per packet ordinal until
                    // exhausted. This cursor is independent of the repeated
                    // source occurrence scan; neither source samples nor runs
                    // are expanded. Keep staging across a one-token work yield.
                    let delta = match self.duration_scan.poll(work)? {
                        SampleStep::Yield => return Ok(Mov9CursorStatus::Yield),
                        SampleStep::Sample(sample) => {
                            self.scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
                            count_limit(self.scanned, self.limits.max_scanned_samples)?;
                            Some(sample.duration_ticks)
                        }
                        SampleStep::End => None,
                    };
                    let mut next = self
                        .staging
                        .take()
                        .ok_or(Error::Invalid("missing staged native occurrence"))?;
                    if let Some(delta) = delta {
                        next.native.duration = i64::from(delta);
                        next.duration_basis = Mov9DurationBasis::OriginalDecodeTableOrdinal;
                    }
                    self.phase = self.after_emit;
                    if let Some(mut previous) = self.pending.take() {
                        if self.staged_empty_endpoint {
                            previous.duration_basis =
                                Mov9DurationBasis::TerminalEmptyEndpointObservation;
                        } else if previous.duration_basis
                            != Mov9DurationBasis::OriginalDecodeTableOrdinal
                        {
                            previous.duration_basis = if next.native.dts >= previous.native.dts {
                                Mov9DurationBasis::FollowingOccurrenceDts
                            } else {
                                Mov9DurationBasis::SourceDeltaAtBackwardJump
                            };
                            previous.native =
                                previous.native.with_following_dts(next.native.dts)?;
                        }
                        self.pending = Some(next);
                        self.ready = Some(previous);
                        return Ok(Mov9CursorStatus::Ready);
                    }
                    self.pending = Some(next);
                }
                Phase::Done => {
                    if self.pending.is_none() {
                        return Ok(Mov9CursorStatus::End);
                    }
                    if !work.try_charge(1) {
                        return Ok(Mov9CursorStatus::Yield);
                    }
                    let last = self
                        .pending
                        .take()
                        .ok_or(Error::Invalid("missing terminal occurrence"))?;
                    self.ready = Some(self.terminal(last)?);
                    return Ok(Mov9CursorStatus::Ready);
                }
            }
        }
    }
}
