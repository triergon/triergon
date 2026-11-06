//! Legacy fragment syntax/default precedence from Adobe F4V 10.1 §§2.9, 2.12.
//! tfdt: ETSI TS 126 244 V18.0.0 §13.5. The moof-base bit uses acquired ISO
//! 14496-12:2008/FDAmd2 addition to §8.8.7.1, with the published ETSI profile's
//! named flag semantics. Selected signed trun v1 uses FDAM3 replacement8.8.8.2
//! and MPEG m76694 table9 Original, experimentally. Final clauses/brands stay open.
use crate::{
    Access, BoxChildren, BoxStep, BoxView, BuildStatus, FragmentDecodeTime, Limits, Sample,
    SampleStep, add_offset, fields, policy,
};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trex {
    pub track_id: u32,
    pub description_index: u32,
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
}
impl Trex {
    pub fn parse(raw: &BoxView) -> Result<Self> {
        if raw.header().kind != *b"trex" {
            return Err(Error::Invalid("not trex"));
        }
        let b = raw.payload();
        fields::full_zero(b)?;
        fields::exact_rows(b, 24, 0, 0)?;
        let track_id = fields::u32_at(b, 4)?;
        if track_id == 0 {
            return Err(Error::Invalid("zero trex track ID"));
        }
        Ok(Self {
            track_id,
            description_index: fields::u32_at(b, 8)?,
            duration: fields::u32_at(b, 12)?,
            size: fields::u32_at(b, 16)?,
            flags: fields::u32_at(b, 20)?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ImplicitBase {
    FirstTraf,
    /// Caller has established the preceding traf's data endpoint in this moof.
    PreviousTraf(u64),
}
#[derive(Clone, Copy, Debug)]
pub struct FragmentContext {
    pub moof_start: u64,
    pub implicit_base: ImplicitBase,
    /// Independently established origin if tfdt is absent. When tfdt is present,
    /// a supplied value must agree. Absence never silently implies time zero.
    pub decode_origin: Option<i128>,
    pub first_ordinal: u64,
    pub description_count: u32,
}

#[derive(Clone, Copy, Debug)]
struct Tfhd {
    track_id: u32,
    base: Option<u64>,
    base_is_moof: bool,
    description: Option<u32>,
    duration: Option<u32>,
    size: Option<u32>,
    flags: Option<u32>,
    empty: bool,
}
impl Tfhd {
    fn parse(raw: &BoxView) -> Result<Self> {
        let b = raw.payload();
        let (version, flags) = fields::full(b)?;
        if version != 0 || flags & !0x03003b != 0 {
            return Err(Error::Unsupported(
                "tfhd version/flags outside acquired syntax",
            ));
        }
        let mut at = 8usize;
        let base = if flags & 1 != 0 {
            let value = fields::u64_at(b, at)?;
            at += 8;
            Some(value)
        } else {
            None
        };
        let mut read = |bit: u32| -> Result<Option<u32>> {
            if flags & bit == 0 {
                return Ok(None);
            }
            let value = fields::u32_at(b, at)?;
            at += 4;
            Ok(Some(value))
        };
        let description = read(2)?;
        let duration = read(8)?;
        let size = read(16)?;
        let sample_flags = read(32)?;
        fields::exact_rows(b, at, 0, 0)?;
        let track_id = fields::u32_at(b, 4)?;
        if track_id == 0 {
            return Err(Error::Invalid("zero tfhd track ID"));
        }
        Ok(Self {
            track_id,
            base,
            base_is_moof: flags & 0x020000 != 0,
            description,
            duration,
            size,
            flags: sample_flags,
            empty: flags & 0x010000 != 0,
        })
    }
    fn effective(self, trex: Trex) -> Result<Trex> {
        if self.track_id != trex.track_id {
            return Err(Error::Invalid("tfhd/trex track ID mismatch"));
        }
        Ok(Trex {
            track_id: trex.track_id,
            description_index: self.description.unwrap_or(trex.description_index),
            duration: self.duration.unwrap_or(trex.duration),
            size: self.size.unwrap_or(trex.size),
            flags: self.flags.unwrap_or(trex.flags),
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct Values {
    duration: u32,
    size: u32,
    flags: u32,
    cto: Option<crate::CompositionOffset>,
}
#[derive(Debug)]
struct Run {
    raw: BoxView,
    version: u8,
    count: u32,
    data_offset: Option<i32>,
    first_flags: Option<u32>,
    fields: u32,
    start: usize,
    stride: usize,
}
impl Run {
    fn parse(raw: BoxView, limits: Limits) -> Result<Self> {
        let b = raw.payload();
        let (version, flags) = fields::full(b)?;
        // Experimental signed v1 field types from ISO FDAM3 replacement8.8.8.2
        // and MPEG m76694 table9 Original. No arbitrary nonzero-version rule.
        if version > 1 || flags & !0x000f05 != 0 {
            return Err(Error::Unsupported(
                "trun version/flags outside acquired syntax",
            ));
        }
        if flags & 4 != 0 && flags & 0x400 != 0 {
            return Err(Error::Invalid("both first and per-sample flags"));
        }
        let count = fields::u32_at(b, 4)?;
        let mut at = 8usize;
        let data_offset = if flags & 1 != 0 {
            let v = fields::i32_at(b, at)?;
            at += 4;
            Some(v)
        } else {
            None
        };
        let first_flags = if flags & 4 != 0 {
            let v = fields::u32_at(b, at)?;
            at += 4;
            Some(v)
        } else {
            None
        };
        let stride = usize::try_from((flags & 0xf00).count_ones())
            .map_err(|_| Error::Overflow)?
            .checked_mul(4)
            .ok_or(Error::Overflow)?;
        fields::exact_rows(b, at, count, stride)?;
        if stride != 0 {
            policy(count, limits.max_records)?;
        }
        Ok(Self {
            raw,
            version,
            count,
            data_offset,
            first_flags,
            fields: flags,
            start: at,
            stride,
        })
    }
    fn values(&self, index: u32, defaults: Trex) -> Result<Values> {
        if index >= self.count {
            return Err(Error::Invalid("fragment sample index"));
        }
        let mut at = usize::try_from(index)
            .map_err(|_| Error::Overflow)?
            .checked_mul(self.stride)
            .and_then(|v| v.checked_add(self.start))
            .ok_or(Error::Overflow)?;
        let mut read = |bit: u32, fallback: u32| -> Result<u32> {
            if self.fields & bit == 0 {
                return Ok(fallback);
            }
            let value = fields::u32_at(self.raw.payload(), at)?;
            at += 4;
            Ok(value)
        };
        let duration = read(0x100, defaults.duration)?;
        let size = read(0x200, defaults.size)?;
        let flags = read(
            0x400,
            if index == 0 {
                self.first_flags.unwrap_or(defaults.flags)
            } else {
                defaults.flags
            },
        )?;
        let raw_cto = read(0x800, 0)?;
        let cto = if self.fields & 0x800 != 0 {
            Some(crate::CompositionOffset::from_wire(self.version, raw_cto)?)
        } else {
            None
        };
        Ok(Values {
            duration,
            size,
            flags,
            cto,
        })
    }
    fn constant_extents(&self) -> bool {
        self.fields & 0x300 == 0
    }
}

#[derive(Debug)]
struct IndexedRun {
    wire: Run,
    byte: u64,
    decode: Option<i128>,
    ordinal: u64,
    bytes: u64,
    duration: u64,
}

#[derive(Debug)]
pub struct FragmentIndex {
    raw: BoxView,
    decode_time: Option<FragmentDecodeTime>,
    runs: BudgetVec<IndexedRun>,
    defaults: Trex,
    first_ordinal: u64,
    count: u64,
    data_end: u64,
    implicit_safe: bool,
    decode_end: Option<i128>,
}
impl FragmentIndex {
    /// Format-local source syntax provenance; no native distance semantics.
    pub(crate) fn run_flags(&self, run: usize) -> Result<u32> {
        self.runs
            .as_slice()
            .get(run)
            .map(|r| r.wire.fields)
            .ok_or(Error::Invalid("fragment source run absent"))
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn decode_time(&self) -> Option<&FragmentDecodeTime> {
        self.decode_time.as_ref()
    }
    pub fn sample_count(&self) -> u64 {
        self.count
    }
    /// Original container sample ordinal, not a clock or request generation.
    pub(crate) fn first_ordinal(&self) -> u64 {
        self.first_ordinal
    }
    pub fn track_id(&self) -> u32 {
        self.defaults.track_id
    }
    pub fn description_index(&self) -> u32 {
        self.defaults.description_index
    }
    pub fn data_end(&self) -> u64 {
        self.data_end
    }
    /// The last source-order run endpoint is an unambiguous following-traf base
    /// only in this nonempty, forward/nonoverlapping legacy run profile. Backward
    /// explicit runs remain preserved but cannot silently establish this context.
    pub fn implicit_data_end(&self) -> Result<u64> {
        if !self.implicit_safe || self.count == 0 {
            return Err(Error::Unsupported(
                "implicit predecessor with empty/backward/overlapping runs",
            ));
        }
        Ok(self.data_end)
    }
    pub fn decode_end(&self) -> Option<i128> {
        self.decode_end
    }
    pub fn cursor(&self) -> FragmentCursor<'_> {
        FragmentCursor {
            index: self,
            run: 0,
            sample: 0,
            byte: 0,
            decode: None,
        }
    }
    pub fn locate(&self, ordinal: u64) -> Result<FragmentLocator<'_>> {
        let end = self
            .first_ordinal
            .checked_add(self.count)
            .ok_or(Error::Overflow)?;
        if ordinal < self.first_ordinal || ordinal >= end {
            return Err(Error::Invalid("fragment locate sample"));
        }
        Ok(FragmentLocator {
            index: self,
            cursor: None,
            low: 0,
            high: self.runs.len(),
            target: ordinal,
            done: false,
            failed: None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Boxes,
    Initialize,
    StartRun,
    Sum,
    Commit,
    Ready,
}

#[derive(Debug)]
pub struct FragmentBuilder {
    raw: BoxView,
    boxes: BoxChildren,
    runs: Option<BudgetVec<IndexedRun>>,
    pending: Option<IndexedRun>,
    header: Option<Tfhd>,
    decode_time: Option<FragmentDecodeTime>,
    trex: Trex,
    context: FragmentContext,
    limits: Limits,
    phase: Phase,
    defaults: Option<Trex>,
    base: u64,
    byte: u64,
    decode: Option<i128>,
    ordinal: u64,
    run: usize,
    sample: u32,
    total_bytes: u64,
    total_duration: u64,
    implicit_safe: bool,
    reject_zero_size: bool,
    failed: Option<Error>,
}
impl FragmentBuilder {
    pub fn new(
        raw: BoxView,
        trex: Trex,
        context: FragmentContext,
        budget: Budget,
        limits: Limits,
    ) -> Result<Self> {
        if raw.header().kind != *b"traf" {
            return Err(Error::Invalid("not traf"));
        }
        if !raw.bytes().belongs_to(&budget) {
            return Err(Error::Invalid(
                "fragment and index use different budget pools",
            ));
        }
        if context.first_ordinal == 0 {
            return Err(Error::Invalid("zero first fragment ordinal"));
        }
        let boxes = raw.children(0, limits)?;
        Ok(Self {
            raw,
            boxes,
            runs: Some(BudgetVec::new(&budget)),
            pending: None,
            header: None,
            decode_time: None,
            trex,
            context,
            limits,
            phase: Phase::Boxes,
            defaults: None,
            base: 0,
            byte: 0,
            decode: context.decode_origin,
            ordinal: context.first_ordinal,
            run: 0,
            sample: 0,
            total_bytes: 0,
            total_duration: 0,
            implicit_safe: true,
            reject_zero_size: false,
            failed: None,
        })
    }
    /// The pinned MOV receiver rejects a zero-size sample in a fragment
    /// reached during header opening. Source and packet-stage parsing retain it.
    pub fn new_pinned_mov_header(
        raw: BoxView,
        trex: Trex,
        context: FragmentContext,
        budget: Budget,
        limits: Limits,
    ) -> Result<Self> {
        let mut builder = Self::new(raw, trex, context, budget, limits)?;
        builder.reject_zero_size = true;
        Ok(builder)
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
            if self.phase == Phase::Ready {
                return Ok(BuildStatus::Ready);
            }
            if self.phase == Phase::Boxes {
                if let Some(value) = self.pending.take() {
                    if !work.try_charge(1) {
                        self.pending = Some(value);
                        return Ok(BuildStatus::Yield);
                    }
                    let runs = self
                        .runs
                        .as_mut()
                        .ok_or(Error::Invalid("missing fragment runs"))?;
                    if let Err(error) = runs.try_push(value) {
                        self.pending = Some(error.value);
                        return Err(error.error);
                    }
                    continue;
                }
                match self.boxes.poll(work)? {
                    BoxStep::Yield => return Ok(BuildStatus::Yield),
                    BoxStep::End => {
                        self.phase = Phase::Initialize;
                    }
                    BoxStep::Box(raw) => match &raw.header().kind {
                        b"tfhd" => {
                            if self.header.is_some() {
                                return Err(Error::Invalid("duplicate tfhd"));
                            }
                            self.header = Some(Tfhd::parse(&raw)?);
                        }
                        b"trun" => {
                            let count = self
                                .runs
                                .as_ref()
                                .ok_or(Error::Invalid("missing fragment runs"))?
                                .len();
                            policy(
                                u32::try_from(count)
                                    .map_err(|_| Error::Overflow)?
                                    .checked_add(1)
                                    .ok_or(Error::Overflow)?,
                                self.limits.max_records,
                            )?;
                            self.pending = Some(IndexedRun {
                                wire: Run::parse(raw, self.limits)?,
                                byte: 0,
                                decode: None,
                                ordinal: 0,
                                bytes: 0,
                                duration: 0,
                            });
                        }
                        b"tfdt" => {
                            if self.decode_time.is_some() {
                                return Err(Error::Invalid("duplicate tfdt"));
                            }
                            if self.header.is_none()
                                || self.runs.as_ref().is_some_and(|r| !r.is_empty())
                            {
                                return Err(Error::Invalid(
                                    "tfdt must follow tfhd and precede trun",
                                ));
                            }
                            self.decode_time = Some(FragmentDecodeTime::parse(raw)?);
                        }
                        _ => {} // Groups/private boxes remain byte-exact in raw traf.
                    },
                }
                continue;
            }
            let length = self
                .runs
                .as_ref()
                .ok_or(Error::Invalid("missing fragment runs"))?
                .len();
            if self.phase == Phase::StartRun && self.run == length {
                self.phase = Phase::Ready;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            match self.phase {
                Phase::Initialize => {
                    let header = self.header.ok_or(Error::Invalid("missing tfhd"))?;
                    if let Some(wire) = &self.decode_time {
                        let origin = i128::from(wire.ticks());
                        if self
                            .context
                            .decode_origin
                            .is_some_and(|caller| caller != origin)
                        {
                            return Err(Error::Invalid("caller origin disagrees with tfdt"));
                        }
                        self.decode = Some(origin);
                    }
                    let defaults = header.effective(self.trex)?;
                    if defaults.description_index == 0
                        || defaults.description_index > self.context.description_count
                    {
                        return Err(Error::Invalid("fragment sample description index"));
                    }
                    if header.empty && length != 0 {
                        return Err(Error::Invalid("duration-is-empty with trun boxes"));
                    }
                    // An explicit base takes precedence. Without it the modern
                    // bit anchors this traf independently of any predecessor.
                    self.base = header.base.unwrap_or(if header.base_is_moof {
                        self.context.moof_start
                    } else {
                        match self.context.implicit_base {
                            ImplicitBase::FirstTraf => self.context.moof_start,
                            ImplicitBase::PreviousTraf(end) => end,
                        }
                    });
                    self.byte = self.base;
                    self.defaults = Some(defaults);
                    self.phase = Phase::StartRun;
                }
                Phase::StartRun => {
                    let run = self
                        .runs
                        .as_mut()
                        .ok_or(Error::Invalid("missing runs"))?
                        .as_mut_slice()
                        .get_mut(self.run)
                        .ok_or(Error::Invalid("run index"))?;
                    run.byte = match run.wire.data_offset {
                        Some(delta) => add_offset(self.base, delta)?,
                        None => self.byte,
                    };
                    if run.wire.count == 0 || (self.run > 0 && run.byte < self.byte) {
                        self.implicit_safe = false;
                    }
                    run.decode = self.decode;
                    run.ordinal = self.ordinal;
                    self.sample = 0;
                    self.total_bytes = 0;
                    self.total_duration = 0;
                    self.phase = Phase::Sum;
                }
                Phase::Sum => {
                    let run = self
                        .runs
                        .as_ref()
                        .ok_or(Error::Invalid("missing runs"))?
                        .as_slice()
                        .get(self.run)
                        .ok_or(Error::Invalid("run index"))?;
                    let defaults = self
                        .defaults
                        .ok_or(Error::Invalid("missing fragment defaults"))?;
                    if run.wire.constant_extents() {
                        if self.reject_zero_size && run.wire.count != 0 && defaults.size == 0 {
                            return Err(Error::Invalid(
                                "zero-size fragment sample in native MOV header",
                            ));
                        }
                        self.total_bytes = u64::from(run.wire.count)
                            .checked_mul(u64::from(defaults.size))
                            .ok_or(Error::Overflow)?;
                        self.total_duration = u64::from(run.wire.count)
                            .checked_mul(u64::from(defaults.duration))
                            .ok_or(Error::Overflow)?;
                        self.phase = Phase::Commit;
                    } else if self.sample < run.wire.count {
                        let value = run.wire.values(self.sample, defaults)?;
                        if self.reject_zero_size && value.size == 0 {
                            return Err(Error::Invalid(
                                "zero-size fragment sample in native MOV header",
                            ));
                        }
                        self.total_bytes = self
                            .total_bytes
                            .checked_add(u64::from(value.size))
                            .ok_or(Error::Overflow)?;
                        self.total_duration = self
                            .total_duration
                            .checked_add(u64::from(value.duration))
                            .ok_or(Error::Overflow)?;
                        self.sample = self.sample.checked_add(1).ok_or(Error::Overflow)?;
                    } else {
                        self.phase = Phase::Commit;
                    }
                }
                Phase::Commit => {
                    let run = self
                        .runs
                        .as_mut()
                        .ok_or(Error::Invalid("missing runs"))?
                        .as_mut_slice()
                        .get_mut(self.run)
                        .ok_or(Error::Invalid("run index"))?;
                    let byte_end = run
                        .byte
                        .checked_add(self.total_bytes)
                        .ok_or(Error::Overflow)?;
                    let decode_end = run
                        .decode
                        .map(|d| {
                            d.checked_add(i128::from(self.total_duration))
                                .ok_or(Error::Overflow)
                        })
                        .transpose()?;
                    let ordinal = run
                        .ordinal
                        .checked_add(u64::from(run.wire.count))
                        .ok_or(Error::Overflow)?;
                    run.bytes = self.total_bytes;
                    run.duration = self.total_duration;
                    self.byte = byte_end;
                    self.decode = decode_end;
                    self.ordinal = ordinal;
                    self.run = self.run.checked_add(1).ok_or(Error::Overflow)?;
                    self.phase = Phase::StartRun;
                }
                Phase::Boxes | Phase::Ready => {
                    return Err(Error::Invalid("fragment phase invariant"));
                }
            }
        }
    }
    pub fn finish(mut self) -> Result<FragmentIndex> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("fragment builder not ready"));
        }
        Ok(FragmentIndex {
            raw: self.raw,
            decode_time: self.decode_time,
            runs: self.runs.take().ok_or(Error::Invalid("missing runs"))?,
            defaults: self.defaults.ok_or(Error::Invalid("missing defaults"))?,
            first_ordinal: self.context.first_ordinal,
            count: self
                .ordinal
                .checked_sub(self.context.first_ordinal)
                .ok_or(Error::Overflow)?,
            data_end: self.byte,
            implicit_safe: self.implicit_safe,
            decode_end: self.decode,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FragmentCursor<'a> {
    index: &'a FragmentIndex,
    run: usize,
    sample: u32,
    byte: u64,
    decode: Option<i128>,
}
impl FragmentCursor<'_> {
    /// Source run ordinal for the most recently emitted fragment sample. The
    /// owner reads this after Sample, before another poll can leave that run.
    pub(crate) fn run_index(&self) -> usize {
        self.run
    }
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        loop {
            let Some(run) = self.index.runs.as_slice().get(self.run) else {
                return Ok(SampleStep::End);
            };
            if !work.try_charge(1) {
                return Ok(SampleStep::Yield);
            }
            if self.sample == run.wire.count {
                self.run = self.run.checked_add(1).ok_or(Error::Overflow)?;
                self.sample = 0;
                continue;
            }
            let byte = if self.sample == 0 {
                run.byte
            } else {
                self.byte
            };
            let decode = if self.sample == 0 {
                run.decode
            } else {
                self.decode
            };
            let values = run.wire.values(self.sample, self.index.defaults)?;
            let end = byte
                .checked_add(u64::from(values.size))
                .ok_or(Error::Overflow)?;
            let cto_ticks = values.cto.map(|value| value.ticks()).unwrap_or(0);
            let cts = decode
                .map(|d| d.checked_add(cto_ticks).ok_or(Error::Overflow))
                .transpose()?;
            let decode_end = decode
                .map(|d| {
                    d.checked_add(i128::from(values.duration))
                        .ok_or(Error::Overflow)
                })
                .transpose()?;
            let ordinal = run
                .ordinal
                .checked_add(u64::from(self.sample))
                .ok_or(Error::Overflow)?;
            let sample = Sample {
                ordinal,
                description_index: self.index.defaults.description_index,
                range: byte..end,
                dts_ticks: decode,
                cts_ticks: cts,
                duration_ticks: values.duration,
                composition_offset: values.cto,
                access: if values.flags & 0x10000 == 0 {
                    Access::ContainerSync
                } else {
                    Access::NonSync
                },
                partial_sync: false,
                dependency: None,
                fragment_flags: Some(values.flags),
            };
            let next = self.sample.checked_add(1).ok_or(Error::Overflow)?;
            self.byte = end;
            self.decode = decode_end;
            self.sample = next;
            return Ok(SampleStep::Sample(sample));
        }
    }
}

#[derive(Debug)]
pub struct FragmentLocator<'a> {
    index: &'a FragmentIndex,
    cursor: Option<FragmentCursor<'a>>,
    low: usize,
    high: usize,
    target: u64,
    done: bool,
    failed: Option<Error>,
}
impl FragmentLocator<'_> {
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        if self.done {
            return Ok(SampleStep::End);
        }
        loop {
            if self.cursor.is_none() {
                if !work.try_charge(1) {
                    return Ok(SampleStep::Yield);
                }
                if self.low < self.high {
                    let middle = self.low + (self.high - self.low) / 2;
                    let run = &self.index.runs.as_slice()[middle];
                    let end = run
                        .ordinal
                        .checked_add(u64::from(run.wire.count))
                        .ok_or(Error::Overflow)?;
                    // Empty runs have no ordinal interval and are skipped too.
                    if end <= self.target {
                        self.low = middle + 1;
                    } else {
                        self.high = middle;
                    }
                    continue;
                }
                let run = self
                    .index
                    .runs
                    .as_slice()
                    .get(self.low)
                    .ok_or(Error::Invalid("fragment locator has no indexed run"))?;
                let prefix = self
                    .target
                    .checked_sub(run.ordinal)
                    .and_then(|value| u32::try_from(value).ok())
                    .filter(|value| *value < run.wire.count)
                    .ok_or(Error::Invalid("fragment locator indexed ordinal gap"))?;
                let (sample, byte, decode) = if run.wire.constant_extents() {
                    // Already validated default extents need no per-sample walk,
                    // even for a UINT32_MAX compact run. Flags/CTO are still read
                    // at the exact target row by the ordinary source cursor.
                    let bytes = u64::from(prefix)
                        .checked_mul(u64::from(self.index.defaults.size))
                        .ok_or(Error::Overflow)?;
                    let ticks = u64::from(prefix)
                        .checked_mul(u64::from(self.index.defaults.duration))
                        .ok_or(Error::Overflow)?;
                    let byte = run.byte.checked_add(bytes).ok_or(Error::Overflow)?;
                    let decode = run
                        .decode
                        .map(|origin| origin.checked_add(i128::from(ticks)).ok_or(Error::Overflow))
                        .transpose()?;
                    (prefix, byte, decode)
                } else {
                    // Only the target run's variable prefix must be visited.
                    // The retained cursor advances on each charged source row.
                    (0, run.byte, run.decode)
                };
                self.cursor = Some(FragmentCursor {
                    index: self.index,
                    run: self.low,
                    sample,
                    byte,
                    decode,
                });
            }
            match self
                .cursor
                .as_mut()
                .ok_or(Error::Invalid("fragment locator cursor missing"))?
                .poll(work)?
            {
                SampleStep::Sample(sample) if sample.ordinal == self.target => {
                    self.done = true;
                    return Ok(SampleStep::Sample(sample));
                }
                SampleStep::Sample(_) => {}
                SampleStep::Yield => return Ok(SampleStep::Yield),
                SampleStep::End => {
                    return Err(Error::Invalid("fragment locator exhausted unexpectedly"));
                }
            }
        }
    }
}

/// An owned iterator over one immutable fragment index. Reborrows the same source
/// cursor on each poll; no self-reference, alternate parser or retained-state injection.
pub struct OwnedFragmentCursor {
    index: FragmentIndex,
    run: usize,
    sample: u32,
    byte: u64,
    decode: Option<i128>,
}
impl FragmentIndex {
    pub fn into_cursor(self) -> OwnedFragmentCursor {
        OwnedFragmentCursor {
            index: self,
            run: 0,
            sample: 0,
            byte: 0,
            decode: None,
        }
    }
}
impl OwnedFragmentCursor {
    pub(crate) fn restart(&mut self) {
        self.run = 0;
        self.sample = 0;
        self.byte = 0;
        self.decode = None;
    }
    pub(crate) fn run_index(&self) -> usize {
        self.run
    }
    pub(crate) fn delivered_run_start(&self) -> bool {
        self.sample == 1
    }
    pub(crate) fn remaining_in_delivered_run(&self) -> u32 {
        self.index.runs.as_slice()[self.run].wire.count - self.sample
    }
    pub fn index(&self) -> &FragmentIndex {
        &self.index
    }
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<SampleStep> {
        let mut cursor = FragmentCursor {
            index: &self.index,
            run: self.run,
            sample: self.sample,
            byte: self.byte,
            decode: self.decode,
        };
        let result = cursor.poll(work);
        self.run = cursor.run;
        self.sample = cursor.sample;
        self.byte = cursor.byte;
        self.decode = cursor.decode;
        result
    }
}
