//! Regular sample-table seam. Sequential O(samples + source runs); bounded seek
//! locator works over source run/chunk tables without a per-sample index allocation.
use crate::{
    Access, BoxChildren, BoxStep, BoxView, BuildStatus, Limits, Sample, SampleDescription,
    SampleStep, fields, policy,
    tables::{Offsets, Sizes, Words},
};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimingRunPolicy {
    #[default]
    Strict,
    /// Use the timing prefix covered by the sample sizes. The final decode
    /// run and positive composition runs may extend past that prefix.
    BoundedPrefix,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncSamplePolicy {
    #[default]
    Strict,
    /// Use the increasing prefix before the first entry past the last sample.
    /// The remaining entries stay in the raw table and never provide a seek point.
    BoundedPrefix,
}

/// Validation policies for regular sample tables. Both are strict by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleTablePolicy {
    pub timing_runs: TimingRunPolicy,
    pub sync_samples: SyncSamplePolicy,
}

/// Declared timing occurrences beyond the number of samples with sizes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimingRunSurplus {
    pub decode_samples: u64,
    pub composition_samples: u64,
}

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
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn descriptions(&self) -> &[SampleDescription] {
        self.descriptions.as_slice()
    }
    pub fn sample_count(&self) -> u64 {
        u64::from(self.sizes.count)
    }
    /// The raw table retains the original run counts.
    pub fn timing_run_surplus(&self) -> TimingRunSurplus {
        self.timing_run_surplus
    }
    /// Raw entry count. None is absent; Some(0) is explicitly present-empty.
    pub fn sync_entry_count(&self) -> Option<u32> {
        self.sync
            .as_ref()
            .map(|table| table.count + self.unused_sync_entries)
    }
    /// Number of raw entries at and after the first sync marker past the track.
    pub fn unused_sync_entries(&self) -> u32 {
        self.unused_sync_entries
    }
    /// Preserve presence separately from an explicitly encoded all-zero SDTP.
    /// Internal observation profiles can select a source-backed branch without
    /// rescanning tables or inferring presence from the first sample's value.
    pub(crate) fn has_dependency_table(&self) -> bool {
        self.dependency.is_some()
    }
    /// Borrow compact validated wire runs for the separate demux observation
    /// view. Their source version, raw bytes and per-sample meaning stay intact.
    pub(crate) fn composition_runs(&self) -> Option<&Words> {
        self.ctts.as_ref()
    }
    pub(crate) fn decode_runs(&self) -> &Words {
        &self.stts
    }
    pub fn cursor(&self) -> SampleCursor<'_> {
        SampleCursor {
            table: self,
            ordinal: 0,
            stts_run: 0,
            stts_left: 0,
            delta: 0,
            ctts_run: 0,
            ctts_left: 0,
            offset: 0,
            chunk: 0,
            chunk_left: 0,
            stsc_run: 0,
            description: 0,
            byte: 0,
            dts: 0,
            sync_row: 0,
            partial_row: 0,
        }
    }
    pub fn previous_sync(&self, ordinal: u64) -> Result<Option<u64>> {
        if ordinal == 0 || ordinal > self.sample_count() {
            return Err(Error::Invalid("sync query sample"));
        }
        match &self.sync {
            Some(sync) => sync.previous(ordinal),
            None => Ok(Some(ordinal)),
        }
    }
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
            .map(|table| {
                let (version, _) = fields::full(table.raw.payload())?;
                crate::CompositionOffset::from_wire(version, cto)
            })
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
    ctts: Option<BoxView>,
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
fn required(slot: &Option<BoxView>) -> Result<BoxView> {
    slot.clone()
        .ok_or(Error::Invalid("missing mandatory sample table"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Boxes,
    Descriptions,
    Validate,
    Ready,
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

#[derive(Debug)]
pub struct SampleTableBuilder {
    raw: BoxView,
    limits: Limits,
    boxes: BoxChildren,
    parts: Parts,
    stage: Stage,
    description_boxes: Option<BoxChildren>,
    descriptions: Option<BudgetVec<SampleDescription>>,
    pending_description: Option<SampleDescription>,
    description_count: u32,
    table: Option<SampleTable>,
    check: Check,
    row: u32,
    total: u64,
    previous: u32,
    failed: Option<Error>,
    policy: SampleTablePolicy,
    sync_prefix: Option<u32>,
    recover_sync: bool,
    recover_initial_description: bool,
    recover_descriptions: bool,
    initial_description: Option<InitialDescriptionDiagnostic>,
    coverage: [Option<RunCoverage>; 2],
    sync_diagnostic: Option<SyncDiagnostic>,
}
impl SampleTableBuilder {
    pub fn new(raw: BoxView, budget: Budget, limits: Limits) -> Result<Self> {
        Self::with_timing_policy(raw, budget, limits, TimingRunPolicy::Strict)
    }

    /// Select how timing counts are checked against the sample sizes.
    /// All other table fields use strict validation.
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
            boxes,
            parts: Parts::default(),
            stage: Stage::Boxes,
            description_boxes: None,
            descriptions: Some(BudgetVec::new(&budget)),
            pending_description: None,
            description_count: 0,
            table: None,
            check: Check::Decode,
            row: 0,
            total: 0,
            previous: 0,
            failed: None,
            policy,
            sync_prefix: None,
            recover_sync: false,
            recover_initial_description: false,
            recover_descriptions: false,
            initial_description: None,
            coverage: [None, None],
            sync_diagnostic: None,
        })
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. })
        {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget) -> Result<BuildStatus> {
        loop {
            match self.stage {
                Stage::Ready => return Ok(BuildStatus::Ready),
                Stage::Boxes => match self.boxes.poll(work)? {
                    BoxStep::Yield => return Ok(BuildStatus::Yield),
                    BoxStep::Box(raw) => match &raw.header().kind {
                        b"stsd" => single(&mut self.parts.stsd, raw)?,
                        b"stts" => single(&mut self.parts.stts, raw)?,
                        b"ctts" => single(&mut self.parts.ctts, raw)?,
                        b"stsc" => single(&mut self.parts.stsc, raw)?,
                        b"stsz" => single(&mut self.parts.sizes, raw)?,
                        b"stz2" => single(&mut self.parts.compact, raw)?,
                        b"stco" | b"co64" => single(&mut self.parts.offsets, raw)?,
                        b"stss" => single(&mut self.parts.sync, raw)?,
                        b"stps" => single(&mut self.parts.partial, raw)?,
                        b"sdtp" => single(&mut self.parts.dependency, raw)?,
                        _ => {} // Full raw stbl remains owned, including all opaque metadata.
                    },
                    BoxStep::End => {
                        if self.parts.compact.is_some() && self.parts.sizes.is_some() {
                            return Err(Error::Invalid("both stsz and stz2"));
                        }
                        if let Some(ctts) = &self.parts.ctts {
                            let (version, flags) = fields::full(ctts.payload())?;
                            if version > 1 || flags != 0 {
                                return Err(Error::Unsupported(
                                    "ctts version/flags outside acquired syntax",
                                ));
                            }
                        }
                        let stsd = required(&self.parts.stsd)?;
                        fields::full_zero(stsd.payload())?;
                        self.description_count = fields::u32_at(stsd.payload(), 4)?;
                        if self.description_count == 0 {
                            return Err(Error::Invalid("empty sample descriptions"));
                        }
                        policy(self.description_count, self.limits.max_descriptions)?;
                        self.description_boxes = Some(stsd.children(8, self.limits)?);
                        self.stage = Stage::Descriptions;
                    }
                },
                Stage::Descriptions => {
                    if let Some(value) = self.pending_description.take() {
                        if !work.try_charge(1) {
                            self.pending_description = Some(value);
                            return Ok(BuildStatus::Yield);
                        }
                        if let Err(rejected) = self
                            .descriptions
                            .as_mut()
                            .ok_or(Error::Invalid("missing descriptions"))?
                            .try_push(value)
                        {
                            self.pending_description = Some(rejected.value);
                            return Err(rejected.error);
                        }
                        continue;
                    }
                    let descriptions = self
                        .descriptions
                        .as_ref()
                        .ok_or(Error::Invalid("missing descriptions"))?;
                    match self
                        .description_boxes
                        .as_mut()
                        .ok_or(Error::Invalid("missing description cursor"))?
                        .poll(work)?
                    {
                        BoxStep::Yield => return Ok(BuildStatus::Yield),
                        BoxStep::Box(raw) => {
                            let ordinal = u32::try_from(descriptions.len())
                                .map_err(|_| Error::Overflow)?
                                .checked_add(1)
                                .ok_or(Error::Overflow)?;
                            if ordinal > self.description_count {
                                return Err(Error::Invalid("extra sample description"));
                            }
                            self.pending_description =
                                Some(SampleDescription::parse(raw, ordinal)?);
                        }
                        BoxStep::End => {
                            if descriptions.len() != self.description_count as usize {
                                return Err(Error::Invalid("sample description count mismatch"));
                            }
                            // All constructors below only inspect fixed headers/checked row extents.
                            let stts = Words::parse(required(&self.parts.stts)?, 8, self.limits)?;
                            let stsc = Words::parse(required(&self.parts.stsc)?, 12, self.limits)?;
                            let sizes = Sizes::parse(
                                self.parts
                                    .sizes
                                    .clone()
                                    .or_else(|| self.parts.compact.clone())
                                    .ok_or(Error::Invalid("missing sample size table"))?,
                            )?;
                            let offsets =
                                Offsets::parse(required(&self.parts.offsets)?, self.limits)?;
                            let ctts = self
                                .parts
                                .ctts
                                .clone()
                                .map(|v| Words::parse_ctts(v, self.limits))
                                .transpose()?;
                            let sync = self
                                .parts
                                .sync
                                .clone()
                                .map(|v| Words::parse(v, 4, self.limits))
                                .transpose()?;
                            let partial = self
                                .parts
                                .partial
                                .clone()
                                .map(|v| Words::parse(v, 4, self.limits))
                                .transpose()?;
                            if let Some(raw) = &self.parts.dependency {
                                fields::full_zero(raw.payload())?;
                                fields::exact_rows(raw.payload(), 4, sizes.count, 1)?;
                            }
                            self.table = Some(SampleTable {
                                raw: self.raw.clone(),
                                descriptions: self
                                    .descriptions
                                    .take()
                                    .ok_or(Error::Invalid("missing descriptions"))?,
                                stts,
                                ctts,
                                stsc,
                                sizes,
                                offsets,
                                sync,
                                partial,
                                dependency: self.parts.dependency.clone(),
                                timing_run_surplus: TimingRunSurplus::default(),
                                unused_sync_entries: 0,
                            });
                            self.stage = Stage::Validate;
                        }
                    }
                }
                Stage::Validate => {
                    if self.check == Check::Done {
                        self.stage = Stage::Ready;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    self.validate_one()?;
                }
            }
        }
    }
    fn next_check(&mut self, check: Check) {
        self.check = check;
        self.row = 0;
        self.total = 0;
        self.previous = 0;
    }
    fn validate_one(&mut self) -> Result<()> {
        let t = self.table.as_ref().ok_or(Error::Invalid("missing index"))?;
        match self.check {
            Check::Decode | Check::Composition => {
                let source = if self.check == Check::Decode {
                    Some(&t.stts)
                } else {
                    t.ctts.as_ref()
                };
                if let Some(source) = source {
                    if self.row < source.count {
                        let count = source.word(self.row, 0)?;
                        if count == 0 {
                            return Err(Error::Invalid("zero sample count in time run"));
                        }
                        let total = self
                            .total
                            .checked_add(u64::from(count))
                            .ok_or(Error::Overflow)?;
                        if total > t.sample_count() {
                            let decode = self.check == Check::Decode;
                            if self.policy.timing_runs == TimingRunPolicy::Strict
                                || t.sample_count() == 0
                                || (decode
                                    && (self.row + 1 != source.count
                                        || self.total >= t.sample_count()))
                            {
                                return Err(Error::Invalid("time run exceeds sample count"));
                            }
                            let slot = usize::from(!decode);
                            let first =
                                self.coverage[slot].map_or(self.row, |v| v.first_excess_row);
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
                        self.total = total;
                        self.row += 1;
                        return Ok(());
                    }
                    if self.total != t.sample_count()
                        && !(self.policy.timing_runs == TimingRunPolicy::BoundedPrefix
                            && self.total > t.sample_count())
                    {
                        return Err(Error::Invalid("time table sample count mismatch"));
                    }
                }
                self.next_check(if self.check == Check::Decode {
                    Check::Composition
                } else {
                    Check::Chunks
                });
            }
            Check::Chunks => {
                if self.row < t.stsc.count {
                    let first = t.stsc.word(self.row, 0)?;
                    let per_chunk = t.stsc.word(self.row, 4)?;
                    let description = t.stsc.word(self.row, 8)?;
                    let chunks = u64::from(t.offsets.table.count);
                    let next = if self.row + 1 < t.stsc.count {
                        u64::from(t.stsc.word(self.row + 1, 0)?)
                    } else {
                        chunks + 1
                    };
                    if first == 0
                        || (self.row == 0 && first != 1)
                        || u64::from(first) > chunks
                        || next <= u64::from(first)
                        || next > chunks + 1
                        || per_chunk == 0
                        || (description == 0
                            && !(self.recover_initial_description
                                && self.row == 0
                                && t.stsc.count >= 2
                                && t.stsc.word(1, 0)? == 2
                                && t.stsc.word(1, 8)? == 1
                                && self.description_count == 1)
                            && !self.recover_descriptions)
                        || (description > self.description_count && !self.recover_descriptions)
                    {
                        return Err(Error::Invalid("stsc order/count/description"));
                    }
                    if description == 0 && !self.recover_descriptions {
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
                    self.total = self.total.checked_add(span).ok_or(Error::Overflow)?;
                    if self.total > t.sample_count() {
                        return Err(Error::Invalid("chunks exceed sample count"));
                    }
                    self.row += 1;
                    return Ok(());
                }
                if self.total != t.sample_count()
                    || (t.sample_count() == 0 && t.offsets.table.count != 0)
                {
                    return Err(Error::Invalid("chunk coverage mismatch"));
                }
                self.next_check(Check::Sync);
            }
            Check::Sync | Check::Partial => {
                let source = if self.check == Check::Sync {
                    t.sync.as_ref()
                } else {
                    t.partial.as_ref()
                };
                if let Some(source) = source
                    && self.row < source.count
                {
                    let sample = source.word(self.row, 0)?;
                    let past_end = u64::from(sample) > t.sample_count();
                    if sample == 0 || sample <= self.previous || past_end {
                        if past_end
                            && self.check == Check::Sync
                            && self.policy.sync_samples == SyncSamplePolicy::BoundedPrefix
                        {
                            self.sync_prefix = Some(self.row);
                            self.next_check(Check::Partial);
                            return Ok(());
                        }
                        if !self.recover_sync || self.check != Check::Sync || sample == 0 {
                            return Err(Error::Invalid("sync sample order/range"));
                        }
                        if self.sync_diagnostic.is_none() {
                            let start = source
                                .raw
                                .header()
                                .start
                                .checked_add(u64::from(source.raw.header().header_size))
                                .and_then(|v| v.checked_add(8 + u64::from(self.row) * 4))
                                .ok_or(Error::Overflow)?;
                            self.sync_diagnostic = Some(SyncDiagnostic {
                                first_bad_row: self.row,
                                source_value: sample,
                                field_start: start,
                                field_end: start.checked_add(4).ok_or(Error::Overflow)?,
                                strict_error: Error::Invalid("sync sample order/range"),
                            });
                        }
                    }
                    self.previous = sample;
                    self.row += 1;
                    return Ok(());
                }
                self.next_check(if self.check == Check::Sync {
                    Check::Partial
                } else {
                    Check::Done
                });
            }
            Check::Done => {}
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<SampleTable> {
        if self.recover_sync {
            return Err(Error::Invalid(
                "recovered table cannot become a Strict sample table",
            ));
        }
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.stage != Stage::Ready {
            return Err(Error::Invalid("sample table builder not ready"));
        }
        let mut table = self
            .table
            .take()
            .ok_or(Error::Invalid("missing completed index"))?;
        if self.policy.timing_runs == TimingRunPolicy::BoundedPrefix {
            let [decode, composition] = self.coverage;
            table.timing_run_surplus = TimingRunSurplus {
                decode_samples: decode.map_or(0, |run| run.surplus_samples),
                composition_samples: composition.map_or(0, |run| run.surplus_samples),
            };
        }
        if let Some(prefix) = self.sync_prefix
            && let Some(sync) = &mut table.sync
        {
            table.unused_sync_entries = sync.count - prefix;
            sync.count = prefix;
        }
        Ok(table)
    }
}

/// One sample step checks at most one row in each validated sync table.
/// It allocates nothing and commits cursor changes only after all checks succeed.
#[derive(Clone, Copy, Debug)]
pub struct SampleCursor<'a> {
    table: &'a SampleTable,
    ordinal: u64,
    stts_run: u32,
    stts_left: u32,
    delta: u32,
    ctts_run: u32,
    ctts_left: u32,
    offset: u32,
    chunk: u32,
    chunk_left: u32,
    stsc_run: u32,
    description: u32,
    byte: u64,
    dts: i128,
    sync_row: u32,
    partial_row: u32,
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

impl SampleCursor<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        if self.ordinal == self.table.sample_count() {
            return Ok(SampleStep::End);
        }
        if !work.try_charge(1) {
            return Ok(SampleStep::Yield);
        }
        let mut next = *self;
        let sample = next.advance()?;
        *self = next;
        Ok(SampleStep::Sample(sample))
    }
    fn advance(&mut self) -> Result<Sample> {
        if self.stts_left == 0 {
            self.stts_left = self.table.stts.word(self.stts_run, 0)?;
            self.delta = self.table.stts.word(self.stts_run, 4)?;
            self.stts_run = self.stts_run.checked_add(1).ok_or(Error::Overflow)?;
        }
        if let Some(ctts) = &self.table.ctts
            && self.ctts_left == 0
        {
            self.ctts_left = ctts.word(self.ctts_run, 0)?;
            self.offset = ctts.word(self.ctts_run, 4)?;
            self.ctts_run = self.ctts_run.checked_add(1).ok_or(Error::Overflow)?;
        }
        if self.chunk_left == 0 {
            let chunk_number = u64::from(self.chunk) + 1;
            if self.stsc_run + 1 < self.table.stsc.count
                && u64::from(self.table.stsc.word(self.stsc_run + 1, 0)?) == chunk_number
            {
                self.stsc_run += 1;
            }
            self.chunk_left = self.table.stsc.word(self.stsc_run, 4)?;
            self.description = self.table.stsc.word(self.stsc_run, 8)?;
            self.byte = self.table.offsets.get(self.chunk)?;
            self.chunk = self.chunk.checked_add(1).ok_or(Error::Overflow)?;
        }
        let ordinal = self.ordinal.checked_add(1).ok_or(Error::Overflow)?;
        let table = self.table;
        let sample = table.sample_with_access(
            SamplePosition {
                ordinal,
                description: self.description,
                byte: self.byte,
                dts: self.dts,
                delta: self.delta,
                cto: self.offset,
            },
            || {
                let sync =
                    sequential_contains(table.sync.as_ref(), &mut self.sync_row, ordinal, true)?;
                let partial = sequential_contains(
                    table.partial.as_ref(),
                    &mut self.partial_row,
                    ordinal,
                    false,
                )?;
                Ok((sync, partial))
            },
        )?;
        self.byte = sample.range.end;
        self.dts = self
            .dts
            .checked_add(i128::from(self.delta))
            .ok_or(Error::Overflow)?;
        self.ordinal = ordinal;
        self.stts_left = self
            .stts_left
            .checked_sub(1)
            .ok_or(Error::Invalid("time run exhausted"))?;
        if self.table.ctts.is_some() {
            self.ctts_left = self
                .ctts_left
                .checked_sub(1)
                .ok_or(Error::Invalid("composition run exhausted"))?;
        }
        self.chunk_left = self
            .chunk_left
            .checked_sub(1)
            .ok_or(Error::Invalid("chunk exhausted"))?;
        Ok(sample)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocatePhase {
    Decode,
    Composition,
    Chunk,
    Size,
    Emit,
}

/// Resumable O(time runs + chunk runs + variable sizes within one chunk).
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
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        if self.done {
            return Ok(SampleStep::End);
        }
        loop {
            if !work.try_charge(1) {
                return Ok(SampleStep::Yield);
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
                    return Ok(SampleStep::Sample(value));
                }
            }
        }
    }
}

/// Explicit opt-in to the observed reference MOV run-prefix operation. This does not
/// change source validity, Strict defaults, packet order or codec configuration.
#[derive(Clone, Copy, Debug)]
pub struct RunRecoveryProfile;
#[cfg(feature = "reference-trace")]
impl RunRecoveryProfile {
    pub const REFERENCE_WORKER_SHA256: &'static str =
        "7ff6742eaafa59884b6dc95f65c701c26b18975c265aa8401d61154eb2ef1204";
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    Decode,
    Composition,
}
/// Original run coverage and the first offending source count field. Row zero
/// is the first encoded row. The full raw table, including unused runs, is owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunCoverage {
    pub kind: RunKind,
    pub first_excess_row: u32,
    pub count_field_start: u64,
    pub count_field_end: u64,
    pub declared_samples: u64,
    pub bounded_samples: u64,
    pub surplus_samples: u64,
    pub strict_error: Error,
}
/// Separate bounded builder. Only the final positive STTS run may overcover;
/// CTTS may have an overcovering positive prefix and positive trailing runs.
/// All other validation/limits/pool identity checks are the Strict checks.
#[derive(Debug)]
pub struct RunRecoveryBuilder {
    inner: SampleTableBuilder,
}
impl RunRecoveryBuilder {
    pub fn new(
        raw: BoxView,
        budget: Budget,
        limits: Limits,
        _profile: RunRecoveryProfile,
    ) -> Result<Self> {
        let mut inner = SampleTableBuilder::with_timing_policy(
            raw,
            budget,
            limits,
            TimingRunPolicy::BoundedPrefix,
        )?;
        inner.recover_sync = true;
        Ok(Self { inner })
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<BuildStatus> {
        if let Some(error) = self.inner.failed {
            return Err(error);
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            self.inner.failed = Some(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        self.inner.poll(work)
    }
    pub fn finish(mut self) -> Result<RunRecoveryTable> {
        if let Some(error) = self.inner.failed {
            return Err(error);
        }
        if self.inner.stage != Stage::Ready {
            return Err(Error::Invalid("run recovery builder not ready"));
        }
        Ok(RunRecoveryTable {
            storage: self
                .inner
                .table
                .take()
                .ok_or(Error::Invalid("missing recovery storage"))?,
            coverage: self.inner.coverage,
            sync_diagnostic: self.inner.sync_diagnostic,
            initial_description: self.inner.initial_description,
        })
    }
}
/// This deliberately has no Deref/conversion/accessor to a Strict SampleTable.
/// Private storage is shared with the partially validated Strict builder; only
/// this labelled type can expose the bounded observations after a recovery.
#[derive(Debug)]
pub struct RunRecoveryTable {
    storage: SampleTable,
    coverage: [Option<RunCoverage>; 2],
    sync_diagnostic: Option<SyncDiagnostic>,
    initial_description: Option<InitialDescriptionDiagnostic>,
}
impl RunRecoveryTable {
    pub fn raw(&self) -> &BoxView {
        self.storage.raw()
    }
    pub fn descriptions(&self) -> &[SampleDescription] {
        self.storage.descriptions()
    }
    pub fn coverage(&self) -> &[Option<RunCoverage>; 2] {
        &self.coverage
    }
    /// Encoded facts for the first STSZ count samples. This is not a claim that
    /// the source table is valid. Coverage diagnostics must accompany the result.
    pub fn source_prefix_cursor(&self) -> RecoverySourceCursor<'_> {
        RecoverySourceCursor {
            inner: self.storage.cursor(),
            unknown_access: self.sync_diagnostic.is_some(),
        }
    }
    pub fn sync_diagnostic(&self) -> Option<SyncDiagnostic> {
        self.sync_diagnostic
    }
    pub fn stream_view_for_recovery_header(
        &self,
        header: &crate::RecoveryTrackHeader,
        major_brand: Option<[u8; 4]>,
    ) -> Result<crate::StreamView> {
        let entry = self
            .storage
            .descriptions()
            .first()
            .ok_or(Error::Invalid("missing recovery description"))?;
        if self.storage.sample_count() == 0 {
            return Err(Error::Unsupported("empty recovery stream"));
        }
        let mut stream = crate::StreamView::with_projection_base(
            header.source(),
            entry,
            header.demux_projection_base()?,
        )?;
        if stream.codec == "rawvideo" {
            match major_brand {
                Some(brand) if brand == *b"qt  " => {}
                Some(_) => stream.vendor_id = None,
                None => return Err(Error::Unsupported("native major-brand context absent")),
            }
        }
        Ok(stream)
    }
    pub fn stream_view(
        &self,
        header: &crate::TrackHeader,
        major_brand: Option<[u8; 4]>,
    ) -> Result<crate::StreamView> {
        crate::StreamView::for_source(header, &self.storage, false, major_brand)
    }
    fn track(&self, track_id: u32, media_base: triergon_core::TimeBase) -> crate::DemuxTrack<'_> {
        crate::DemuxTrack {
            track_id,
            table: &self.storage,
            fragments: &[],
            media_base,
            edits: None,
        }
    }
    /// Demux extraction is separate from source validity/clock knownness.
    pub fn packet_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
        media: &'a [std::ops::Range<u64>],
        limits: crate::PacketLimits,
    ) -> Result<RecoveryPacketCursor<'a>> {
        if self.initial_description.is_some() {
            return Err(Error::Unsupported(
                "initial-description recovery requires explicit v2 cursor",
            ));
        }
        let inner = crate::PacketCursor::new_complete_source(
            pool,
            &[self.track(header.source().track_id, header.demux_projection_base()?)],
            media,
            limits,
            crate::DemuxProfile::NoParseNoFillin,
        )?;
        Ok(RecoveryPacketCursor {
            inner,
            unknown_access: self.sync_diagnostic.is_some(),
            source_timescale: header.source().timescale,
        })
    }
    pub fn index_cursor<'a>(
        &'a self,
        pool: &Budget,
        header: &crate::RecoveryTrackHeader,
        media: &'a [std::ops::Range<u64>],
        limits: crate::IndexLimits,
    ) -> Result<RecoveryIndexCursor<'a>> {
        if self.initial_description.is_some() {
            return Err(Error::Unsupported(
                "initial-description recovery requires explicit v2 cursor",
            ));
        }
        let mut inner = crate::IndexCursor::new_complete_source(
            pool,
            self.track(header.source().track_id, header.demux_projection_base()?),
            media,
            limits,
            crate::DemuxProfile::NoParseNoFillin,
        )?;
        inner.allow_recovery_initial_nonkey();
        Ok(RecoveryIndexCursor {
            inner,
            unknown_access: self.sync_diagnostic.is_some(),
            source_timescale: header.source().timescale,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncDiagnostic {
    pub first_bad_row: u32,
    pub source_value: u32,
    pub field_start: u64,
    pub field_end: u64,
    pub strict_error: Error,
}
/// A recovered declaration whose sync rows were rejected has unknown access.
fn recovery_access(mut source: Sample, unknown_access: bool) -> Sample {
    if unknown_access {
        source.access = Access::Unknown;
    }
    source
}
#[derive(Debug)]
pub struct RecoverySourceCursor<'a> {
    inner: SampleCursor<'a>,
    unknown_access: bool,
}
impl RecoverySourceCursor<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        match self.inner.poll(work)? {
            SampleStep::Sample(source) => Ok(SampleStep::Sample(recovery_access(
                source,
                self.unknown_access,
            ))),
            other => Ok(other),
        }
    }
}
/// Demux ticks are observations; source_timescale zero means that conversion
/// to source seconds is invalid, even though the original raw ticks are known.
#[derive(Debug)]
pub struct RecoveryOccurrence {
    pub track_id: u32,
    pub source: Sample,
    pub demux: crate::PacketView,
    pub source_timescale: u32,
}
#[derive(Debug)]
pub struct RecoveryPacketCursor<'a> {
    inner: crate::PacketCursor<'a>,
    unknown_access: bool,
    source_timescale: u32,
}
impl RecoveryPacketCursor<'_> {
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::CursorStatus> {
        self.inner.poll(work, cancel)
    }
    pub fn take_occurrence(&mut self) -> Result<RecoveryOccurrence> {
        let row = self.inner.take_occurrence()?;
        Ok(RecoveryOccurrence {
            source: recovery_access(row.source, self.unknown_access),
            demux: row.demux,
            track_id: row.track_id,
            source_timescale: self.source_timescale,
        })
    }
}
#[derive(Debug)]
pub struct RecoveryIndexEntry {
    pub ordinal: u64,
    pub position: i64,
    pub timestamp: i64,
    pub flags: i32,
    pub size: u32,
    pub min_distance: i32,
    pub source: Sample,
    pub source_timescale: u32,
}
#[derive(Debug)]
pub struct RecoveryIndexCursor<'a> {
    inner: crate::IndexCursor<'a>,
    unknown_access: bool,
    source_timescale: u32,
}
impl RecoveryIndexCursor<'_> {
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::CursorStatus> {
        self.inner.poll(work, cancel)
    }
    pub fn take_entry(&mut self) -> Result<RecoveryIndexEntry> {
        let e = self.inner.take_entry()?;
        Ok(RecoveryIndexEntry {
            ordinal: e.ordinal,
            position: e.position,
            timestamp: e.timestamp,
            flags: e.flags,
            size: e.size,
            min_distance: e.min_distance,
            source: recovery_access(e.occurrence.source, self.unknown_access),
            source_timescale: self.source_timescale,
        })
    }
}

mod direct_recovery;
pub use direct_recovery::*;

mod description_recovery;
pub use description_recovery::*;

/// A [`SampleCursor`]'s place in its table, kept without borrowing the table so
/// an owner can hold both.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SampleCursorPosition {
    ordinal: u64,
    stts_run: u32,
    stts_left: u32,
    delta: u32,
    ctts_run: u32,
    ctts_left: u32,
    offset: u32,
    chunk: u32,
    chunk_left: u32,
    stsc_run: u32,
    description: u32,
    byte: u64,
    dts: i128,
    sync_row: u32,
    partial_row: u32,
}

impl SampleTable {
    /// The cursor continuing from `position`, which must come from this table.
    pub(crate) fn resume(&self, position: SampleCursorPosition) -> SampleCursor<'_> {
        SampleCursor {
            table: self,
            ordinal: position.ordinal,
            stts_run: position.stts_run,
            stts_left: position.stts_left,
            delta: position.delta,
            ctts_run: position.ctts_run,
            ctts_left: position.ctts_left,
            offset: position.offset,
            chunk: position.chunk,
            chunk_left: position.chunk_left,
            stsc_run: position.stsc_run,
            description: position.description,
            byte: position.byte,
            dts: position.dts,
            sync_row: position.sync_row,
            partial_row: position.partial_row,
        }
    }
}

impl SampleCursor<'_> {
    pub(crate) fn position(&self) -> SampleCursorPosition {
        SampleCursorPosition {
            ordinal: self.ordinal,
            stts_run: self.stts_run,
            stts_left: self.stts_left,
            delta: self.delta,
            ctts_run: self.ctts_run,
            ctts_left: self.ctts_left,
            offset: self.offset,
            chunk: self.chunk,
            chunk_left: self.chunk_left,
            stsc_run: self.stsc_run,
            description: self.description,
            byte: self.byte,
            dts: self.dts,
            sync_row: self.sync_row,
            partial_row: self.partial_row,
        }
    }
}
