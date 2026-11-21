//! FFmpeg's MOV demuxer applying an edit list to a regular track. `EditCursor`
//! emits each media edit's samples from the preceding sync sample on, up to a sync
//! sample that reaches the edit's end, and so repeats, trims and shifts samples,
//! for rate-one edits over samples with no composition offsets and positive decode
//! deltas. The source table is never changed.
//!
//! Specification: Apple QuickTime File Format and Adobe F4V (edit mapping, gaps and
//! repetition).
use crate::{
    Access, CursorStatus, Next, Sample,
    compat::mov::{
        clocks::ClockField,
        view::{PacketProjector, PacketView, ProjectedEdit},
    },
    movie::EditList,
    table::{DecodeDeltas, SampleCursor, SampleTable},
};
use triergon_core::{Error, Result, Rounding, Time, TimeBase, WorkBudget};

/// Limits of an [`EditCursor`]; passing one is `Error::Limit`.
#[derive(Clone, Copy, Debug)]
pub struct EditLimits {
    /// Packets the cursor may emit in all.
    pub max_occurrences: u64,
    /// Sample reads the cursor may make in all, counting every pass over the table
    /// and every decode delta read for a duration.
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

/// Where an edited packet's duration comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurationBasis {
    /// The source decode delta, for the packet the demuxer repeats at a trailing
    /// empty edit and for the packet before it.
    TerminalEmptyEndpointObservation,
    /// The `stts` delta of the sample whose ordinal is the packet's position: the
    /// demuxer takes the n-th packet's duration from the n-th decode delta, even
    /// where edits repeat or skip samples.
    OriginalDecodeTableOrdinal,
    /// The decode delta its `trun` declares, for a fragment sample under an edit.
    FragmentDecodeDelta,
    /// The distance to the next packet's decode time.
    FollowingOccurrenceDts,
    /// The source decode delta, because the next packet decodes earlier.
    SourceDeltaAtBackwardJump,
    /// The source decode delta of the last packet.
    SourceDeltaAtTerminal,
    /// The last packet of an edit list that repeats samples, cut to end at the
    /// edits' total duration in media ticks, and zero when it starts at or after it.
    ExpandedTerminalEndObservation,
}

/// One packet an [`EditCursor`] emits.
#[derive(Clone, Debug)]
pub struct EditOccurrence {
    /// 1-based position of the packet in the cursor's output.
    pub sequence: u64,
    /// 1-based position in `elst` of the edit the packet falls in.
    pub edit_ordinal: u32,
    /// The source sample the packet carries, unchanged; edits can repeat it.
    pub source: Sample,
    /// The packet the demuxer reports.
    pub demux: PacketView,
    /// Where the packet's duration comes from.
    pub duration_basis: DurationBasis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Advance,
    Find,
    Emit,
    Done,
}

/// Applies one regular track's edit list as FFmpeg's MOV demuxer does, yielding an
/// [`EditOccurrence`] per packet. Each media edit starts at the last sync sample at
/// or before its media time and runs to the first sync sample whose end reaches the
/// edit's end; packets outside the edit are marked discard. It reads the table in
/// passes with no per-sample storage, within [`EditLimits`]. The caller pairs the
/// table and edit list of one track.
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
    duration_scan: DecodeDeltas<'a>,
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

const EDIT_PROFILE: Error =
    Error::Unsupported("edit over reordered, zero-delta or recovery samples");

/// One step of [`EditCursor::prepare_emit`].
#[derive(Debug)]
pub(crate) enum EmitPrepare<'a> {
    Ready(EmitStart<'a>),
    Yield,
    /// The edits or clocks need the cursor's full steps.
    Unsuited,
}

/// A prepared edit cursor: its table, the first sample it emits, the demux
/// decode-time shift and the edit's end in media ticks, how many samples it has
/// scanned, and its limits.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EmitStart<'a> {
    pub(crate) table: &'a SampleTable,
    pub(crate) first: u64,
    pub(crate) shift: i64,
    pub(crate) end: i64,
    pub(crate) scanned: u64,
    pub(crate) limits: EditLimits,
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
    /// A cursor over `table` under `edits`, with the track's media time base and
    /// the movie's `mvhd` time base. `Error::Unsupported` for an empty table, an
    /// empty edit list or an empty `stss`.
    pub fn new(
        table: &'a SampleTable,
        edits: &'a EditList,
        media_base: TimeBase,
        movie_base: TimeBase,
        limits: EditLimits,
    ) -> Result<Self> {
        if table.sample_count() == 0 || edits.is_empty() || table.sync_entry_count() == Some(0) {
            return Err(Error::Unsupported(
                "edited track with an empty source, list or sync table",
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
            duration_scan: DecodeDeltas::new(table),
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
    /// Takes the ready packet; `Error::Invalid` when none is ready.
    pub fn take_occurrence(&mut self) -> Result<EditOccurrence> {
        self.ready
            .take()
            .ok_or(Error::Invalid("no ready occurrence"))
    }
    /// Advances to the next packet. Failures are sticky. `Error::Unsupported` for
    /// an edit whose rate is not one, a media edit with a negative start or zero
    /// duration, one that starts outside the samples or after no sync sample, and
    /// a sample with a composition offset, a zero delta, unknown or unequal clocks,
    /// or partial sync, dependency or unknown access flags.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<CursorStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner::<false>(work);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn scan_sample(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        let step = self.scan.poll(work)?;
        if let Next::Item(sample) = &step {
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
                return Err(EDIT_PROFILE);
            }
        }
        Ok(step)
    }
    fn begin_emit(&mut self) -> Result<()> {
        self.first = self
            .preceding_sync
            .ok_or(Error::Unsupported("edit has no preceding declared sync"))?;
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
                    .ok_or(Error::Unsupported("terminal DTS unknown"))?,
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
    /// Runs a fresh cursor until its last edit, a media edit after at most a
    /// leading empty edit, is about to emit its first occurrence, and returns
    /// what [`skip_to`](Self::skip_to) and a scan of the occurrences need.
    /// `Unsuited` for another edit list or a projection that needs exact time.
    pub(crate) fn prepare_emit(&mut self, work: &mut WorkBudget) -> Result<EmitPrepare<'a>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.generated != 0
            || self.staging.is_some()
            || self.pending.is_some()
            || self.ready.is_some()
            || !(self.edits.len() == 1
                || (self.edits.len() == 2 && self.edits.entry(0)?.is_empty()))
        {
            return Ok(EmitPrepare::Unsuited);
        }
        let status = self.poll_inner::<true>(work);
        if let Err(error) = status {
            self.failed = Some(error);
        }
        if status? == CursorStatus::Yield {
            return Ok(EmitPrepare::Yield);
        }
        let Some(ticks) = self
            .active_ticks
            .filter(|_| matches!(self.phase, Phase::Emit) && self.next_edit == self.edits.len())
        else {
            return Ok(EmitPrepare::Unsuited);
        };
        Ok(EmitPrepare::Ready(EmitStart {
            table: self.table,
            first: self.first,
            shift: ticks.shift,
            end: ticks.end,
            scanned: self.scanned,
            limits: self.limits,
        }))
    }

    /// Moves a cursor that [`prepare_emit`](Self::prepare_emit) left ready, and
    /// that holds no occurrence, to sample `ordinal` without building the
    /// occurrences before it. Each sample before `ordinal` is scanned; each from
    /// the edit's first emitted sample on is also counted as an occurrence and
    /// has its decode delta read, against the cursor's limits, as polling counts
    /// them. The next occurrence the cursor makes ready is then `ordinal`'s.
    ///
    /// Charges two work units per sample from the edit's first emitted sample,
    /// one per sample before it, and returns `false` when fewer than two are
    /// left; a later call continues. Fails, and keeps failing, where the cursor
    /// is not so prepared, where `ordinal` precedes the edit's first emitted
    /// sample, where a skipped sample is a sync sample that reaches the edit's
    /// end, where the track has composition offsets or a dependency table, where
    /// a skipped sample has a zero decode delta or is a partial sync sample, or
    /// where a limit is passed.
    pub(crate) fn skip_to(&mut self, ordinal: u64, work: &mut WorkBudget) -> Result<bool> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.skip_inner(ordinal, work);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn skip_inner(&mut self, ordinal: u64, work: &mut WorkBudget) -> Result<bool> {
        if !matches!(self.phase, Phase::Emit)
            || self.staging.is_some()
            || self.pending.is_some()
            || self.ready.is_some()
            || ordinal < self.first
        {
            return Err(Error::Invalid("edit skip needs a prepared emitting cursor"));
        }
        let ticks = self
            .active_ticks
            .ok_or(Error::Invalid("edit skip needs tick clocks"))?;
        while self.scan.ordinal() + 1 < ordinal {
            // A sample's scan and its decode delta; or, before the first, its scan.
            if work.remaining() < 2 {
                return Ok(false);
            }
            work.take(1);
            let sample = self.scan.skip()?;
            self.scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
            count_limit(self.scanned, self.limits.max_scanned_samples)?;
            if sample.ordinal < self.first {
                continue;
            }
            if self.table.has_composition_offsets()
                || sample.delta == 0
                || sample.partial
                || self.table.has_dependency_table()
            {
                return Err(EDIT_PROFILE);
            }
            let end = sample
                .dts
                .checked_add(i128::from(sample.delta))
                .ok_or(Error::Overflow)?;
            if sample.sync && end >= i128::from(ticks.end) {
                return Err(Error::Invalid("edit skip target past its edit"));
            }
            self.generated = self.generated.checked_add(1).ok_or(Error::Overflow)?;
            count_limit(self.generated, self.limits.max_occurrences)?;
            if let Next::Item(_) = self.duration_scan.poll(work)? {
                self.scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
                count_limit(self.scanned, self.limits.max_scanned_samples)?;
            }
        }
        Ok(true)
    }

    fn poll_inner<const PREPARE: bool>(&mut self, work: &mut WorkBudget) -> Result<CursorStatus> {
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
                        return Err(Error::Unsupported("non-unit edit occurrence"));
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
                            .ok_or(Error::Invalid("gap has no retained predecessor"))?;
                        let active = self.active.ok_or(Error::Invalid("gap has no media edit"))?;
                        if self.next_edit != self.edits.len()
                            || self.generated <= self.table.sample_count()
                            || !previous.source.is_container_sync()
                            || previous.source.dts_ticks
                                != Some(i128::from(active.entry.media_time))
                        {
                            return Err(Error::Unsupported("empty edit after the first"));
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
                        return Err(Error::Unsupported("negative or zero-duration media edit"));
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
                    Next::Yield => return Ok(CursorStatus::Yield),
                    Next::End => {
                        if self
                            .last_source_end
                            .is_none_or(|end| self.source_start >= end)
                        {
                            return Err(Error::Unsupported(
                                "edit starts outside known source samples",
                            ));
                        }
                        self.begin_emit()?;
                    }
                    Next::Item(sample) => {
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
                    if PREPARE {
                        return Ok(CursorStatus::Ready);
                    }
                    let mut next = if let Some(staged) = self.staging.take() {
                        staged
                    } else {
                        // Finding the edit checked the samples before its first.
                        while self.scan.ordinal() + 1 < self.first {
                            if !work.try_charge(1) {
                                return Ok(CursorStatus::Yield);
                            }
                            self.scan.skip()?;
                            self.scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
                            count_limit(self.scanned, self.limits.max_scanned_samples)?;
                        }
                        let source = match self.scan_sample(work)? {
                            Next::Yield => return Ok(CursorStatus::Yield),
                            Next::End => {
                                self.phase = Phase::Advance;
                                continue;
                            }
                            Next::Item(source) => source,
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
                        EditOccurrence {
                            sequence: self.generated,
                            edit_ordinal: self.next_edit,
                            source,
                            demux,
                            duration_basis: DurationBasis::SourceDeltaAtTerminal,
                        }
                    };
                    // One original decode-table row per packet ordinal until
                    // exhausted. This cursor is independent of the repeated
                    // source occurrence scan; neither source samples nor runs
                    // are expanded. Keep the occurrence across a one-token work yield.
                    let delta = match self.duration_scan.poll(work)? {
                        Next::Yield => {
                            self.staging = Some(next);
                            return Ok(CursorStatus::Yield);
                        }
                        Next::Item(delta) => {
                            self.scanned = self.scanned.checked_add(1).ok_or(Error::Overflow)?;
                            count_limit(self.scanned, self.limits.max_scanned_samples)?;
                            Some(delta)
                        }
                        Next::End => None,
                    };
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
                    if PREPARE || self.pending.is_none() {
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
