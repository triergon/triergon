//! Regular sample-table seam. Sequential O(samples + source runs); bounded seek
//! locator works over source run/chunk tables without a per-sample index allocation.
use crate::{
    Access, BoxChildren, BoxStep, BoxView, BuildStatus, Limits, Sample, SampleDescription,
    SampleStep, fields, policy,
    tables::{Offsets, Sizes, Words},
};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

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
    /// None is absent; Some(0) is explicitly present-empty, not all-sync.
    pub fn sync_entry_count(&self) -> Option<u32> {
        self.sync.as_ref().map(|table| table.count)
    }
    /// Borrow compact validated wire runs for the separate native observation
    /// view. Their source version, raw bytes and per-sample meaning stay intact.
    pub(crate) fn composition_runs(&self) -> Option<&Words> {
        self.ctts.as_ref()
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
        let sync = self
            .sync
            .as_ref()
            .map(|v| v.contains(ordinal))
            .transpose()?
            .unwrap_or(true);
        let partial_sync = self
            .partial
            .as_ref()
            .map(|v| v.contains(ordinal))
            .transpose()?
            .unwrap_or(false);
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
}
impl SampleTableBuilder {
    pub fn new(raw: BoxView, budget: Budget, limits: Limits) -> Result<Self> {
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
        if let Err(error) = result {
            if !matches!(error, Error::Backpressure { .. }) {
                self.failed = Some(error);
            }
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
                            return Err(Error::Invalid("time run exceeds sample count"));
                        }
                        self.total = total;
                        self.row += 1;
                        return Ok(());
                    }
                    if self.total != t.sample_count() {
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
                        || description == 0
                        || description > self.description_count
                    {
                        return Err(Error::Invalid("stsc order/count/description"));
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
                if let Some(source) = source {
                    if self.row < source.count {
                        let sample = source.word(self.row, 0)?;
                        if sample == 0
                            || sample <= self.previous
                            || u64::from(sample) > t.sample_count()
                        {
                            return Err(Error::Invalid("sync sample order/range"));
                        }
                        self.previous = sample;
                        self.row += 1;
                        return Ok(());
                    }
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
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.stage != Stage::Ready {
            return Err(Error::Invalid("sample table builder not ready"));
        }
        self.table
            .take()
            .ok_or(Error::Invalid("missing completed index"))
    }
}

/// One sample step performs a fixed number of fields plus <=32 sync-table probes.
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
}
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
        if let Some(ctts) = &self.table.ctts {
            if self.ctts_left == 0 {
                self.ctts_left = ctts.word(self.ctts_run, 0)?;
                self.offset = ctts.word(self.ctts_run, 4)?;
                self.ctts_run = self.ctts_run.checked_add(1).ok_or(Error::Overflow)?;
            }
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
        let sample = self.table.sample(
            ordinal,
            self.description,
            self.byte,
            self.dts,
            self.delta,
            self.offset,
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
