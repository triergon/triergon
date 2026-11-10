//! Reference MOV compatibility occurrence view for the observed strict,
//! nonreordered, positive-delta, rate-one regular sample profile.
//!
//! Apple/Adobe establish edit point mapping, gaps and repetition. Demux key-end
//! postroll and expanded-index terminal duration are separate observation rules.
//! This cursor never changes the unique source index or injects expected packets.
use crate::{
    Access, ClockField, EditList, PacketProjector, PacketView, ProjectedEdit, Sample, SampleCursor,
    SampleStep, SampleTable,
};
use triergon_core::{Error, Result, Rounding, Time, TimeBase, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct EditLimits {
    pub max_occurrences: u64,
    pub max_scanned_samples: u64,
}
impl Default for EditLimits {
    fn default() -> Self {
        Self {
            max_occurrences: 1_000_000,
            max_scanned_samples: 1_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurationBasis {
    TerminalEmptyEndpointObservation,
    /// Reference demux packet ordinal still consumes the original decode table.
    OriginalDecodeTableOrdinal,
    /// A fragment clock offset keeps the duration declared by its run.
    FragmentDecodeDelta,
    FollowingOccurrenceDts,
    SourceDeltaAtBackwardJump,
    SourceDeltaAtTerminal,
    /// Provisional reference-output rule, not a source duration or standards rule.
    ExpandedTerminalEndObservation,
}

#[derive(Clone, Debug)]
pub struct EditOccurrence {
    pub sequence: u64,
    pub edit_ordinal: u32,
    pub source: Sample,
    pub demux: PacketView,
    pub duration_basis: DurationBasis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorStatus {
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
#[derive(Clone, Debug)]
pub struct EditCursor<'a> {
    table: &'a SampleTable,
    edits: &'a EditList,
    media_base: TimeBase,
    movie_base: TimeBase,
    view: PacketProjector,
    limits: EditLimits,
    phase: Phase,
    scan: SampleCursor<'a>,
    duration_scan: SampleCursor<'a>,
    staging: Option<EditOccurrence>,
    after_emit: Phase,
    next_edit: u32,
    movie_cursor: Time,
    demux_movie_ticks: i128,
    demux_source_start: Time,
    staged_empty_endpoint: bool,
    active: Option<ProjectedEdit>,
    active_ticks: Option<EditTicks>,
    source_start: Time,
    source_end: Time,
    last_source_end: Option<Time>,
    preceding_sync: Option<u64>,
    first: u64,
    generated: u64,
    scanned: u64,
    pending: Option<EditOccurrence>,
    ready: Option<EditOccurrence>,
    failed: Option<Error>,
}

#[derive(Clone, Copy, Debug)]
struct EditTicks {
    shift: i64,
    start: i64,
    end: i64,
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
impl<'a> EditCursor<'a> {
    pub fn new(
        table: &'a SampleTable,
        edits: &'a EditList,
        media_base: TimeBase,
        movie_base: TimeBase,
        limits: EditLimits,
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
            view: PacketProjector::new(media_base, table.sync_entry_count()),
            limits,
            phase: Phase::Advance,
            scan: table.cursor(),
            duration_scan: table.cursor(),
            staging: None,
            after_emit: Phase::Emit,
            next_edit: 0,
            movie_cursor: zero,
            demux_movie_ticks: 0,
            demux_source_start: zero,
            staged_empty_endpoint: false,
            active: None,
            active_ticks: None,
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
    pub fn take_occurrence(&mut self) -> Result<EditOccurrence> {
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready native occurrence"))
    }
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<CursorStatus> {
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
        self.active_ticks = self.edit_ticks();
        self.scan = self.table.cursor();
        self.phase = Phase::Emit;
        Ok(())
    }

    fn edit_ticks(&self) -> Option<EditTicks> {
        // These starts were constructed in the media base. With its unit
        // numerator, subtracting two signed64 ticks and adding a third fits
        // the exact-time representation. The end comparison uses the ceiling
        // since it is tested against integer source ticks.
        if self.media_base.numerator() != 1 {
            return None;
        }
        let edit = self.active?;
        let movie = edit
            .movie_start
            .to_i64_ticks(self.media_base, Rounding::NearestAway)
            .ok()?;
        let decode = self
            .demux_source_start
            .to_i64_ticks(self.media_base, Rounding::NearestAway)
            .ok()?;
        Some(EditTicks {
            shift: movie.checked_sub(decode)?,
            start: edit.entry.media_time,
            end: self
                .source_end
                .to_i64_ticks(self.media_base, Rounding::Ceil)
                .ok()?,
        })
    }

    fn project(&self, source: &Sample, edit: ProjectedEdit) -> Result<PacketView> {
        // scan_sample has already established equal, known DTS and CTS and
        // checked the edit's source flag profile.
        if let Some(ticks) = self.active_ticks
            && let Some(dts) = source.dts_ticks.and_then(|dts| i64::try_from(dts).ok())
        {
            let field = ClockField::from_exact_ticks(i128::from(dts) + i128::from(ticks.shift))?;
            return Ok(PacketView {
                source_ordinal: source.ordinal,
                source_description: source.description_index,
                source_range: source.range.clone(),
                pts: field.api_ticks(),
                dts: field.api_ticks(),
                pts_field: field,
                dts_field: field,
                duration: i64::from(source.duration_ticks),
                flags: u32::from(source.is_container_sync())
                    | if dts < ticks.start || dts >= ticks.end {
                        4
                    } else {
                        0
                    },
            });
        }
        self.view
            .project_demux_edit(source, edit, self.demux_source_start)
    }

    fn reached_edit_end(&self, ticks: i128) -> Result<bool> {
        if let Some(edit) = self.active_ticks {
            return Ok(ticks >= i128::from(edit.end));
        }
        Ok(Time::from_ticks(ticks, self.media_base)? >= self.source_end)
    }
    fn terminal(&self, mut occurrence: EditOccurrence) -> Result<EditOccurrence> {
        if self.generated > self.table.sample_count() {
            let end = self.demux_movie_ticks;
            let dts = i128::from(
                occurrence
                    .demux
                    .dts
                    .ok_or(Error::Unsupported("native terminal DTS unknown"))?,
            );
            let full_end = dts
                .checked_add(i128::from(occurrence.demux.duration))
                .ok_or(Error::Overflow)?;
            occurrence.demux.duration = if end <= dts {
                0
            } else if end >= full_end {
                occurrence.demux.duration
            } else {
                i64::try_from(end - dts).map_err(|_| Error::Overflow)?
            };
            occurrence.duration_basis = DurationBasis::ExpandedTerminalEndObservation;
        }
        Ok(occurrence)
    }
    fn poll_inner(&mut self, work: &mut WorkBudget) -> Result<CursorStatus> {
        if self.ready.is_some() {
            return Ok(CursorStatus::Ready);
        }
        loop {
            match self.phase {
                Phase::Advance => {
                    if self.next_edit == self.edits.len() {
                        self.phase = Phase::Done;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
                    }
                    let entry = self.edits.entry(self.next_edit)?;
                    if entry.rate_integer != 1 || entry.rate_fraction != 0 {
                        return Err(Error::Unsupported(
                            "native non-unit edit occurrence profile",
                        ));
                    }
                    // Source movie_cursor remains the exact rational edit sum.
                    // Demux edits accumulate separately rounded media ticks.
                    let movie_start = Time::from_ticks(self.demux_movie_ticks, self.media_base)?;
                    let duration = entry.duration(self.movie_base)?;
                    let demux_delta = duration.to_ticks(self.media_base, Rounding::NearestAway)?;
                    self.demux_movie_ticks = self
                        .demux_movie_ticks
                        .checked_add(demux_delta)
                        .ok_or(Error::Overflow)?;
                    self.demux_source_start = Time::new(0, 1)?;
                    self.movie_cursor = self.movie_cursor.checked_add(duration)?;
                    self.next_edit = self.next_edit.checked_add(1).ok_or(Error::Overflow)?;
                    if entry.is_empty() {
                        if self.generated == 0 {
                            continue;
                        }
                        // Observed expanded terminal gap appends a demux-only
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
                        endpoint.duration_basis = DurationBasis::TerminalEmptyEndpointObservation;
                        let field = ClockField::from_exact_ticks(
                            movie_start.to_ticks(self.media_base, Rounding::NearestAway)?,
                        )?;
                        endpoint.demux.dts_field = field;
                        endpoint.demux.pts_field = field;
                        endpoint.demux.dts = field.api_ticks();
                        endpoint.demux.pts = field.api_ticks();
                        endpoint.demux.duration = i64::from(endpoint.source.duration_ticks);
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
                    self.active = Some(ProjectedEdit {
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
                    SampleStep::Yield => return Ok(CursorStatus::Yield),
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
                            self.demux_source_start = dts;
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
                            SampleStep::Yield => return Ok(CursorStatus::Yield),
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
                        self.after_emit =
                            if source.is_container_sync() && self.reached_edit_end(end_ticks)? {
                                Phase::Advance
                            } else {
                                Phase::Emit
                            };
                        let demux = self.project(&source, active)?;
                        self.staged_empty_endpoint = false;
                        self.generated = self.generated.checked_add(1).ok_or(Error::Overflow)?;
                        count_limit(self.generated, self.limits.max_occurrences)?;
                        self.staging = Some(EditOccurrence {
                            sequence: self.generated,
                            edit_ordinal: self.next_edit,
                            source,
                            demux,
                            duration_basis: DurationBasis::SourceDeltaAtTerminal,
                        });
                    }
                    // One original decode-table row per packet ordinal until
                    // exhausted. This cursor is independent of the repeated
                    // source occurrence scan; neither source samples nor runs
                    // are expanded. Keep staging across a one-token work yield.
                    let delta = match self.duration_scan.poll(work)? {
                        SampleStep::Yield => return Ok(CursorStatus::Yield),
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
                        next.demux.duration = i64::from(delta);
                        next.duration_basis = DurationBasis::OriginalDecodeTableOrdinal;
                    }
                    self.phase = self.after_emit;
                    if let Some(mut previous) = self.pending.take() {
                        if self.staged_empty_endpoint {
                            previous.duration_basis =
                                DurationBasis::TerminalEmptyEndpointObservation;
                        } else if previous.duration_basis
                            != DurationBasis::OriginalDecodeTableOrdinal
                        {
                            previous.duration_basis = if next.demux.dts >= previous.demux.dts {
                                DurationBasis::FollowingOccurrenceDts
                            } else {
                                DurationBasis::SourceDeltaAtBackwardJump
                            };
                            previous.demux = previous.demux.with_following_dts(next.demux.dts)?;
                        }
                        self.pending = Some(next);
                        self.ready = Some(previous);
                        return Ok(CursorStatus::Ready);
                    }
                    self.pending = Some(next);
                }
                Phase::Done => {
                    if self.pending.is_none() {
                        return Ok(CursorStatus::End);
                    }
                    if !work.try_charge(1) {
                        return Ok(CursorStatus::Yield);
                    }
                    let last = self
                        .pending
                        .take()
                        .ok_or(Error::Invalid("missing terminal occurrence"))?;
                    self.ready = Some(self.terminal(last)?);
                    return Ok(CursorStatus::Ready);
                }
            }
        }
    }
}
