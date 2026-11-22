//! Regular sample tables. [`SampleTableBuilder`] checks a track's `stbl` into a
//! [`SampleTable`] that keeps the tables as stored; [`SampleCursor`] reads every sample in
//! order and [`SampleLocator`] finds one sample from the run and chunk tables without
//! visiting the samples before it. Nothing is stored per sample.
use crate::{
    Access, BuildStatus, Limits, Next, Sample,
    boxes::{BoxChildren, BoxView},
    fields,
    movie::SampleDescription,
    policy,
    sample::SampleSpan,
};
use recovery::{InitialDescriptionDiagnostic, RunCoverage, RunKind, SyncDiagnostic};
use rows::{Ctts, Offsets, Sizes, Words};
use std::mem;
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

pub(crate) mod recovery;
pub(crate) mod rows;

/// How [`SampleTableBuilder`] treats `stts` and `ctts` runs that count more samples than
/// the sample size table has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimingRunPolicy {
    /// The runs must count exactly the sample count; otherwise `Error::Invalid`.
    #[default]
    Strict,
    /// For a nonempty table, the final decode run may straddle the end and
    /// composition runs may overrun it. Every run must declare at least one sample.
    BoundedPrefix,
}

/// How [`SampleTableBuilder`] treats an `stss` entry past the last sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncSamplePolicy {
    /// Such an entry is `Error::Invalid`.
    #[default]
    Strict,
    /// Use the increasing prefix before the first entry past the last sample.
    /// The remaining entries stay in the raw table and never provide a seek point.
    BoundedPrefix,
}

/// Validation policies for regular sample tables. Both are strict by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleTablePolicy {
    /// How timing runs must cover the samples.
    pub timing_runs: TimingRunPolicy,
    /// How sync sample entries must lie within the samples.
    pub sync_samples: SyncSamplePolicy,
}

/// Samples counted by timing runs beyond those the sample size table has; zero unless
/// [`TimingRunPolicy::BoundedPrefix`] accepted them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimingRunSurplus {
    /// Surplus samples of the last `stts` run.
    pub decode_samples: u64,
    /// Surplus samples of the `ctts` runs.
    pub composition_samples: u64,
}

/// Checked `stbl` boxes, with no per-sample index.
#[derive(Debug)]
pub struct SampleTable {
    raw: BoxView,
    descriptions: BudgetVec<SampleDescription>,
    stts: Words,
    ctts: Option<Words>,
    stsc: Words,
    sizes: Sizes,
    offsets: Offsets,
    sync: Option<Words>,
    partial: Option<Words>,
    dependency: Option<BoxView>,
    timing_run_surplus: TimingRunSurplus,
    unused_sync_entries: u32,
}

struct SamplePosition {
    ordinal: u64,
    description: u32,
    byte: u64,
    dts: i128,
    delta: u32,
    cto: u32,
}

impl SampleTable {
    /// The `stbl` box as read.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// The entries of `stsd`, in order.
    pub fn descriptions(&self) -> &[SampleDescription] {
        self.descriptions.as_slice()
    }
    /// Number of samples, from `stsz` or `stz2`.
    pub fn sample_count(&self) -> u64 {
        u64::from(self.sizes.count)
    }
    /// The timing runs' surplus over the sample count; the stored runs keep their counts.
    pub fn timing_run_surplus(&self) -> TimingRunSurplus {
        self.timing_run_surplus
    }
    /// Number of entries stored in `stss`, unused ones included; `None` without `stss`,
    /// `Some(0)` for an empty one.
    pub fn sync_entry_count(&self) -> Option<u32> {
        self.sync
            .as_ref()
            .map(|table| table.count + self.unused_sync_entries)
    }
    /// Number of `stss` entries from the first one past the last sample on, which
    /// [`SyncSamplePolicy::BoundedPrefix`] leaves unused; zero otherwise.
    pub fn unused_sync_entries(&self) -> u32 {
        self.unused_sync_entries
    }
    /// Whether the table has `sdtp`, even one whose bytes are all zero.
    pub(crate) fn has_dependency_table(&self) -> bool {
        self.dependency.is_some()
    }
    /// Whether a partial sync table marks any sample.
    pub(crate) fn has_partial_sync_samples(&self) -> bool {
        self.partial.as_ref().is_some_and(|table| table.count != 0)
    }
    /// Whether the table includes `ctts`, even if it has no rows.
    pub fn has_composition_offsets(&self) -> bool {
        self.ctts.is_some()
    }

    /// The checked `ctts` runs as stored, with their box version.
    pub(crate) fn composition_runs(&self) -> Option<&Words> {
        self.ctts.as_ref()
    }
    pub(crate) fn decode_runs(&self) -> &Words {
        &self.stts
    }
    /// A cursor over every sample in decode order.
    pub fn cursor(&self) -> SampleCursor<'_> {
        self.resume(SampleCursorPosition::default())
    }
    /// The byte ranges of its samples in order, without their clocks or access.
    pub(crate) fn ranges(&self) -> SampleRanges<'_> {
        SampleRanges {
            table: self,
            ordinal: 0,
            chunks: ChunkCursor::default(),
        }
    }
    /// The latest sync sample at or before 1-based `ordinal`, or `None` if `stss` lists
    /// none that early; without `stss` every sample is one. `Error::Invalid` for an
    /// ordinal outside the table.
    pub fn previous_sync(&self, ordinal: u64) -> Result<Option<u64>> {
        if ordinal == 0 || ordinal > self.sample_count() {
            return Err(Error::Invalid("sync query sample"));
        }
        match &self.sync {
            Some(sync) => sync.previous(ordinal),
            None => Ok(Some(ordinal)),
        }
    }
    /// A locator for the sample with 1-based `ordinal`; `Error::Invalid` outside the table.
    pub fn locate(&self, ordinal: u64) -> Result<SampleLocator<'_>> {
        if ordinal == 0 || ordinal > self.sample_count() {
            return Err(Error::Invalid("locate sample"));
        }
        Ok(SampleLocator {
            table: self,
            target: ordinal,
            phase: LocatePhase::Decode,
            row: 0,
            skip: ordinal - 1,
            dts: 0,
            cto: 0,
            delta: 0,
            description: 0,
            chunk: 0,
            first_sample: 0,
            byte: 0,
            size_skip: 0,
            done: false,
        })
    }
    fn sample(
        &self,
        ordinal: u64,
        description: u32,
        byte: u64,
        dts: i128,
        delta: u32,
        cto: u32,
    ) -> Result<Sample> {
        self.sample_with_access(
            SamplePosition {
                ordinal,
                description,
                byte,
                dts,
                delta,
                cto,
            },
            || {
                let sync = self
                    .sync
                    .as_ref()
                    .map(|v| v.contains(ordinal))
                    .transpose()?
                    .unwrap_or(true);
                let partial = self
                    .partial
                    .as_ref()
                    .map(|v| v.contains(ordinal))
                    .transpose()?
                    .unwrap_or(false);
                Ok((sync, partial))
            },
        )
    }
    fn sample_with_access(
        &self,
        position: SamplePosition,
        access: impl FnOnce() -> Result<(bool, bool)>,
    ) -> Result<Sample> {
        let SamplePosition {
            ordinal,
            description,
            byte,
            dts,
            delta,
            cto,
        } = position;
        let size = self.sizes.get(
            ordinal
                .checked_sub(1)
                .ok_or(Error::Invalid("sample ordinal"))?,
        )?;
        let end = byte.checked_add(u64::from(size)).ok_or(Error::Overflow)?;
        let composition_offset = self
            .ctts
            .as_ref()
            .map(|table| crate::CompositionOffset::from_wire(table.version, cto))
            .transpose()?;
        let cts = dts
            .checked_add(composition_offset.map_or(0, crate::CompositionOffset::ticks))
            .ok_or(Error::Overflow)?;
        let (sync, partial_sync) = access()?;
        let dependency = self
            .dependency
            .as_ref()
            .map(|v| {
                let at = usize::try_from(ordinal - 1)
                    .map_err(|_| Error::Overflow)?
                    .checked_add(4)
                    .ok_or(Error::Overflow)?;
                v.payload().get(at).copied().ok_or(Error::Truncated)
            })
            .transpose()?;
        Ok(Sample {
            ordinal,
            description_index: description,
            range: byte..end,
            dts_ticks: Some(dts),
            cts_ticks: Some(cts),
            duration_ticks: delta,
            composition_offset,
            access: if sync {
                Access::ContainerSync
            } else {
                Access::NonSync
            },
            partial_sync,
            dependency,
            fragment_flags: None,
        })
    }
}

#[derive(Default, Debug)]
struct Parts {
    stsd: Option<BoxView>,
    stts: Option<BoxView>,
    stsc: Option<BoxView>,
    sizes: Option<BoxView>,
    offsets: Option<BoxView>,
    sync: Option<BoxView>,
    partial: Option<BoxView>,
    dependency: Option<BoxView>,
    compact: Option<BoxView>,
}
fn single(slot: &mut Option<BoxView>, value: BoxView) -> Result<()> {
    if slot.is_some() {
        return Err(Error::Invalid("duplicate/conflicting sample table"));
    }
    *slot = Some(value);
    Ok(())
}
fn required(slot: Option<&BoxView>) -> Result<BoxView> {
    slot.cloned()
        .ok_or(Error::Invalid("missing mandatory sample table"))
}

impl Parts {
    /// Keeps `raw` where it is one of the tables the builder reads; other boxes stay
    /// only in the raw `stbl`.
    fn insert(&mut self, raw: BoxView) -> Result<()> {
        match &raw.header().kind {
            b"stsd" => single(&mut self.stsd, raw),
            b"stts" => single(&mut self.stts, raw),
            b"stsc" => single(&mut self.stsc, raw),
            b"stsz" => single(&mut self.sizes, raw),
            b"stz2" => single(&mut self.compact, raw),
            b"stco" | b"co64" => single(&mut self.offsets, raw),
            b"stss" => single(&mut self.sync, raw),
            b"stps" => single(&mut self.partial, raw),
            b"sdtp" => single(&mut self.dependency, raw),
            _ => Ok(()),
        }
    }
}

/// What a [`SampleTableBuilder`] accepts beyond strict tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Acceptance {
    /// Strict tables only.
    Strict,
    /// Strict tables, with an `sdtp` whose flags word is 1, as legacy QuickTime exports
    /// write it.
    LegacyDependencyFlags,
    /// Run recovery: the timing runs may overrun the sample count, and `stss` entries
    /// may be out of order or past the last sample.
    Runs,
    /// Run recovery that also accepts a zero description index in the first `stsc` row,
    /// when the table has one description and the second row starts at chunk 2 with
    /// index 1.
    RunsAndInitialDescription,
    /// Run recovery that accepts any description index.
    RunsAndDescriptions,
}
impl Acceptance {
    fn recovers_runs(self) -> bool {
        matches!(
            self,
            Self::Runs | Self::RunsAndInitialDescription | Self::RunsAndDescriptions
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    Decode,
    Composition,
    Chunks,
    Sync,
    Partial,
    Done,
}

/// Where the row checks of a built table stand.
#[derive(Clone, Copy, Debug)]
struct RowCheck {
    check: Check,
    row: u32,
    total: u64,
    previous: u32,
}
impl RowCheck {
    fn new(check: Check) -> Self {
        Self {
            check,
            row: 0,
            total: 0,
            previous: 0,
        }
    }
}

/// What run recovery found while checking the rows.
#[derive(Clone, Copy, Debug, Default)]
struct Findings {
    coverage: [Option<RunCoverage>; 2],
    sync_diagnostic: Option<SyncDiagnostic>,
    initial_description: Option<InitialDescriptionDiagnostic>,
    sync_prefix: Option<u32>,
}

/// Where a build stands. Each stage owns what it reads and what the stages before it
/// found, and changes in place.
#[derive(Debug)]
enum Stage {
    /// Collecting the tables among the `stbl` children, then parsing the `stsd`
    /// entries into `descriptions`.
    Collect {
        parts: Parts,
        descriptions: BudgetVec<SampleDescription>,
        children: Children,
    },
    /// Checking the rows of the built table, one per work unit; the table is ready
    /// once `rows` reaches [`Check::Done`].
    Validate {
        table: SampleTable,
        rows: RowCheck,
    },
    Failed(Error),
}

/// The children a collecting [`SampleTableBuilder`] reads.
#[derive(Debug)]
enum Children {
    /// The `stbl` children.
    Tables {
        boxes: BoxChildren,
        ctts: Option<BoxView>,
    },
    /// The `stsd` entries; `pending` is one parsed but not yet stored.
    Entries {
        entries: BoxChildren,
        ctts: Option<Ctts>,
        pending: Option<SampleDescription>,
    },
}

/// Checks one `stbl` into a [`SampleTable`] under a work budget, one table row per work
/// unit. Poll until [`BuildStatus::Ready`], then call [`finish`](Self::finish). A failure
/// other than backpressure is kept for later polls.
#[derive(Debug)]
pub struct SampleTableBuilder {
    raw: BoxView,
    limits: Limits,
    policy: SampleTablePolicy,
    acceptance: Acceptance,
    description_count: u32,
    stage: Stage,
    findings: Findings,
}
impl SampleTableBuilder {
    /// Allow the dependency flag used by legacy QuickTime exports in the
    /// recovery view. The version and row count are still checked.
    pub(crate) fn allow_legacy_dependency_flags(&mut self) {
        self.acceptance = Acceptance::LegacyDependencyFlags;
    }

    /// Validates an `stbl` with strict timing and sync counts. Fails for another box
    /// type or budget pool, as in [`with_policy`](Self::with_policy).
    pub fn new(raw: BoxView, budget: Budget, limits: Limits) -> Result<Self> {
        Self::with_timing_policy(raw, budget, limits, TimingRunPolicy::Strict)
    }

    /// Selects timing validation while keeping sync entries strict.
    pub fn with_timing_policy(
        raw: BoxView,
        budget: Budget,
        limits: Limits,
        timing_policy: TimingRunPolicy,
    ) -> Result<Self> {
        Self::with_policy(
            raw,
            budget,
            limits,
            SampleTablePolicy {
                timing_runs: timing_policy,
                ..SampleTablePolicy::default()
            },
        )
    }

    /// A builder with both policies chosen. `Error::Invalid` for another box type or a box
    /// from another budget pool.
    pub fn with_policy(
        raw: BoxView,
        budget: Budget,
        limits: Limits,
        policy: SampleTablePolicy,
    ) -> Result<Self> {
        if raw.header().kind != *b"stbl" {
            return Err(Error::Invalid("not stbl"));
        }
        if !raw.bytes().belongs_to(&budget) {
            return Err(Error::Invalid(
                "metadata and index use different budget pools",
            ));
        }
        let boxes = raw.children(0, limits)?;
        Ok(Self {
            raw,
            limits,
            policy,
            acceptance: Acceptance::Strict,
            description_count: 0,
            stage: Stage::Collect {
                parts: Parts::default(),
                descriptions: BudgetVec::new(&budget),
                children: Children::Tables { boxes, ctts: None },
            },
            findings: Findings::default(),
        })
    }
    /// The `stbl` box being checked.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// One bounded step. `Error::Invalid` for missing, duplicate or inconsistent tables,
    /// `Error::Unsupported` for unknown versions or flags, `Error::Limit` past the limits.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<BuildStatus> {
        if let Stage::Failed(error) = self.stage {
            return Err(error);
        }
        let result = self.poll_inner(work);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. })
        {
            self.stage = Stage::Failed(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget) -> Result<BuildStatus> {
        loop {
            match &mut self.stage {
                Stage::Collect {
                    parts,
                    descriptions,
                    children,
                } => match children {
                    Children::Tables { boxes, ctts } => match boxes.poll(work)? {
                        Next::Yield => return Ok(BuildStatus::Yield),
                        Next::Item(raw) if raw.header().kind == *b"ctts" => single(ctts, raw)?,
                        Next::Item(raw) => parts.insert(raw)?,
                        Next::End => {
                            if parts.compact.is_some() && parts.sizes.is_some() {
                                return Err(Error::Invalid("both stsz and stz2"));
                            }
                            let ctts = ctts.take().map(Ctts::header).transpose()?;
                            let stsd = required(parts.stsd.as_ref())?;
                            fields::full_zero(stsd.payload())?;
                            self.description_count = fields::u32_at(stsd.payload(), 4)?;
                            if self.description_count == 0 {
                                return Err(Error::Invalid("empty sample descriptions"));
                            }
                            policy(self.description_count, self.limits.max_descriptions)?;
                            *children = Children::Entries {
                                entries: stsd.children(8, self.limits)?,
                                ctts,
                                pending: None,
                            };
                        }
                    },
                    Children::Entries {
                        entries,
                        ctts,
                        pending,
                    } => {
                        if let Some(value) = pending.take() {
                            if !work.try_charge(1) {
                                *pending = Some(value);
                                return Ok(BuildStatus::Yield);
                            }
                            if let Err(rejected) = descriptions.try_push(value) {
                                *pending = Some(rejected.value);
                                return Err(rejected.error);
                            }
                            continue;
                        }
                        match entries.poll(work)? {
                            Next::Yield => return Ok(BuildStatus::Yield),
                            Next::Item(raw) => {
                                let ordinal = u32::try_from(descriptions.len())
                                    .map_err(|_| Error::Overflow)?
                                    .checked_add(1)
                                    .ok_or(Error::Overflow)?;
                                if ordinal > self.description_count {
                                    return Err(Error::Invalid("extra sample description"));
                                }
                                *pending = Some(SampleDescription::parse(raw, ordinal)?);
                            }
                            Next::End => {
                                if descriptions.len() != self.description_count as usize {
                                    return Err(Error::Invalid(
                                        "sample description count mismatch",
                                    ));
                                }
                                let empty = BudgetVec::new(descriptions.budget());
                                let table = build(
                                    &self.raw,
                                    self.limits,
                                    self.acceptance,
                                    parts,
                                    ctts.take(),
                                    mem::replace(descriptions, empty),
                                )?;
                                self.stage = Stage::Validate {
                                    table,
                                    rows: RowCheck::new(Check::Decode),
                                };
                            }
                        }
                    }
                },
                Stage::Validate { rows, .. } if rows.check == Check::Done => {
                    return Ok(BuildStatus::Ready);
                }
                Stage::Validate { table, rows } => {
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    self.findings.check_row(
                        table,
                        rows,
                        self.policy,
                        self.acceptance,
                        self.description_count,
                    )?;
                }
                Stage::Failed(error) => return Err(*error),
            }
        }
    }
    /// The table once the rows are checked.
    pub(crate) fn built(&self) -> Option<&SampleTable> {
        match &self.stage {
            Stage::Validate { table, rows } if rows.check == Check::Done => Some(table),
            _ => None,
        }
    }
    /// The table; the error that stopped the build, or `Error::Invalid` before
    /// [`BuildStatus::Ready`].
    pub fn finish(self) -> Result<SampleTable> {
        if self.acceptance.recovers_runs() {
            return Err(Error::Invalid(
                "recovered table cannot become a Strict sample table",
            ));
        }
        let mut table = match self.stage {
            Stage::Validate { table, rows } if rows.check == Check::Done => table,
            Stage::Failed(error) => return Err(error),
            _ => return Err(Error::Invalid("sample table builder not ready")),
        };
        if self.policy.timing_runs == TimingRunPolicy::BoundedPrefix {
            let [decode, composition] = self.findings.coverage;
            table.timing_run_surplus = TimingRunSurplus {
                decode_samples: decode.map_or(0, |run| run.surplus_samples),
                composition_samples: composition.map_or(0, |run| run.surplus_samples),
            };
        }
        if let Some(prefix) = self.findings.sync_prefix
            && let Some(sync) = &mut table.sync
        {
            table.unused_sync_entries = sync.count - prefix;
            sync.count = prefix;
        }
        Ok(table)
    }
}

/// The table `parts` and `descriptions` make, its fixed headers and row extents
/// checked.
fn build(
    raw: &BoxView,
    limits: Limits,
    acceptance: Acceptance,
    parts: &Parts,
    ctts: Option<Ctts>,
    descriptions: BudgetVec<SampleDescription>,
) -> Result<SampleTable> {
    let stts = Words::parse(required(parts.stts.as_ref())?, 8, limits)?;
    let stsc = Words::parse(required(parts.stsc.as_ref())?, 12, limits)?;
    let sizes = Sizes::parse(
        parts
            .sizes
            .clone()
            .or_else(|| parts.compact.clone())
            .ok_or(Error::Invalid("missing sample size table"))?,
    )?;
    let offsets = Offsets::parse(required(parts.offsets.as_ref())?, limits)?;
    let ctts = ctts.map(|table| table.rows(limits)).transpose()?;
    let sync = parts
        .sync
        .clone()
        .map(|v| Words::parse(v, 4, limits))
        .transpose()?;
    let partial = parts
        .partial
        .clone()
        .map(|v| Words::parse(v, 4, limits))
        .transpose()?;
    if let Some(dependency) = &parts.dependency {
        if acceptance != Acceptance::LegacyDependencyFlags
            || dependency.payload().get(..4) != Some(&[0, 0, 0, 1])
        {
            fields::full_zero(dependency.payload())?;
        }
        fields::exact_rows(dependency.payload(), 4, sizes.count, 1)?;
    }
    Ok(SampleTable {
        raw: raw.clone(),
        descriptions,
        stts,
        ctts,
        stsc,
        sizes,
        offsets,
        sync,
        partial,
        dependency: parts.dependency.clone(),
        timing_run_surplus: TimingRunSurplus::default(),
        unused_sync_entries: 0,
    })
}

impl Findings {
    /// Checks one row of `table` at `rows`, or moves `rows` to the next table.
    fn check_row(
        &mut self,
        t: &SampleTable,
        rows: &mut RowCheck,
        policy: SampleTablePolicy,
        acceptance: Acceptance,
        description_count: u32,
    ) -> Result<()> {
        match rows.check {
            Check::Decode | Check::Composition => {
                let source = if rows.check == Check::Decode {
                    Some(&t.stts)
                } else {
                    t.ctts.as_ref()
                };
                if let Some(source) = source {
                    if rows.row < source.count {
                        let count = source.word(rows.row, 0)?;
                        if count == 0 {
                            return Err(Error::Invalid("zero sample count in time run"));
                        }
                        let total = rows
                            .total
                            .checked_add(u64::from(count))
                            .ok_or(Error::Overflow)?;
                        if total > t.sample_count() {
                            let decode = rows.check == Check::Decode;
                            if policy.timing_runs == TimingRunPolicy::Strict
                                || t.sample_count() == 0
                                || (decode
                                    && (rows.row + 1 != source.count
                                        || rows.total >= t.sample_count()))
                            {
                                return Err(Error::Invalid("time run exceeds sample count"));
                            }
                            let slot = usize::from(!decode);
                            let first =
                                self.coverage[slot].map_or(rows.row, |v| v.first_excess_row);
                            let field_start = source
                                .raw
                                .header()
                                .start
                                .checked_add(u64::from(source.raw.header().header_size))
                                .and_then(|v| v.checked_add(8))
                                .and_then(|v| v.checked_add(u64::from(first) * 8))
                                .ok_or(Error::Overflow)?;
                            self.coverage[slot] = Some(RunCoverage {
                                kind: if decode {
                                    RunKind::Decode
                                } else {
                                    RunKind::Composition
                                },
                                first_excess_row: first,
                                count_field_start: field_start,
                                count_field_end: field_start
                                    .checked_add(4)
                                    .ok_or(Error::Overflow)?,
                                declared_samples: total,
                                bounded_samples: t.sample_count(),
                                surplus_samples: total - t.sample_count(),
                                strict_error: Error::Invalid("time run exceeds sample count"),
                            });
                        }
                        rows.total = total;
                        rows.row += 1;
                        return Ok(());
                    }
                    if rows.total != t.sample_count()
                        && !(policy.timing_runs == TimingRunPolicy::BoundedPrefix
                            && rows.total > t.sample_count())
                    {
                        return Err(Error::Invalid("time table sample count mismatch"));
                    }
                }
                *rows = RowCheck::new(if rows.check == Check::Decode {
                    Check::Composition
                } else {
                    Check::Chunks
                });
            }
            Check::Chunks => {
                if rows.row < t.stsc.count {
                    let first = t.stsc.word(rows.row, 0)?;
                    let per_chunk = t.stsc.word(rows.row, 4)?;
                    let description = t.stsc.word(rows.row, 8)?;
                    let chunks = u64::from(t.offsets.table.count);
                    let next = if rows.row + 1 < t.stsc.count {
                        u64::from(t.stsc.word(rows.row + 1, 0)?)
                    } else {
                        chunks + 1
                    };
                    let any_description = acceptance == Acceptance::RunsAndDescriptions;
                    if first == 0
                        || (rows.row == 0 && first != 1)
                        || u64::from(first) > chunks
                        || next <= u64::from(first)
                        || next > chunks + 1
                        || per_chunk == 0
                        || (description == 0
                            && !(acceptance == Acceptance::RunsAndInitialDescription
                                && rows.row == 0
                                && t.stsc.count >= 2
                                && t.stsc.word(1, 0)? == 2
                                && t.stsc.word(1, 8)? == 1
                                && description_count == 1)
                            && !any_description)
                        || (description > description_count && !any_description)
                    {
                        return Err(Error::Invalid("stsc order/count/description"));
                    }
                    if description == 0 && !any_description {
                        let field_start = t
                            .stsc
                            .raw
                            .header()
                            .start
                            .checked_add(u64::from(t.stsc.raw.header().header_size))
                            .and_then(|v| v.checked_add(16))
                            .ok_or(Error::Overflow)?;
                        self.initial_description = Some(InitialDescriptionDiagnostic {
                            field_start,
                            field_end: field_start.checked_add(4).ok_or(Error::Overflow)?,
                            raw_description: 0,
                            declared_first_chunk_samples: per_chunk,
                            observed_first_chunk_samples: t.stsc.word(1, 4)?,
                            demux_catalogue_entry: 1,
                            strict_error: Error::Invalid("stsc order/count/description"),
                        });
                    }
                    let span = (next - u64::from(first))
                        .checked_mul(u64::from(per_chunk))
                        .ok_or(Error::Overflow)?;
                    rows.total = rows.total.checked_add(span).ok_or(Error::Overflow)?;
                    if rows.total > t.sample_count() {
                        return Err(Error::Invalid("chunks exceed sample count"));
                    }
                    rows.row += 1;
                    return Ok(());
                }
                if rows.total != t.sample_count()
                    || (t.sample_count() == 0 && t.offsets.table.count != 0)
                {
                    return Err(Error::Invalid("chunk coverage mismatch"));
                }
                *rows = RowCheck::new(Check::Sync);
            }
            Check::Sync | Check::Partial => {
                let source = if rows.check == Check::Sync {
                    t.sync.as_ref()
                } else {
                    t.partial.as_ref()
                };
                if let Some(source) = source
                    && rows.row < source.count
                {
                    let sample = source.word(rows.row, 0)?;
                    let past_end = u64::from(sample) > t.sample_count();
                    if sample == 0 || sample <= rows.previous || past_end {
                        if past_end
                            && rows.check == Check::Sync
                            && policy.sync_samples == SyncSamplePolicy::BoundedPrefix
                        {
                            self.sync_prefix = Some(rows.row);
                            *rows = RowCheck::new(Check::Partial);
                            return Ok(());
                        }
                        if !acceptance.recovers_runs() || rows.check != Check::Sync || sample == 0 {
                            return Err(Error::Invalid("sync sample order/range"));
                        }
                        if self.sync_diagnostic.is_none() {
                            let start = source
                                .raw
                                .header()
                                .start
                                .checked_add(u64::from(source.raw.header().header_size))
                                .and_then(|v| v.checked_add(8 + u64::from(rows.row) * 4))
                                .ok_or(Error::Overflow)?;
                            self.sync_diagnostic = Some(SyncDiagnostic {
                                first_bad_row: rows.row,
                                source_value: sample,
                                field_start: start,
                                field_end: start.checked_add(4).ok_or(Error::Overflow)?,
                                strict_error: Error::Invalid("sync sample order/range"),
                            });
                        }
                    }
                    rows.previous = sample;
                    rows.row += 1;
                    return Ok(());
                }
                *rows = RowCheck::new(if rows.check == Check::Sync {
                    Check::Partial
                } else {
                    Check::Done
                });
            }
            Check::Done => {}
        }
        Ok(())
    }
}

/// Reads the samples of a [`SampleTable`] in decode order; from [`SampleTable::cursor`].
/// It allocates nothing, and a failed poll leaves it where it was.
// The table before the position: the compiler's order puts it after, which measured more
// instructions in the per-sample paths that copy or poll the cursor.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct SampleCursor<'a> {
    table: &'a SampleTable,
    at: SampleCursorPosition,
}

/// A [`SampleCursor`]'s place in its table, kept without borrowing the table so an
/// owner can hold both.
// The time last, so that the cursor fits in 96 bytes with the table first.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub(crate) struct SampleCursorPosition {
    ordinal: u64,
    chunks: ChunkCursor,
    stts_run: u32,
    stts_left: u32,
    delta: u32,
    ctts_run: u32,
    ctts_left: u32,
    offset: u32,
    sync_row: u32,
    partial_row: u32,
    dts: i128,
}

/// The chunk holding a table's next sample: its samples left and the byte at
/// which the next one starts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ChunkCursor {
    chunk: u32,
    chunk_left: u32,
    stsc_run: u32,
    description: u32,
    byte: u64,
}

impl ChunkCursor {
    /// Enter the next chunk once the current one has no samples left.
    fn enter(&mut self, table: &SampleTable) -> Result<()> {
        if self.chunk_left == 0 {
            let chunk_number = u64::from(self.chunk) + 1;
            if self.stsc_run + 1 < table.stsc.count
                && u64::from(table.stsc.word(self.stsc_run + 1, 0)?) == chunk_number
            {
                self.stsc_run += 1;
            }
            self.chunk_left = table.stsc.word(self.stsc_run, 4)?;
            self.description = table.stsc.word(self.stsc_run, 8)?;
            self.byte = table.offsets.get(self.chunk)?;
            self.chunk = self.chunk.checked_add(1).ok_or(Error::Overflow)?;
        }
        Ok(())
    }
    /// Step past the current sample, which ends at `end`.
    fn consume(&mut self, end: u64) -> Result<()> {
        self.byte = end;
        self.chunk_left = self
            .chunk_left
            .checked_sub(1)
            .ok_or(Error::Invalid("chunk exhausted"))?;
        Ok(())
    }
}

/// The byte ranges of a table's samples in order, from its chunk offsets and
/// sizes alone: the ranges [`SampleCursor`] reports, with the same errors, a
/// span of a chunk's consecutive samples at a time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SampleRanges<'a> {
    table: &'a SampleTable,
    ordinal: u64,
    chunks: ChunkCursor,
}

impl SampleRanges<'_> {
    /// The next at most `maximum` samples, all in one chunk, charging a work
    /// unit for each. A span stops before a sample whose end would overflow, and
    /// the next step reports that sample's error.
    pub(crate) fn next_span(
        &mut self,
        maximum: u64,
        work: &mut WorkBudget,
    ) -> Result<Next<SampleSpan>> {
        let left = self.table.sample_count() - self.ordinal;
        if left == 0 {
            return Ok(Next::End);
        }
        if !work.try_charge(1) {
            return Ok(Next::Yield);
        }
        self.chunks.enter(self.table)?;
        let start = self.chunks.byte;
        let wanted = left
            .min(u64::from(self.chunks.chunk_left))
            .min(maximum.saturating_sub(1).min(work.remaining()) + 1);
        let (end, count) = self.table.sizes.extend(self.ordinal, wanted, start)?;
        if count == 0 {
            return Err(Error::Overflow);
        }
        work.take(count - 1);
        self.chunks.byte = end;
        // At most the chunk's samples left.
        self.chunks.chunk_left -= u32::try_from(count).map_err(|_| Error::Overflow)?;
        let first = self.ordinal + 1;
        self.ordinal += count;
        Ok(Next::Item(SampleSpan {
            first,
            count,
            range: start..end,
            fragment: false,
            offset_minimum: None,
        }))
    }
}

// Rows are strictly increasing, 1-based, and range-checked by the builder.
// The cursor starts at sample 1 and visits every ordinal, so the next row is
// either this sample or a later sample. Random lookup keeps the binary path.
fn sequential_contains(
    table: Option<&Words>,
    row: &mut u32,
    ordinal: u64,
    absent: bool,
) -> Result<bool> {
    let Some(table) = table else {
        return Ok(absent);
    };
    if *row < table.count && u64::from(table.word(*row, 0)?) == ordinal {
        *row = (*row).checked_add(1).ok_or(Error::Overflow)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod advancing_sync_tests;

impl<'a> SampleCursor<'a> {
    pub(crate) fn table(&self) -> &'a SampleTable {
        self.table
    }
    /// How many samples the cursor has produced.
    pub(crate) fn ordinal(&self) -> u64 {
        self.at.ordinal
    }
    /// Where the cursor stands, to resume from with [`SampleTable::resume`].
    pub(crate) fn position(&self) -> SampleCursorPosition {
        self.at
    }
    /// The next sample, for one work unit.
    #[inline]
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        if self.at.ordinal == self.table.sample_count() {
            return Ok(Next::End);
        }
        if !work.try_charge(1) {
            return Ok(Next::Yield);
        }
        let mut next = *self;
        let sample = next.advance()?;
        *self = next;
        Ok(Next::Item(sample))
    }
    fn advance(&mut self) -> Result<Sample> {
        let ordinal = self.enter()?;
        let table = self.table;
        let sample = table.sample_with_access(
            SamplePosition {
                ordinal,
                description: self.at.chunks.description,
                byte: self.at.chunks.byte,
                dts: self.at.dts,
                delta: self.at.delta,
                cto: self.at.offset,
            },
            || {
                let sync =
                    sequential_contains(table.sync.as_ref(), &mut self.at.sync_row, ordinal, true)?;
                let partial = sequential_contains(
                    table.partial.as_ref(),
                    &mut self.at.partial_row,
                    ordinal,
                    false,
                )?;
                Ok((sync, partial))
            },
        )?;
        self.leave(ordinal, sample.range.end)?;
        Ok(sample)
    }
    /// Steps past the next sample without building it: its decode time, whether
    /// it is a sync sample, and where its bytes start and how many there are. The
    /// cursor then stands where [`poll`](Self::poll) would leave it.
    pub(crate) fn skip(&mut self) -> Result<SkippedSample> {
        let ordinal = self.enter()?;
        let start = self.at.chunks.byte;
        let size = self.table.sizes.get(ordinal - 1)?;
        let end = start.checked_add(u64::from(size)).ok_or(Error::Overflow)?;
        let sync = sequential_contains(
            self.table.sync.as_ref(),
            &mut self.at.sync_row,
            ordinal,
            true,
        )?;
        let partial = sequential_contains(
            self.table.partial.as_ref(),
            &mut self.at.partial_row,
            ordinal,
            false,
        )?;
        let (dts, delta) = (self.at.dts, self.at.delta);
        self.leave(ordinal, end)?;
        Ok(SkippedSample {
            ordinal,
            dts,
            delta,
            sync,
            partial,
            start,
            size,
        })
    }
    /// Loads the time runs and the chunk that hold the next sample, and returns
    /// its ordinal.
    fn enter(&mut self) -> Result<u64> {
        if self.at.stts_left == 0 {
            self.at.stts_left = self.table.stts.word(self.at.stts_run, 0)?;
            self.at.delta = self.table.stts.word(self.at.stts_run, 4)?;
            self.at.stts_run = self.at.stts_run.checked_add(1).ok_or(Error::Overflow)?;
        }
        if let Some(ctts) = &self.table.ctts
            && self.at.ctts_left == 0
        {
            self.at.ctts_left = ctts.word(self.at.ctts_run, 0)?;
            self.at.offset = ctts.word(self.at.ctts_run, 4)?;
            self.at.ctts_run = self.at.ctts_run.checked_add(1).ok_or(Error::Overflow)?;
        }
        self.at.chunks.enter(self.table)?;
        self.at.ordinal.checked_add(1).ok_or(Error::Overflow)
    }
    /// Steps past sample `ordinal`, which ends at byte `end`.
    fn leave(&mut self, ordinal: u64, end: u64) -> Result<()> {
        self.at.dts = self
            .at
            .dts
            .checked_add(i128::from(self.at.delta))
            .ok_or(Error::Overflow)?;
        self.at.ordinal = ordinal;
        self.at.stts_left = self
            .at
            .stts_left
            .checked_sub(1)
            .ok_or(Error::Invalid("time run exhausted"))?;
        if self.table.ctts.is_some() {
            self.at.ctts_left = self
                .at
                .ctts_left
                .checked_sub(1)
                .ok_or(Error::Invalid("composition run exhausted"))?;
        }
        self.at.chunks.consume(end)
    }
}

/// The decode deltas of a table's samples in order, from its time runs alone:
/// the durations [`SampleCursor`] reports for them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DecodeDeltas<'a> {
    table: &'a SampleTable,
    ordinal: u64,
    run: u32,
    left: u32,
    delta: u32,
}

impl<'a> DecodeDeltas<'a> {
    pub(crate) fn new(table: &'a SampleTable) -> Self {
        Self {
            table,
            ordinal: 0,
            run: 0,
            left: 0,
            delta: 0,
        }
    }
    /// The next sample's decode delta, for a work unit.
    pub(crate) fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<u32>> {
        if self.ordinal == self.table.sample_count() {
            return Ok(Next::End);
        }
        if !work.try_charge(1) {
            return Ok(Next::Yield);
        }
        if self.left == 0 {
            self.left = self.table.stts.word(self.run, 0)?;
            self.delta = self.table.stts.word(self.run, 4)?;
            self.run = self.run.checked_add(1).ok_or(Error::Overflow)?;
        }
        self.left = self
            .left
            .checked_sub(1)
            .ok_or(Error::Invalid("time run exhausted"))?;
        self.ordinal += 1;
        Ok(Next::Item(self.delta))
    }
}

/// A sample [`SampleCursor::skip`] stepped past.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SkippedSample {
    pub(crate) ordinal: u64,
    pub(crate) dts: i128,
    pub(crate) delta: u32,
    pub(crate) sync: bool,
    pub(crate) partial: bool,
    pub(crate) start: u64,
    pub(crate) size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocatePhase {
    Decode,
    Composition,
    Chunk,
    Size,
    Emit,
}

/// Finds one sample of a [`SampleTable`] by ordinal; from [`SampleTable::locate`]. It
/// visits the time and chunk runs before the sample and, for variable sizes, the earlier
/// samples of its chunk, one work unit each.
#[derive(Debug)]
pub struct SampleLocator<'a> {
    table: &'a SampleTable,
    target: u64,
    phase: LocatePhase,
    row: u32,
    skip: u64,
    dts: i128,
    cto: u32,
    delta: u32,
    description: u32,
    chunk: u32,
    first_sample: u64,
    byte: u64,
    size_skip: u64,
    done: bool,
}
impl SampleLocator<'_> {
    /// The located sample once, then [`Next::End`].
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        if self.done {
            return Ok(Next::End);
        }
        loop {
            if !work.try_charge(1) {
                return Ok(Next::Yield);
            }
            match self.phase {
                LocatePhase::Decode => {
                    let count = u64::from(self.table.stts.word(self.row, 0)?);
                    let delta = self.table.stts.word(self.row, 4)?;
                    let take = self.skip.min(count);
                    self.dts = self
                        .dts
                        .checked_add(
                            i128::from(take)
                                .checked_mul(i128::from(delta))
                                .ok_or(Error::Overflow)?,
                        )
                        .ok_or(Error::Overflow)?;
                    if self.skip < count {
                        self.delta = delta;
                        self.phase = LocatePhase::Composition;
                        self.row = 0;
                        self.skip = self.target - 1;
                    } else {
                        self.skip -= count;
                        self.row = self.row.checked_add(1).ok_or(Error::Overflow)?;
                    }
                }
                LocatePhase::Composition => {
                    if let Some(ctts) = &self.table.ctts {
                        let count = u64::from(ctts.word(self.row, 0)?);
                        if self.skip < count {
                            self.cto = ctts.word(self.row, 4)?;
                            self.phase = LocatePhase::Chunk;
                            self.row = 0;
                        } else {
                            self.skip -= count;
                            self.row = self.row.checked_add(1).ok_or(Error::Overflow)?;
                        }
                    } else {
                        self.phase = LocatePhase::Chunk;
                        self.row = 0;
                    }
                }
                LocatePhase::Chunk => {
                    let first = u64::from(self.table.stsc.word(self.row, 0)?);
                    let per = u64::from(self.table.stsc.word(self.row, 4)?);
                    let next = if self.row + 1 < self.table.stsc.count {
                        u64::from(self.table.stsc.word(self.row + 1, 0)?)
                    } else {
                        u64::from(self.table.offsets.table.count) + 1
                    };
                    let covered = (next - first).checked_mul(per).ok_or(Error::Overflow)?;
                    let relative = (self.target - 1)
                        .checked_sub(self.first_sample)
                        .ok_or(Error::Invalid("sample location state"))?;
                    if relative < covered {
                        let chunk = (first - 1)
                            .checked_add(relative / per)
                            .ok_or(Error::Overflow)?;
                        self.chunk = u32::try_from(chunk).map_err(|_| Error::Overflow)?;
                        self.description = self.table.stsc.word(self.row, 8)?;
                        self.byte = self.table.offsets.get(self.chunk)?;
                        self.size_skip = relative % per;
                        self.first_sample = self.target - 1 - self.size_skip;
                        self.phase = LocatePhase::Size;
                    } else {
                        self.first_sample = self
                            .first_sample
                            .checked_add(covered)
                            .ok_or(Error::Overflow)?;
                        self.row = self.row.checked_add(1).ok_or(Error::Overflow)?;
                    }
                }
                LocatePhase::Size => {
                    if self.table.sizes.constant != 0 {
                        let bytes = self
                            .size_skip
                            .checked_mul(u64::from(self.table.sizes.constant))
                            .ok_or(Error::Overflow)?;
                        self.byte = self.byte.checked_add(bytes).ok_or(Error::Overflow)?;
                        self.size_skip = 0;
                    } else if self.size_skip > 0 {
                        let size = self.table.sizes.get(self.first_sample)?;
                        self.byte = self
                            .byte
                            .checked_add(u64::from(size))
                            .ok_or(Error::Overflow)?;
                        self.first_sample =
                            self.first_sample.checked_add(1).ok_or(Error::Overflow)?;
                        self.size_skip -= 1;
                    }
                    if self.size_skip == 0 {
                        self.phase = LocatePhase::Emit;
                    }
                }
                LocatePhase::Emit => {
                    let value = self.table.sample(
                        self.target,
                        self.description,
                        self.byte,
                        self.dts,
                        self.delta,
                        self.cto,
                    )?;
                    self.done = true;
                    return Ok(Next::Item(value));
                }
            }
        }
    }
}

impl SampleTable {
    /// The cursor continuing from `position`, which must come from this table.
    pub(crate) fn resume(&self, position: SampleCursorPosition) -> SampleCursor<'_> {
        SampleCursor {
            table: self,
            at: position,
        }
    }
}
