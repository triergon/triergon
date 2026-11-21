//! Track fragments. [`Trex`] holds a track's fragment defaults, [`FragmentBuilder`] indexes
//! one `traf` into a [`FragmentIndex`] without reading media, and [`FragmentCursor`],
//! [`OwnedFragmentCursor`] and [`FragmentLocator`] read its samples back. Signed `trun`
//! version 1 composition offsets follow draft text and are read experimentally.
//!
//! Specification: Adobe F4V 10.1 §§2.9, 2.12; ETSI TS 126 244 V18.0.0 §13.5 (`tfdt`);
//! ISO/IEC 14496-12:2008/FDAmd2 §8.8.7.1 (moof-relative base); FDAM3 §8.8.8.2 and MPEG
//! m76694 Table 9 (signed `trun`).
use crate::{
    Access, BuildStatus, Limits, Next, Sample,
    boxes::{BoxChildren, BoxView},
    fields, policy,
    sample::SampleSpan,
};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

mod decode_time;
pub use decode_time::FragmentDecodeTime;

/// A track's fragment defaults, from its `trex` box in `mvex`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trex {
    /// `track_ID` the defaults apply to; never zero.
    pub track_id: u32,
    /// Default 1-based sample description index.
    pub description_index: u32,
    /// Default sample duration in ticks of the media timescale.
    pub duration: u32,
    /// Default sample size in bytes.
    pub size: u32,
    /// Default `sample_flags` word.
    pub flags: u32,
}
impl Trex {
    /// Reads a `trex` box. `Error::Invalid` for another box type, a zero track ID or
    /// trailing bytes; `Error::Unsupported` for nonzero version or flags.
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

/// Where the data of a `traf` starts when it states neither a base data offset nor the
/// moof-relative flag.
#[derive(Clone, Copy, Debug)]
pub enum ImplicitBase {
    /// The first `traf` of its `moof`: at the `moof`'s first byte.
    FirstTraf,
    /// A later `traf`: at this absolute offset, the end of the preceding `traf`'s data in
    /// the same `moof` ([`FragmentIndex::implicit_data_end`]).
    PreviousTraf(u64),
}
/// The place of a `traf` in its file and track, for [`FragmentBuilder`].
#[derive(Clone, Copy, Debug)]
pub struct FragmentContext {
    /// Absolute file offset of the enclosing `moof`'s first byte.
    pub moof_start: u64,
    /// Where the data starts if the `traf` does not say.
    pub implicit_base: ImplicitBase,
    /// Decode time of the first sample in media ticks, if the caller knows it, for a
    /// `traf` without `tfdt`. With `tfdt` present a supplied value must match it; without
    /// either, sample times are unknown, not zero.
    pub decode_origin: Option<i128>,
    /// 1-based track ordinal of the fragment's first sample; never zero.
    pub first_ordinal: u64,
    /// Number of entries in the track's `stsd`, which the fragment's description index
    /// must not exceed.
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
        let at = usize::try_from(index)
            .map_err(|_| Error::Overflow)?
            .checked_mul(self.stride)
            .and_then(|v| v.checked_add(self.start))
            .ok_or(Error::Overflow)?;
        // The row's present fields in their wire order: duration, size, flags,
        // composition offset.
        let row = fields::slice(self.raw.payload(), at, self.stride)?;
        let mut words = row
            .chunks_exact(4)
            .map(|word| u32::from_be_bytes([word[0], word[1], word[2], word[3]]));
        let mut read = |bit: u32, fallback: u32| {
            if self.fields & bit == 0 {
                fallback
            } else {
                words.next().unwrap_or(fallback)
            }
        };
        let duration = read(0x100, defaults.duration);
        let size = read(0x200, defaults.size);
        let flags = read(
            0x400,
            if index == 0 {
                self.first_flags.unwrap_or(defaults.flags)
            } else {
                defaults.flags
            },
        );
        let raw_cto = read(0x800, 0);
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

/// One `traf` indexed by run, from [`FragmentBuilder`]: its defaults, and where each run's
/// data starts and its decode time. Nothing is stored per sample.
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
    /// The `trun` flags of 0-based run `run`.
    pub(crate) fn run_flags(&self, run: usize) -> Result<u32> {
        self.runs
            .as_slice()
            .get(run)
            .map(|r| r.wire.fields)
            .ok_or(Error::Invalid("fragment source run absent"))
    }
    /// The `traf` box as read.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// The `tfdt` box, if the `traf` has one.
    pub fn decode_time(&self) -> Option<&FragmentDecodeTime> {
        self.decode_time.as_ref()
    }
    /// Number of samples across all runs.
    pub fn sample_count(&self) -> u64 {
        self.count
    }
    /// 1-based track ordinal of the first sample.
    pub(crate) fn first_ordinal(&self) -> u64 {
        self.first_ordinal
    }
    /// `track_ID` from `tfhd`.
    pub fn track_id(&self) -> u32 {
        self.defaults.track_id
    }
    /// 1-based sample description index all the fragment's samples use.
    pub fn description_index(&self) -> u32 {
        self.defaults.description_index
    }
    /// Absolute file offset just past the data of the last run.
    pub fn data_end(&self) -> u64 {
        self.data_end
    }
    /// [`data_end`](Self::data_end) as the implicit base of the next `traf` in the same
    /// `moof`. `Error::Unsupported` when the fragment has no samples, an empty run, or a
    /// run that starts before the previous one ends, since that end is then ambiguous.
    pub fn implicit_data_end(&self) -> Result<u64> {
        if !self.implicit_safe || self.count == 0 {
            return Err(Error::Unsupported(
                "implicit predecessor with empty/backward/overlapping runs",
            ));
        }
        Ok(self.data_end)
    }
    /// Decode time just after the last sample, in media ticks; `None` when the fragment's
    /// times are unknown.
    pub fn decode_end(&self) -> Option<i128> {
        self.decode_end
    }
    /// A cursor over the samples in run order.
    pub fn cursor(&self) -> FragmentCursor<'_> {
        FragmentCursor {
            index: self,
            run: 0,
            sample: 0,
            byte: 0,
            decode: None,
        }
    }
    /// A locator for the sample with 1-based track `ordinal`; `Error::Invalid` outside
    /// this fragment.
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
            passed_sync: None,
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

/// Indexes one `traf` into a [`FragmentIndex`] under a work budget, reading no media.
/// Poll until [`BuildStatus::Ready`], then call [`finish`](Self::finish). A failure other
/// than backpressure is kept for later polls.
#[derive(Debug)]
pub struct FragmentBuilder {
    raw: BoxView,
    boxes: BoxChildren,
    runs: BudgetVec<IndexedRun>,
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
    /// A builder for `raw`, a `traf` stored in `budget`, taking from `trex` the defaults
    /// `tfhd` does not override. `Error::Invalid` for another box type, a box from another
    /// budget pool or a zero first ordinal.
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
            runs: BudgetVec::new(&budget),
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
    /// [`new`](Self::new), also refusing a zero-size sample, as FFmpeg's MOV demuxer
    /// does in the fragments it reads while opening a file.
    pub(crate) fn new_header_open(
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
    /// The `traf` box being indexed.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// One bounded step. `Error::Invalid` for structural faults such as a missing or
    /// duplicate `tfhd`, a misplaced `tfdt` or a description index outside the track;
    /// `Error::Unsupported` for unknown `tfhd`, `tfdt` or `trun` versions or flags.
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
                    // Most fragments hold one run or a few: each of the first
                    // four takes exactly its own slot, where a push would take
                    // four at once. Longer lists grow geometrically.
                    if self.runs.len() < 4
                        && let Err(error) = self.runs.try_reserve_exact(1)
                    {
                        self.pending = Some(value);
                        return Err(error);
                    }
                    if let Err(error) = self.runs.try_push(value) {
                        self.pending = Some(error.value);
                        return Err(error.error);
                    }
                    continue;
                }
                match self.boxes.poll(work)? {
                    Next::Yield => return Ok(BuildStatus::Yield),
                    Next::End => {
                        self.phase = Phase::Initialize;
                    }
                    Next::Item(raw) => match &raw.header().kind {
                        b"tfhd" => {
                            if self.header.is_some() {
                                return Err(Error::Invalid("duplicate tfhd"));
                            }
                            self.header = Some(Tfhd::parse(&raw)?);
                        }
                        b"trun" => {
                            policy(
                                u32::try_from(self.runs.len())
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
                            if self.header.is_none() || !self.runs.is_empty() {
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
            let length = self.runs.len();
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
                        // One unit per sample: the first was charged above.
                        loop {
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
                            if self.sample == run.wire.count || !work.try_charge(1) {
                                break;
                            }
                        }
                    } else {
                        self.phase = Phase::Commit;
                    }
                }
                Phase::Commit => {
                    let run = self
                        .runs
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
    /// The index; the error that stopped the build, or `Error::Invalid` before
    /// [`BuildStatus::Ready`].
    pub fn finish(self) -> Result<FragmentIndex> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("fragment builder not ready"));
        }
        Ok(FragmentIndex {
            raw: self.raw,
            decode_time: self.decode_time,
            runs: self.runs,
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

/// Where a walk over a fragment index's sample ranges stands.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RunPosition {
    run: usize,
    sample: u32,
    byte: u64,
}

impl FragmentIndex {
    /// The ranges [`FragmentCursor`] reports from `position` on, with the same
    /// errors: the next at most `maximum` samples of one run, charging a work
    /// unit for each and for each run passed. A span stops before a sample whose
    /// end would overflow, and the next step reports that sample's error.
    pub(crate) fn next_span(
        &self,
        position: &mut RunPosition,
        maximum: u64,
        work: &mut WorkBudget,
    ) -> Result<Next<SampleSpan>> {
        loop {
            let Some(run) = self.runs.as_slice().get(position.run) else {
                return Ok(Next::End);
            };
            if !work.try_charge(1) {
                return Ok(Next::Yield);
            }
            if position.sample == run.wire.count {
                position.run = position.run.checked_add(1).ok_or(Error::Overflow)?;
                position.sample = 0;
                continue;
            }
            let start = if position.sample == 0 {
                run.byte
            } else {
                position.byte
            };
            let wanted = u64::from(run.wire.count - position.sample)
                .min(maximum.saturating_sub(1).min(work.remaining()) + 1);
            let (mut end, mut count, mut offset_minimum) = (start, 0u32, None::<i32>);
            while u64::from(count) < wanted {
                let values = run.wire.values(position.sample + count, self.defaults)?;
                match end.checked_add(u64::from(values.size)) {
                    Some(next) => end = next,
                    None if count == 0 => return Err(Error::Overflow),
                    None => break,
                }
                if let Some(offset) = values.cto {
                    let signed = offset.raw_bits().cast_signed();
                    offset_minimum = Some(offset_minimum.map_or(signed, |least| least.min(signed)));
                }
                count += 1;
            }
            work.take(u64::from(count) - 1);
            let first = run
                .ordinal
                .checked_add(u64::from(position.sample))
                .ok_or(Error::Overflow)?;
            position.sample += count;
            position.byte = end;
            return Ok(Next::Item(SampleSpan {
                first,
                count: u64::from(count),
                range: start..end,
                fragment: true,
                offset_minimum,
            }));
        }
    }
}

/// Reads the samples of a [`FragmentIndex`] in run order; from [`FragmentIndex::cursor`].
#[derive(Clone, Copy, Debug)]
pub struct FragmentCursor<'a> {
    index: &'a FragmentIndex,
    run: usize,
    sample: u32,
    byte: u64,
    decode: Option<i128>,
}
impl FragmentCursor<'_> {
    /// 0-based run of the sample the last poll returned, until the next poll.
    pub(crate) fn run_index(&self) -> usize {
        self.run
    }
    /// The next sample, for one work unit per sample and per run passed.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        let Advance::Sample(next) = self.advance(work)? else {
            return Ok(Next::Yield);
        };
        let Some(next) = next else {
            return Ok(Next::End);
        };
        let values = next.values;
        let cto_ticks = values.cto.map(|value| value.ticks()).unwrap_or(0);
        let cts = next
            .decode
            .map(|d| d.checked_add(cto_ticks).ok_or(Error::Overflow))
            .transpose()?;
        let sample = Sample {
            ordinal: next.ordinal,
            description_index: self.index.defaults.description_index,
            range: next.start..next.end,
            dts_ticks: next.decode,
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
        self.commit(&next);
        Ok(Next::Item(sample))
    }
    /// Steps past the next sample without building it, as [`poll`](Self::poll)
    /// does.
    pub(crate) fn skip(&mut self, work: &mut WorkBudget) -> Result<RunSkip> {
        let Advance::Sample(next) = self.advance(work)? else {
            return Ok(RunSkip::Yield);
        };
        let Some(next) = next else {
            return Ok(RunSkip::End);
        };
        self.commit(&next);
        Ok(RunSkip::Sample(SkippedRunSample {
            ordinal: next.ordinal,
            dts: next.decode,
            duration: next.values.duration,
            start: next.start,
            size: next.values.size,
            sync: next.values.flags & 0x10000 == 0,
        }))
    }
    /// Finds the next sample, passing exhausted runs, without moving past it.
    fn advance(&mut self, work: &mut WorkBudget) -> Result<Advance> {
        loop {
            let Some(run) = self.index.runs.as_slice().get(self.run) else {
                return Ok(Advance::Sample(None));
            };
            if !work.try_charge(1) {
                return Ok(Advance::Yield);
            }
            if self.sample == run.wire.count {
                self.run = self.run.checked_add(1).ok_or(Error::Overflow)?;
                self.sample = 0;
                continue;
            }
            let start = if self.sample == 0 {
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
            let end = start
                .checked_add(u64::from(values.size))
                .ok_or(Error::Overflow)?;
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
            let following = self.sample.checked_add(1).ok_or(Error::Overflow)?;
            return Ok(Advance::Sample(Some(NextSample {
                ordinal,
                start,
                end,
                decode,
                decode_end,
                following,
                values,
            })));
        }
    }
    fn commit(&mut self, next: &NextSample) {
        self.byte = next.end;
        self.decode = next.decode_end;
        self.sample = next.following;
    }
}

/// The sample a [`FragmentCursor`] produces next.
struct NextSample {
    ordinal: u64,
    start: u64,
    end: u64,
    decode: Option<i128>,
    decode_end: Option<i128>,
    following: u32,
    values: Values,
}

enum Advance {
    Sample(Option<NextSample>),
    Yield,
}

/// One step of [`FragmentCursor::skip`].
#[derive(Debug)]
pub(crate) enum RunSkip {
    Sample(SkippedRunSample),
    Yield,
    End,
}

/// A fragment sample [`FragmentCursor::skip`] stepped past.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SkippedRunSample {
    pub(crate) ordinal: u64,
    pub(crate) dts: Option<i128>,
    pub(crate) duration: u32,
    pub(crate) start: u64,
    pub(crate) size: u32,
    pub(crate) sync: bool,
}

/// Finds one sample of a [`FragmentIndex`] by ordinal without visiting the runs before
/// its own; from [`FragmentIndex::locate`].
#[derive(Debug)]
pub struct FragmentLocator<'a> {
    index: &'a FragmentIndex,
    cursor: Option<FragmentCursor<'a>>,
    low: usize,
    high: usize,
    target: u64,
    /// The last sync sample stepped past on the way to the target.
    passed_sync: Option<u64>,
    done: bool,
    failed: Option<Error>,
}
impl FragmentLocator<'_> {
    /// After the target, locate the sample that follows it in this index, by
    /// continuing from it. `false` where that sample is not `ordinal` or the
    /// index has no more.
    pub(crate) fn continue_to(&mut self, ordinal: u64) -> bool {
        let end = self.index.first_ordinal.saturating_add(self.index.count);
        if !self.done
            || self.failed.is_some()
            || self.cursor.is_none()
            || self.target.checked_add(1) != Some(ordinal)
            || ordinal >= end
        {
            return false;
        }
        self.target = ordinal;
        self.passed_sync = None;
        self.done = false;
        true
    }
    /// The last sync sample before the target in the target's run that locating
    /// it stepped past, if it stepped past any.
    pub(crate) fn passed_sync(&self) -> Option<u64> {
        self.passed_sync
    }
    /// The located sample once, then [`Next::End`]. A failure is kept for later polls.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
        if self.done {
            return Ok(Next::End);
        }
        loop {
            if self.cursor.is_none() {
                if !work.try_charge(1) {
                    return Ok(Next::Yield);
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
            let cursor = self
                .cursor
                .as_mut()
                .ok_or(Error::Invalid("fragment locator cursor missing"))?;
            // The samples before the target need only stepping past.
            if self
                .index
                .runs
                .as_slice()
                .get(cursor.run)
                .is_some_and(|run| {
                    cursor.sample < run.wire.count
                        && run
                            .ordinal
                            .checked_add(u64::from(cursor.sample))
                            .is_some_and(|next| next < self.target)
                })
            {
                match cursor.skip(work)? {
                    RunSkip::Sample(sample) => {
                        if sample.sync {
                            self.passed_sync = Some(sample.ordinal);
                        }
                        continue;
                    }
                    RunSkip::Yield => return Ok(Next::Yield),
                    RunSkip::End => {
                        return Err(Error::Invalid("fragment locator exhausted unexpectedly"));
                    }
                }
            }
            match cursor.poll(work)? {
                Next::Item(sample) if sample.ordinal == self.target => {
                    self.done = true;
                    return Ok(Next::Item(sample));
                }
                Next::Item(_) => {}
                Next::Yield => return Ok(Next::Yield),
                Next::End => {
                    return Err(Error::Invalid("fragment locator exhausted unexpectedly"));
                }
            }
        }
    }
}

/// A [`FragmentCursor`] that owns its index, so it can be kept without a borrow; from
/// [`FragmentIndex::into_cursor`].
#[derive(Debug)]
pub struct OwnedFragmentCursor {
    index: FragmentIndex,
    run: usize,
    sample: u32,
    byte: u64,
    decode: Option<i128>,
}
impl FragmentIndex {
    /// A cursor over the samples that takes the index with it.
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
    /// The index being read.
    pub fn index(&self) -> &FragmentIndex {
        &self.index
    }
    /// The next sample, as [`FragmentCursor::poll`] reports it.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<Sample>> {
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

/// A [`FragmentCursor`]'s place in its index, kept without borrowing the index so
/// an owner can hold both.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FragmentCursorPosition {
    run: usize,
    sample: u32,
    byte: u64,
    decode: Option<i128>,
}
impl FragmentIndex {
    /// The cursor continuing from `position`, which must come from this index.
    pub(crate) fn resume(&self, position: FragmentCursorPosition) -> FragmentCursor<'_> {
        FragmentCursor {
            index: self,
            run: position.run,
            sample: position.sample,
            byte: position.byte,
            decode: position.decode,
        }
    }
}
impl FragmentCursor<'_> {
    pub(crate) fn position(&self) -> FragmentCursorPosition {
        FragmentCursorPosition {
            run: self.run,
            sample: self.sample,
            byte: self.byte,
            decode: self.decode,
        }
    }
}

fn add_offset(base: u64, delta: i32) -> Result<u64> {
    let end = i128::from(base)
        .checked_add(i128::from(delta))
        .ok_or(Error::Overflow)?;
    u64::try_from(end).map_err(|_| Error::Overflow)
}
