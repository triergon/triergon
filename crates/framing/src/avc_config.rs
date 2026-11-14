//! ISO/IEC14496-15:2010 / incorporated2006 FRExt record grammar.
//! The private source manifest pins the primary clauses. NAL bodies, sample-entry
//! rules and record/SPS consistency are not decoded or certified here.
use crate::{BuildStep, Codec, ConfigProgress, LengthSize, NalHeader};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceId, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcConfigSource {
    pub source: SourceId,
    /// Caller-supplied configuration epoch, never inferred from NAL contents.
    pub generation: u64,
    pub offset: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcConfigLimits {
    pub maximum_record_bytes: usize,
    pub maximum_nals: usize,
    pub maximum_nal_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvcConfigArrayKind {
    Sequence,
    Picture,
    SequenceExtension,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcConfigFields {
    prefix: [u8; 6],
    extension: Option<[u8; 4]>,
}

impl AvcConfigFields {
    pub const fn raw_prefix(self) -> [u8; 6] {
        self.prefix
    }
    pub const fn version(self) -> u8 {
        self.prefix[0]
    }
    pub const fn profile_indication(self) -> u8 {
        self.prefix[1]
    }
    pub const fn profile_compatibility(self) -> u8 {
        self.prefix[2]
    }
    pub const fn level_indication(self) -> u8 {
        self.prefix[3]
    }
    pub const fn length_size_minus_one(self) -> u8 {
        self.prefix[4] & 3
    }
    pub const fn length_reserved(self) -> u8 {
        self.prefix[4] >> 2
    }
    pub const fn sequence_count_reserved(self) -> u8 {
        self.prefix[5] >> 5
    }
    /// Physical extension bytes, including all reserved fields and count.
    pub const fn raw_extension(self) -> Option<[u8; 4]> {
        self.extension
    }
    pub fn chroma_format(self) -> Option<u8> {
        self.extension.map(|v| v[0] & 3)
    }
    pub fn bit_depth_luma_minus8(self) -> Option<u8> {
        self.extension.map(|v| v[1] & 7)
    }
    pub fn bit_depth_chroma_minus8(self) -> Option<u8> {
        self.extension.map(|v| v[2] & 7)
    }
    /// Local syntax/field issues, not full decoder validity. Bits0/1: prefix
    /// reserved; bits2/3/4: extension reserved; bits5/6: depth outside0..=4.
    pub fn syntax_issues(self) -> u8 {
        let mut issues = u8::from(self.length_reserved() != 63)
            | (u8::from(self.sequence_count_reserved() != 7) << 1);
        if let Some(v) = self.extension {
            issues |= (u8::from(v[0] >> 2 != 63) << 2)
                | (u8::from(v[1] >> 3 != 31) << 3)
                | (u8::from(v[2] >> 3 != 31) << 4)
                | (u8::from(v[1] & 7 > 4) << 5)
                | (u8::from(v[2] & 7 > 4) << 6);
        }
        issues
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArrayDescriptor {
    kind: AvcConfigArrayKind,
    // Three groups carry at most 541 NALs, each with a 16-bit length.
    // Their ranges fit in 32 bits even if the record has trailing bytes.
    range: Range<u32>,
    first_nal: u16,
    nal_count: u8,
}

impl ArrayDescriptor {
    /// Includes the actual count byte and every length/payload byte. For the
    /// extension array its preceding three format/depth bytes are also included.
    fn source_range(&self) -> Range<usize> {
        self.range.start as usize..self.range.end as usize
    }
    fn nal_indices(&self) -> Range<usize> {
        let start = usize::from(self.first_nal);
        start..start + usize::from(self.nal_count)
    }
    const fn count_offset(&self) -> usize {
        let offset = match self.kind {
            AvcConfigArrayKind::Sequence | AvcConfigArrayKind::Picture => self.range.start,
            AvcConfigArrayKind::SequenceExtension => self.range.start + 3,
        };
        offset as usize
    }
}

#[derive(Clone, Copy)]
enum ScanPhase {
    Sequence,
    Picture,
    Extension,
    Nals,
    GroupEnd,
    End,
}

#[derive(Clone, Copy)]
struct Scan {
    phase: ScanPhase,
    after_group: ScanPhase,
    position: usize,
    remaining: u8,
    kind: AvcConfigArrayKind,
}

enum Event {
    Group {
        kind: AvcConfigArrayKind,
        start: usize,
        extension: Option<[u8; 4]>,
    },
    Nal {
        kind: AvcConfigArrayKind,
        payload: Range<usize>,
        header: NalHeader,
    },
    GroupEnd(usize),
    End(usize),
}

impl Scan {
    const fn new() -> Self {
        Self {
            phase: ScanPhase::Sequence,
            after_group: ScanPhase::Picture,
            position: 6,
            remaining: 0,
            kind: AvcConfigArrayKind::Sequence,
        }
    }

    // Single caller per walk; inlining lets the event stay in registers.
    #[inline(always)]
    fn next(&mut self, raw: &[u8], limits: AvcConfigLimits) -> Result<Event> {
        match self.phase {
            ScanPhase::Sequence => self.group(
                AvcConfigArrayKind::Sequence,
                5,
                raw[5] & 31,
                ScanPhase::Picture,
                None,
            ),
            ScanPhase::Picture => {
                let count_offset = self.position;
                let count = *raw.get(self.position).ok_or(Error::Truncated)?;
                self.position = self.position.checked_add(1).ok_or(Error::Overflow)?;
                self.group(
                    AvcConfigArrayKind::Picture,
                    count_offset,
                    count,
                    ScanPhase::Extension,
                    None,
                )
            }
            ScanPhase::Extension => {
                // This explicitly selects the acquired2010 edition, whose
                // branch list includes144. No newer profile list is guessed.
                if !matches!(raw[1], 100 | 110 | 122 | 144) {
                    self.phase = ScanPhase::End;
                    return Ok(Event::End(self.position));
                }
                let start = self.position;
                let end = start.checked_add(4).ok_or(Error::Overflow)?;
                let bytes: [u8; 4] = raw
                    .get(start..end)
                    .ok_or(Error::Truncated)?
                    .try_into()
                    .map_err(|_| Error::Truncated)?;
                self.position = end;
                self.group(
                    AvcConfigArrayKind::SequenceExtension,
                    start,
                    bytes[3],
                    ScanPhase::End,
                    Some(bytes),
                )
            }
            ScanPhase::Nals => {
                let start = self.position;
                let body = start.checked_add(2).ok_or(Error::Overflow)?;
                let field = raw.get(start..body).ok_or(Error::Truncated)?;
                let size = usize::from(u16::from_be_bytes([field[0], field[1]]));
                if size == 0 {
                    return Err(Error::Invalid("zero AVC record NAL length"));
                }
                check_limit(size, limits.maximum_nal_bytes)?;
                let end = body.checked_add(size).ok_or(Error::Overflow)?;
                let payload = raw.get(body..end).ok_or(Error::Truncated)?;
                let header = NalHeader::parse(Codec::Avc, payload)?;
                self.position = end;
                self.remaining -= 1;
                if self.remaining == 0 {
                    self.phase = ScanPhase::GroupEnd;
                }
                Ok(Event::Nal {
                    kind: self.kind,
                    payload: body..end,
                    header,
                })
            }
            ScanPhase::GroupEnd => {
                self.phase = self.after_group;
                Ok(Event::GroupEnd(self.position))
            }
            ScanPhase::End => Ok(Event::End(self.position)),
        }
    }

    fn group(
        &mut self,
        kind: AvcConfigArrayKind,
        start: usize,
        count: u8,
        after: ScanPhase,
        extension: Option<[u8; 4]>,
    ) -> Result<Event> {
        self.kind = kind;
        self.remaining = count;
        self.after_group = after;
        self.phase = if count == 0 {
            ScanPhase::GroupEnd
        } else {
            ScanPhase::Nals
        };
        Ok(Event::Group {
            kind,
            start,
            extension,
        })
    }
}

enum Phase {
    Prefix,
    Count,
    Reserve,
    Fill,
    Complete,
}

/// Input is one already externally framed, same-pool immutable record (without
/// box header). The first walk validates the record before reserving descriptor
/// storage. Small records reuse the validated facts if the fill fits in one step.
/// Other fills walk the record again. A work unit reads at most six fixed bytes
/// or one length and fixed NAL header. NAL bodies remain bounded spans.
/// Source delivery chunks belong to the enclosing container/core input job.
pub struct AvcConfigParser {
    raw: Option<Bytes>,
    source: AvcConfigSource,
    limits: AvcConfigLimits,
    phase: Phase,
    scan: Scan,
    fields: Option<AvcConfigFields>,
    width: Option<LengthSize>,
    arrays: [ArrayDescriptor; 3],
    array_count: usize,
    total: usize,
    // The three wire-counted arrays contain at most 541 NALs.
    filled: u16,
    understood_end: usize,
    // Taken by the published output and dropped by a latched failure, so no
    // spare pool handle or empty replacement vector is kept for either.
    nals: Option<AvcNalStorage>,
    failure: Option<Error>,
    observed_width_three: bool,
}

impl AvcConfigParser {
    pub fn new(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        if limits.maximum_record_bytes == 0 || limits.maximum_nal_bytes == 0 {
            return Err(Error::Invalid("zero AVC config limit"));
        }
        crate::record_input::check_record(pool, &raw, 1, limits.maximum_record_bytes)?;
        source
            .offset
            .checked_add(u64::try_from(raw.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok(Self {
            raw: Some(raw),
            source,
            limits,
            phase: Phase::Prefix,
            scan: Scan::new(),
            fields: None,
            width: None,
            arrays: std::array::from_fn(|_| ArrayDescriptor {
                kind: AvcConfigArrayKind::Sequence,
                range: 0..0,
                first_nal: 0,
                nal_count: 0,
            }),
            array_count: 0,
            total: 0,
            filled: 0,
            understood_end: 0,
            nals: Some(AvcNalStorage::empty()),
            failure: None,
            observed_width_three: false,
        })
    }

    // Crate-private: only the separately typed observed-record wrapper can
    // publish this result. The ordinary source constructor remains strict.
    pub(crate) fn new_observed_width(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        let mut parser = Self::new(pool, source, raw, limits)?;
        parser.observed_width_three = true;
        Ok(parser)
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AvcConfiguration>> {
        let mut ready = None;
        match self.step_into(work, cancel, &mut ready)? {
            ConfigProgress::Ready => Ok(BuildStep::Ready(
                ready.ok_or(Error::Invalid("missing published configuration"))?,
            )),
            ConfigProgress::Yield => Ok(BuildStep::Yield),
            ConfigProgress::Complete => Ok(BuildStep::Complete),
        }
    }

    /// Publishes into an empty caller-provided slot without returning the record.
    /// An occupied slot is rejected before any state, work or retained storage changes.
    /// Yield, completion and errors leave an initially empty slot empty.
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<AvcConfiguration>,
    ) -> Result<ConfigProgress> {
        self.step_into_with(work, cancel, ready, |record| record)
    }

    // Only publication is generic: the grammar walk is shared by source and
    // receiver slots, without an intermediate source-record publication slot.
    #[inline]
    pub(crate) fn step_into_with<T>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<T>,
        wrap: impl FnOnce(AvcConfiguration) -> T,
    ) -> Result<ConfigProgress> {
        if ready.is_some() {
            return Err(Error::Invalid("configuration output sink occupied"));
        }
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance(work, cancel).and_then(|progress| {
            if progress == ConfigProgress::Ready {
                *ready = Some(wrap(AvcConfiguration {
                    raw: self
                        .raw
                        .take()
                        .ok_or(Error::Invalid("missing AVC record"))?,
                    source: self.source,
                    fields: self.fields.ok_or(Error::Invalid("missing AVC fields"))?,
                    width: self.width.ok_or(Error::Invalid("missing AVC width"))?,
                    arrays: self.arrays.clone(),
                    nals: self
                        .nals
                        .take()
                        .ok_or(Error::Invalid("missing AVC descriptors"))?,
                }));
                self.phase = Phase::Complete;
            }
            Ok(progress)
        });
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.failure = Some(error);
            self.raw = None;
            self.nals = None;
        }
        result
    }

    #[inline(never)]
    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ConfigProgress> {
        // Keep the cursor local for a step and save it only when returning to
        // the caller. Each event still commits only after its work succeeds.
        let mut scan = self.scan;
        let progress = self.advance_scan(&mut scan, work, cancel);
        self.scan = scan;
        progress
    }

    #[inline(always)]
    fn advance_scan(
        &mut self,
        scan: &mut Scan,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ConfigProgress> {
        let input = self.raw.as_ref().map(|raw| (raw.as_ref(), raw.budget()));
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.phase, Phase::Complete) {
                return Ok(ConfigProgress::Complete);
            }
            if !work.try_charge(1) {
                return Ok(ConfigProgress::Yield);
            }
            let (raw, pool) = input.ok_or(Error::Invalid("missing AVC record"))?;
            match self.phase {
                Phase::Prefix => {
                    if raw[0] != 1 {
                        return Err(Error::Unsupported("AVC configuration version"));
                    }
                    let prefix: [u8; 6] = raw
                        .get(..6)
                        .ok_or(Error::Truncated)?
                        .try_into()
                        .map_err(|_| Error::Truncated)?;
                    if prefix[4] & 3 == 2 && !self.observed_width_three {
                        return Err(Error::Invalid("AVC three-byte sample length"));
                    }
                    self.width = Some(LengthSize::new((prefix[4] & 3) + 1)?);
                    self.fields = Some(AvcConfigFields {
                        prefix,
                        extension: None,
                    });
                    self.phase = Phase::Count;
                }
                Phase::Count | Phase::Fill => {
                    let mut next = *scan;
                    let event = next.next(raw, self.limits)?;
                    match event {
                        Event::Group {
                            kind,
                            start,
                            extension,
                        } if matches!(self.phase, Phase::Count) => {
                            let start = u32::try_from(start).map_err(|_| Error::Overflow)?;
                            let slot = self
                                .arrays
                                .get_mut(self.array_count)
                                .ok_or(Error::Invalid("AVC array count"))?;
                            *slot = ArrayDescriptor {
                                kind,
                                range: start..start,
                                // At most31+255+255 NALs in this grammar.
                                first_nal: self.total as u16,
                                nal_count: 0,
                            };
                            self.array_count += 1;
                            if let Some(extension) = extension {
                                self.fields
                                    .as_mut()
                                    .ok_or(Error::Invalid("missing AVC fields"))?
                                    .extension = Some(extension);
                            }
                        }
                        Event::GroupEnd(end) if matches!(self.phase, Phase::Count) => {
                            let slot = &mut self.arrays[self.array_count - 1];
                            slot.range.end = u32::try_from(end).map_err(|_| Error::Overflow)?;
                            // Each group consumed its literal count (at most255).
                            slot.nal_count = (self.total - usize::from(slot.first_nal)) as u8;
                        }
                        Event::Nal {
                            kind,
                            payload,
                            header,
                        } => {
                            if matches!(self.phase, Phase::Count) {
                                let count = self.total.checked_add(1).ok_or(Error::Overflow)?;
                                check_limit(count, self.limits.maximum_nals)?;
                                if let Some(AvcNalStorage::Inline(data)) = &mut self.nals
                                    && let Some(slot) = data.get_mut(self.total)
                                {
                                    *slot = AvcNalDescriptor::new(&payload, kind, header);
                                }
                                self.total = count;
                            } else {
                                let nal = AvcNalDescriptor::new(&payload, kind, header);
                                self.nals
                                    .as_mut()
                                    .ok_or(Error::Invalid("missing AVC descriptors"))?
                                    .push_at(usize::from(self.filled), nal)?;
                                self.filled += 1;
                            }
                        }
                        Event::End(end) => {
                            if matches!(self.phase, Phase::Count) {
                                self.understood_end = end;
                                self.phase = Phase::Reserve;
                            } else {
                                return self.publication_ready(end);
                            }
                        }
                        _ => {}
                    }
                    *scan = next;
                }
                Phase::Reserve => {
                    let nals = self
                        .nals
                        .as_mut()
                        .ok_or(Error::Invalid("missing AVC descriptors"))?;
                    nals.reserve(pool, self.total)?;

                    // A complete small record can reuse the first walk's facts.
                    // Charge the group, NAL and end events of the usual fill.
                    let fill_work = (2 * self.array_count + self.total + 1) as u64;
                    if work.remaining() >= fill_work && self.total <= INLINE_NALS {
                        self.filled = self.total as u16;
                        work.take(fill_work);
                        if cancel.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        return self.publication_ready(self.understood_end);
                    }
                    *scan = Scan::new();
                    self.phase = Phase::Fill;
                }
                Phase::Complete => return Ok(ConfigProgress::Complete),
            }
        }
    }

    fn publication_ready(&self, end: usize) -> Result<ConfigProgress> {
        if end != self.understood_end || usize::from(self.filled) != self.total {
            return Err(Error::Invalid("AVC second walk"));
        }
        Ok(ConfigProgress::Ready)
    }
}

fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct AvcNalDescriptor {
    start: usize,
    length: u16,
    kind: AvcConfigArrayKind,
    header: NalHeader,
}
impl AvcNalDescriptor {
    const EMPTY: Self = Self {
        start: 0,
        length: 0,
        kind: AvcConfigArrayKind::Sequence,
        header: NalHeader::Avc {
            nal_ref_idc: 0,
            nal_unit_type: 0,
        },
    };

    fn new(payload: &Range<usize>, kind: AvcConfigArrayKind, header: NalHeader) -> Self {
        // The scan checked the end using the original two-byte length.
        let length = payload.len();
        debug_assert!(u16::try_from(length).is_ok());
        Self {
            start: payload.start,
            length: length as u16,
            kind,
            header,
        }
    }

    fn payload(&self) -> Range<usize> {
        self.start..self.start + usize::from(self.length)
    }

    fn length_field(&self) -> Range<usize> {
        // The parser checks the two-byte prefix immediately before the payload.
        self.start - 2..self.start
    }
}

const INLINE_NALS: usize = 4;

// The parser already needs these slots while validating a small record. Move
// them into the result instead of allocating and copying another descriptor table.
#[derive(Debug)]
enum AvcNalStorage {
    Inline([AvcNalDescriptor; INLINE_NALS]),
    Heap(BudgetVec<AvcNalDescriptor>),
}

impl AvcNalStorage {
    const fn empty() -> Self {
        Self::Inline([AvcNalDescriptor::EMPTY; INLINE_NALS])
    }

    fn reserve(&mut self, pool: &Budget, count: usize) -> Result<()> {
        if count > INLINE_NALS {
            if !matches!(self, Self::Heap(_)) {
                *self = Self::Heap(BudgetVec::new(pool));
            }
            if let Self::Heap(values) = self {
                values.try_reserve_exact(count)?;
            }
        }
        Ok(())
    }

    fn push_at(&mut self, index: usize, value: AvcNalDescriptor) -> Result<()> {
        match self {
            Self::Inline(data) => {
                *data
                    .get_mut(index)
                    .ok_or(Error::Invalid("AVC inline descriptor count"))? = value;
                Ok(())
            }
            Self::Heap(values) => values.try_push(value).map_err(|e| e.error),
        }
    }

    fn as_slice(&self, count: usize) -> &[AvcNalDescriptor] {
        match self {
            Self::Inline(data) => &data[..count],
            Self::Heap(values) => values.as_slice(),
        }
    }

    fn get(&self, index: usize) -> Option<&AvcNalDescriptor> {
        match self {
            // Scan refuses zero-length configuration NALs, so empty slots
            // identify the end without another stored length.
            Self::Inline(data) => data.get(index).filter(|nal| nal.length != 0),
            Self::Heap(values) => values.as_slice().get(index),
        }
    }

    fn heap_len(&self) -> usize {
        match self {
            Self::Inline(_) => 0,
            Self::Heap(values) => values.len(),
        }
    }

    fn heap_capacity(&self) -> usize {
        match self {
            Self::Inline(_) => 0,
            Self::Heap(values) => values.capacity(),
        }
    }
}

/// Complete AVC configuration facts with one raw record handle and charged
/// heap descriptors beyond four inline slots. Borrowed inspection creates no
/// per-NAL owning byte aliases.
#[derive(Debug)]
pub struct AvcConfiguration {
    raw: Bytes,
    source: AvcConfigSource,
    fields: AvcConfigFields,
    width: LengthSize,
    arrays: [ArrayDescriptor; 3],
    nals: AvcNalStorage,
}
impl AvcConfiguration {
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub const fn source(&self) -> AvcConfigSource {
        self.source
    }
    pub const fn fields(&self) -> AvcConfigFields {
        self.fields
    }
    pub const fn sample_length_size(&self) -> LengthSize {
        self.width
    }
    pub const fn understood_end(&self) -> usize {
        self.arrays[self.array_count() - 1].range.end as usize
    }
    pub fn nal_count(&self) -> usize {
        let last = &self.arrays[self.array_count() - 1];
        usize::from(last.first_nal) + usize::from(last.nal_count)
    }
    pub const fn array_count(&self) -> usize {
        if self.fields.extension.is_some() {
            3
        } else {
            2
        }
    }
    pub fn nal(&self, index: usize) -> Option<AvcConfigNal<'_>> {
        self.nals.get(index).map(|descriptor| AvcConfigNal {
            raw: &self.raw,
            source: self.source,
            descriptor,
        })
    }
    pub fn nals(
        &self,
    ) -> impl ExactSizeIterator<Item = AvcConfigNal<'_>> + DoubleEndedIterator + '_ {
        self.nals
            .as_slice(self.nal_count())
            .iter()
            .map(move |descriptor| AvcConfigNal {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    pub fn array(&self, index: usize) -> Option<AvcConfigArray<'_>> {
        self.arrays[..self.array_count()]
            .get(index)
            .map(|descriptor| AvcConfigArray {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    pub fn arrays(
        &self,
    ) -> impl ExactSizeIterator<Item = AvcConfigArray<'_>> + DoubleEndedIterator + '_ {
        self.arrays[..self.array_count()]
            .iter()
            .map(move |descriptor| AvcConfigArray {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    pub fn trailing_bytes(&self) -> &[u8] {
        &self.raw[self.understood_end()..]
    }
    pub fn retain_trailing_bytes(&self) -> Result<Bytes> {
        self.raw.slice(self.understood_end()..self.raw.len())
    }
    /// Element-count lower bound for live descriptor storage (capacity may be
    /// larger).
    pub(crate) fn descriptor_lower_bound(&self) -> Result<usize> {
        self.nals
            .heap_len()
            .checked_mul(size_of::<AvcNalDescriptor>())
            .ok_or(Error::Overflow)
    }
    /// Heap descriptor capacity only; arrays and up to four NALs are inline.
    pub fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.nals
            .heap_capacity()
            .checked_mul(size_of::<AvcNalDescriptor>())
            .ok_or(Error::Overflow)
    }
    /// Full hidden raw backing plus actual descriptor heap capacities.
    pub fn retained_storage_bytes(&self) -> Result<usize> {
        self.descriptor_storage_bytes()?
            .checked_add(self.raw.retained_storage_bytes())
            .ok_or(Error::Overflow)
    }
}

/// A NAL borrow cannot escape its configuration; retaining it is explicit.
///
/// ```compile_fail,E0505
/// use triergon_framing::AvcConfiguration;
/// fn escape(view: AvcConfiguration) {
///     let leaf = view.nal(0).unwrap();
///     drop(view);
///     let _ = leaf.source_bytes();
/// }
/// ```
///
/// Borrowed bytes carry the configuration's lifetime, not the NAL borrow's:
///
/// ```compile_fail,E0505
/// use triergon_framing::AvcConfiguration;
/// fn escape(view: AvcConfiguration) -> usize {
///     let bytes = view.nal(0).unwrap().length_field_bytes();
///     drop(view);
///     bytes.len()
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct AvcConfigNal<'a> {
    raw: &'a Bytes,
    source: AvcConfigSource,
    descriptor: &'a AvcNalDescriptor,
}
impl<'a> AvcConfigNal<'a> {
    pub fn source_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.payload()]
    }
    pub fn length_field_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.length_field()]
    }
    pub fn source_range(&self) -> Range<usize> {
        self.descriptor.payload()
    }
    pub const fn source(&self) -> AvcConfigSource {
        self.source
    }
    pub const fn array_kind(&self) -> AvcConfigArrayKind {
        self.descriptor.kind
    }
    pub const fn header(&self) -> NalHeader {
        self.descriptor.header
    }
}

/// An array borrow is tied to the configuration in the same way.
///
/// ```compile_fail,E0505
/// use triergon_framing::AvcConfiguration;
/// fn escape(view: AvcConfiguration) -> usize {
///     let array = view.array(0).unwrap();
///     drop(view);
///     array.source_bytes().len()
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct AvcConfigArray<'a> {
    raw: &'a Bytes,
    source: AvcConfigSource,
    descriptor: &'a ArrayDescriptor,
}
impl<'a> AvcConfigArray<'a> {
    pub fn source_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.source_range()]
    }
    pub const fn source(&self) -> AvcConfigSource {
        self.source
    }
    pub const fn kind(&self) -> AvcConfigArrayKind {
        self.descriptor.kind
    }
    pub fn source_range(&self) -> Range<usize> {
        self.descriptor.source_range()
    }
    pub fn nal_indices(&self) -> Range<usize> {
        self.descriptor.nal_indices()
    }
    pub const fn count_offset(&self) -> usize {
        self.descriptor.count_offset()
    }
}

#[cfg(test)]
mod descriptor_guards {
    use super::*;

    #[test]
    fn descriptors_hold_no_owner() {
        // No drop glue proves the NAL and array descriptors hold no Bytes owner.
        assert!(!std::mem::needs_drop::<AvcNalDescriptor>());
        assert!(!std::mem::needs_drop::<ArrayDescriptor>());
    }

    #[test]
    fn descriptors_keep_full_width_offsets() {
        let header = NalHeader::parse(Codec::Avc, &[0x67]).unwrap();
        let high = usize::MAX - 3;
        let descriptor =
            AvcNalDescriptor::new(&(high..usize::MAX), AvcConfigArrayKind::Sequence, header);
        // No physical check is repeated after Scan; the extents are copied.
        assert_eq!(descriptor.payload(), high..usize::MAX);
        assert_eq!(descriptor.length_field(), high - 2..high);
    }
}
