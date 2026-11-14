//! Version1 hvcC source-wire facts.
//! Raw syntax, fixed NAL headers and bounded extents; no RBSP/decoder certificate.
use crate::{BuildStep, Codec, ConfigProgress, LengthSize, NalHeader};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceId, WorkBudget};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvcConfigSource {
    pub source: SourceId,
    pub generation: u64,
    pub offset: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvcConfigLimits {
    pub maximum_record_bytes: usize,
    pub maximum_arrays: usize,
    pub maximum_nals: usize,
    pub maximum_nal_bytes: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvcConfigFields {
    prefix: [u8; 23],
}
impl HvcConfigFields {
    pub const fn raw_prefix(self) -> [u8; 23] {
        self.prefix
    }
    pub const fn version(self) -> u8 {
        self.prefix[0]
    }
    pub const fn profile_space(self) -> u8 {
        self.prefix[1] >> 6
    }
    pub const fn tier_flag(self) -> bool {
        self.prefix[1] & 32 != 0
    }
    pub const fn profile_idc(self) -> u8 {
        self.prefix[1] & 31
    }
    pub const fn profile_compatibility_flags(self) -> u32 {
        u32::from_be_bytes([
            self.prefix[2],
            self.prefix[3],
            self.prefix[4],
            self.prefix[5],
        ])
    }
    pub const fn constraint_indicator_flags(self) -> u64 {
        u64::from_be_bytes([
            0,
            0,
            self.prefix[6],
            self.prefix[7],
            self.prefix[8],
            self.prefix[9],
            self.prefix[10],
            self.prefix[11],
        ])
    }
    pub const fn level_idc(self) -> u8 {
        self.prefix[12]
    }
    pub const fn min_spatial_segmentation_idc(self) -> u16 {
        u16::from_be_bytes([self.prefix[13] & 15, self.prefix[14]])
    }
    pub const fn parallelism_type(self) -> u8 {
        self.prefix[15] & 3
    }
    pub const fn chroma_format_idc(self) -> u8 {
        self.prefix[16] & 3
    }
    pub const fn bit_depth_luma_minus8(self) -> u8 {
        self.prefix[17] & 7
    }
    pub const fn bit_depth_chroma_minus8(self) -> u8 {
        self.prefix[18] & 7
    }
    pub const fn average_frame_rate(self) -> u16 {
        u16::from_be_bytes([self.prefix[19], self.prefix[20]])
    }
    pub const fn constant_frame_rate(self) -> u8 {
        self.prefix[21] >> 6
    }
    pub const fn num_temporal_layers(self) -> u8 {
        (self.prefix[21] >> 3) & 7
    }
    pub const fn temporal_id_nested(self) -> bool {
        self.prefix[21] & 4 != 0
    }
    pub const fn length_size_minus_one(self) -> u8 {
        self.prefix[21] & 3
    }
    pub const fn array_count(self) -> u8 {
        self.prefix[22]
    }
    /// Bits0..4 flag actual reserved-prefix patterns; bit5 flags width3.
    pub fn syntax_issues(self) -> u32 {
        u32::from(self.prefix[13] >> 4 != 15)
            | u32::from(self.prefix[15] >> 2 != 63) << 1
            | u32::from(self.prefix[16] >> 2 != 63) << 2
            | u32::from(self.prefix[17] >> 3 != 31) << 3
            | u32::from(self.prefix[18] >> 3 != 31) << 4
            | u32::from(self.length_size_minus_one() == 2) << 5
    }
}
pub fn inspect_hvc_config_prefix(raw: &[u8]) -> Result<HvcConfigFields> {
    Ok(HvcConfigFields {
        prefix: raw
            .get(..23)
            .ok_or(Error::Truncated)?
            .try_into()
            .map_err(|_| Error::Truncated)?,
    })
}
#[derive(Clone, Copy)]
enum ScanPhase {
    Array,
    Nal,
    ArrayEnd,
}

#[derive(Clone, Copy)]
struct Scan {
    phase: ScanPhase,
    position: usize,
    index: u8,
    array_count: u8,
    remaining: u16,
    declared_type: u8,
}

enum Event {
    Array {
        header: Range<usize>,
        count: usize,
    },
    Nal {
        length: Range<usize>,
        payload: Range<usize>,
        header: NalHeader,
    },
    ArrayEnd(usize),
    End(usize),
}

impl Scan {
    const fn new(array_count: u8) -> Self {
        Self {
            phase: ScanPhase::Array,
            position: 23,
            index: 0,
            array_count,
            remaining: 0,
            declared_type: 0,
        }
    }

    // One event examines at most three array bytes or two length bytes plus
    // the two fixed HEVC NAL-header bytes. Opaque bodies are checked spans.
    // Single caller per walk; inlining lets the event stay in registers.
    #[inline(always)]
    fn next(&mut self, raw: &[u8], limits: HvcConfigLimits) -> Result<Event> {
        match self.phase {
            ScanPhase::Array => {
                if self.index == self.array_count {
                    return Ok(Event::End(self.position));
                }
                let start = self.position;
                let end = start.checked_add(3).ok_or(Error::Overflow)?;
                let header = raw.get(start..end).ok_or(Error::Truncated)?;
                let count = u16::from_be_bytes([header[1], header[2]]);
                self.position = end;
                self.remaining = count;
                self.declared_type = header[0] & 63;
                self.phase = if count == 0 {
                    ScanPhase::ArrayEnd
                } else {
                    ScanPhase::Nal
                };
                Ok(Event::Array {
                    header: start..end,
                    count: usize::from(count),
                })
            }
            ScanPhase::Nal => {
                let start = self.position;
                let body = start.checked_add(2).ok_or(Error::Overflow)?;
                let length = raw.get(start..body).ok_or(Error::Truncated)?;
                let size = usize::from(u16::from_be_bytes([length[0], length[1]]));
                if size == 0 {
                    return Err(Error::Invalid("zero hvcC NAL length"));
                }
                check_limit(size, limits.maximum_nal_bytes)?;
                let end = body.checked_add(size).ok_or(Error::Overflow)?;
                let payload = raw.get(body..end).ok_or(Error::Truncated)?;
                let header = NalHeader::parse(Codec::Hevc, payload)?;
                self.position = end;
                self.remaining -= 1;
                if self.remaining == 0 {
                    self.phase = ScanPhase::ArrayEnd;
                }
                Ok(Event::Nal {
                    length: start..body,
                    payload: body..end,
                    header,
                })
            }
            ScanPhase::ArrayEnd => {
                self.index = self.index.checked_add(1).ok_or(Error::Overflow)?;
                self.phase = ScanPhase::Array;
                Ok(Event::ArrayEnd(self.position))
            }
        }
    }
}

enum Phase {
    Prefix,
    Count,
    ReserveArrays,
    ReserveNals,
    Fill,
    FillInline(u8),
    Complete,
}

const INLINE_DESCRIPTORS: usize = 4;

#[derive(Debug)]
enum DescriptorStorage<T> {
    Inline {
        data: [T; INLINE_DESCRIPTORS],
        len: u8,
    },
    Heap(BudgetVec<T>),
}

impl<T> DescriptorStorage<T> {
    fn new(empty: impl FnMut(usize) -> T) -> Self {
        Self::Inline {
            data: std::array::from_fn(empty),
            len: 0,
        }
    }

    fn required_heap_bytes(count: usize) -> Result<usize> {
        if count <= INLINE_DESCRIPTORS {
            Ok(0)
        } else {
            count.checked_mul(size_of::<T>()).ok_or(Error::Overflow)
        }
    }

    // Reservation precedes the fill walk. A failed heap reservation keeps the
    // empty vector and its pool handle for the same checkpoint's next attempt.
    fn reserve(&mut self, pool: &Budget, count: usize) -> Result<()> {
        if count > INLINE_DESCRIPTORS {
            if !matches!(self, Self::Heap(_)) {
                *self = Self::Heap(BudgetVec::new(pool));
            }
            if let Self::Heap(values) = self {
                values.try_reserve_exact(count)?;
            }
        }
        Ok(())
    }

    fn try_push(&mut self, value: T) -> Result<()> {
        match self {
            Self::Inline { data, len } => {
                *data
                    .get_mut(usize::from(*len))
                    .ok_or(Error::Invalid("hvcC inline descriptor count"))? = value;
                *len += 1;
                Ok(())
            }
            Self::Heap(values) => values.try_push(value).map_err(|e| e.error),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => usize::from(*len),
            Self::Heap(values) => values.len(),
        }
    }

    fn as_slice(&self) -> &[T] {
        match self {
            Self::Inline { data, len } => &data[..usize::from(*len)],
            Self::Heap(values) => values.as_slice(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [T] {
        match self {
            Self::Inline { data, len } => &mut data[..usize::from(*len)],
            Self::Heap(values) => values.as_mut_slice(),
        }
    }

    fn heap_len(&self) -> usize {
        match self {
            Self::Inline { .. } => 0,
            Self::Heap(values) => values.len(),
        }
    }

    fn heap_capacity(&self) -> usize {
        match self {
            Self::Inline { .. } => 0,
            Self::Heap(values) => values.capacity(),
        }
    }
}

/// One externally framed, immutable, same-pool hvcC record without a box header.
/// A resumable grammar walk validates the record and fills descriptors when both
/// tables fit inline. A second walk fills the tables when either needs charged
/// heap storage. Source bytes, including unknown NAL bodies, all arrays and any
/// suffix, are retained without rewriting. Fixed-header validity is distinct from
/// array/type/reserved-pattern issues and from decoder validity.
pub struct HvcConfigParser {
    raw: Option<Bytes>,
    source: HvcConfigSource,
    limits: HvcConfigLimits,
    phase: Phase,
    scan: Scan,
    fields: Option<HvcConfigFields>,
    width: Option<LengthSize>,
    total: usize,
    understood_end: usize,
    // Taken by the published output and dropped by a latched failure, so no
    // spare pool handle or empty replacement vectors are kept for either.
    arrays: Option<DescriptorStorage<HvcArrayDescriptor>>,
    nals: Option<DescriptorStorage<HvcNalDescriptor>>,
    failure: Option<Error>,
    observed_width_three: bool,
}

impl HvcConfigParser {
    pub fn new(
        pool: &Budget,
        source: HvcConfigSource,
        raw: Bytes,
        limits: HvcConfigLimits,
    ) -> Result<Self> {
        if limits.maximum_record_bytes == 0 || limits.maximum_nal_bytes == 0 {
            return Err(Error::Invalid("zero hvcC byte limit"));
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
            scan: Scan::new(0),
            fields: None,
            width: None,
            total: 0,
            understood_end: 0,
            arrays: Some(DescriptorStorage::new(|_| HvcArrayDescriptor {
                range: 0..0,
                first_nal: 0,
                nal_count: 0,
            })),
            nals: Some(DescriptorStorage::new(|_| HvcNalDescriptor {
                payload_start: 0,
                payload_len: 0,
                header: NalHeader::Hevc {
                    nal_unit_type: 0,
                    layer_id: 0,
                    temporal_id_plus_one: 1,
                },
                array: 0,
                declared_type: 0,
            })),
            failure: None,
            observed_width_three: false,
        })
    }

    // Crate-private: only the separately typed observed-record wrapper can
    // publish this result. The ordinary source constructor remains strict.
    pub(crate) fn new_observed_width(
        pool: &Budget,
        source: HvcConfigSource,
        raw: Bytes,
        limits: HvcConfigLimits,
    ) -> Result<Self> {
        let mut parser = Self::new(pool, source, raw, limits)?;
        parser.observed_width_three = true;
        Ok(parser)
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HvcConfiguration>> {
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
        ready: &mut Option<HvcConfiguration>,
    ) -> Result<ConfigProgress> {
        self.step_into_with(work, cancel, ready, |record| record)
    }

    // Share the outlined grammar walk; adapt only the final publication.
    #[inline]
    pub(crate) fn step_into_with<T>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<T>,
        wrap: impl FnOnce(HvcConfiguration) -> T,
    ) -> Result<ConfigProgress> {
        if ready.is_some() {
            return Err(Error::Invalid("configuration output sink occupied"));
        }
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance(work, cancel).and_then(|progress| {
            if progress == ConfigProgress::Ready {
                let fields = self.fields.ok_or(Error::Invalid("missing hvcC fields"))?;
                let width = self
                    .width
                    .ok_or(Error::Invalid("missing hvcC length width"))?;
                *ready = Some(wrap(HvcConfiguration {
                    raw: self
                        .raw
                        .take()
                        .ok_or(Error::Invalid("missing hvcC record"))?,
                    source: self.source,
                    fields,
                    width,
                    arrays: self
                        .arrays
                        .take()
                        .ok_or(Error::Invalid("missing hvcC descriptors"))?,
                    nals: self
                        .nals
                        .take()
                        .ok_or(Error::Invalid("missing hvcC descriptors"))?,
                    understood_end: self.understood_end,
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
            self.arrays = None;
            self.nals = None;
        }
        result
    }

    fn tables_fit_inline(&self) -> bool {
        usize::from(self.scan.array_count) <= INLINE_DESCRIPTORS && self.total <= INLINE_DESCRIPTORS
    }

    #[inline(never)]
    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ConfigProgress> {
        loop {
            if matches!(self.phase, Phase::Complete) {
                return Ok(ConfigProgress::Complete);
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(ConfigProgress::Yield);
            }
            let raw = self
                .raw
                .as_ref()
                .ok_or(Error::Invalid("missing hvcC record"))?;
            match self.phase {
                Phase::Prefix => {
                    if raw[0] != 1 {
                        return Err(Error::Unsupported("hvcC configuration version"));
                    }
                    let fields = inspect_hvc_config_prefix(raw.as_ref())?;
                    if fields.length_size_minus_one() == 2 && !self.observed_width_three {
                        return Err(Error::Unsupported("hvcC three-byte sample length"));
                    }
                    let count = fields.array_count();
                    check_limit(usize::from(count), self.limits.maximum_arrays)?;
                    self.width = Some(LengthSize::new(fields.length_size_minus_one() + 1)?);
                    self.fields = Some(fields);
                    self.scan = Scan::new(count);
                    self.phase = Phase::Count;
                }
                Phase::Count | Phase::Fill => {
                    // Commit scan state only after the event's work succeeds.
                    let filling = matches!(self.phase, Phase::Fill);
                    let mut next = self.scan;
                    let event = next.next(raw.as_ref(), self.limits)?;
                    match event {
                        Event::Array { header, count } => {
                            let first = if filling {
                                self.nals.as_ref().map_or(0, DescriptorStorage::len)
                            } else {
                                let first = self.total;
                                let total = first.checked_add(count).ok_or(Error::Overflow)?;
                                check_limit(total, self.limits.maximum_nals)?;
                                self.total = total;
                                first
                            };
                            if filling || self.tables_fit_inline() {
                                self.arrays
                                    .as_mut()
                                    .ok_or(Error::Invalid("missing hvcC descriptors"))?
                                    .try_push(HvcArrayDescriptor::new(header, first, count)?)?;
                            }
                        }
                        Event::Nal {
                            length,
                            payload,
                            header,
                        } if filling || self.tables_fit_inline() => {
                            self.nals
                                .as_mut()
                                .ok_or(Error::Invalid("missing hvcC descriptors"))?
                                .try_push(HvcNalDescriptor::new(
                                    &length,
                                    payload,
                                    header,
                                    usize::from(next.index),
                                    next.declared_type,
                                ))?;
                        }
                        Event::ArrayEnd(end) if filling || self.tables_fit_inline() => {
                            let array = self
                                .arrays
                                .as_mut()
                                .ok_or(Error::Invalid("missing hvcC descriptors"))?
                                .as_mut_slice()
                                .last_mut()
                                .ok_or(Error::Invalid("missing hvcC array"))?;
                            if array.nal_indices().end
                                != self.nals.as_ref().map_or(0, DescriptorStorage::len)
                            {
                                return Err(Error::Invalid("hvcC array second walk"));
                            }
                            array.range.end = end;
                        }
                        Event::End(end) => {
                            if !filling {
                                self.understood_end = end;
                                // Detect an intrinsically impossible live-set without
                                // blaming caller retention. Capacity is charged by
                                // BudgetVec, including any allocator over-allocation.
                                let array_bytes =
                                    DescriptorStorage::<HvcArrayDescriptor>::required_heap_bytes(
                                        usize::from(self.scan.array_count),
                                    )?;
                                let nal_bytes =
                                    DescriptorStorage::<HvcNalDescriptor>::required_heap_bytes(
                                        self.total,
                                    )?;
                                let minimum = Budget::control_storage_bytes()
                                    .checked_add(raw.retained_storage_bytes())
                                    .and_then(|n| n.checked_add(array_bytes))
                                    .and_then(|n| n.checked_add(nal_bytes))
                                    .ok_or(Error::Overflow)?;
                                let limit = raw.budget().limit();
                                check_limit(minimum, limit)?;
                                self.phase = Phase::ReserveArrays;
                            } else {
                                if end != self.understood_end
                                    || self.nals.as_ref().map_or(0, DescriptorStorage::len)
                                        != self.total
                                    || self.arrays.as_ref().map_or(0, DescriptorStorage::len)
                                        != usize::from(next.array_count)
                                {
                                    return Err(Error::Invalid("hvcC second walk"));
                                }
                                return Ok(ConfigProgress::Ready);
                            }
                        }
                        _ => {}
                    }
                    self.scan = next;
                }
                Phase::ReserveArrays => {
                    let arrays = self
                        .arrays
                        .as_mut()
                        .ok_or(Error::Invalid("missing hvcC descriptors"))?;
                    arrays.reserve(raw.budget(), usize::from(self.scan.array_count))?;
                    if self.total > INLINE_DESCRIPTORS
                        && let DescriptorStorage::Inline { len, .. } = arrays
                    {
                        // Only NALs spill. The fill walk rebuilds any cached
                        // array prefix, while NAL reservation replaces its cache.
                        *len = 0;
                    }
                    self.phase = Phase::ReserveNals;
                }
                Phase::ReserveNals => {
                    self.nals
                        .as_mut()
                        .ok_or(Error::Invalid("missing hvcC descriptors"))?
                        .reserve(raw.budget(), self.total)?;
                    if self.tables_fit_inline() {
                        // Retain one charged checkpoint per array start, NAL,
                        // array end and record end, even across short steps.
                        let events = 2 * usize::from(self.scan.array_count) + self.total + 1;
                        self.phase = Phase::FillInline(events as u8);
                    } else {
                        self.scan = Scan::new(self.scan.array_count);
                        self.phase = Phase::Fill;
                    }
                }
                Phase::FillInline(ref mut remaining) => {
                    *remaining -= 1;
                    if *remaining == 0 {
                        return Ok(ConfigProgress::Ready);
                    }
                }
                Phase::Complete => return Ok(ConfigProgress::Complete),
            }
        }
    }
}

fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct HvcArrayDescriptor {
    range: Range<usize>,
    first_nal: u32,
    nal_count: u16,
}
impl HvcArrayDescriptor {
    fn new(header: Range<usize>, first: usize, count: usize) -> Result<Self> {
        // Scan already physically checked the three literal header bytes.
        first.checked_add(count).ok_or(Error::Overflow)?;
        Ok(Self {
            range: header,
            first_nal: first as u32,
            nal_count: count as u16,
        })
    }
    fn nal_indices(&self) -> Range<usize> {
        let first = self.first_nal as usize;
        first..first + usize::from(self.nal_count)
    }
}
#[derive(Clone)]
struct HvcNalDescriptor {
    // Scan checks the full-width start and end using the wire u16 length.
    // Keep that length and the full-width start to recover the same span.
    payload_start: usize,
    payload_len: u16,
    header: NalHeader,
    array: u8,
    declared_type: u8,
}
// Shows the derived ranges and counts instead of their stored parts.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for HvcNalDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HvcNalDescriptor")
            .field("payload", &self.payload_range())
            .field("header", &self.header)
            .field("array", &self.array)
            .field("declared_type", &self.declared_type)
            .finish()
    }
}
impl HvcNalDescriptor {
    fn new(
        length: &Range<usize>,
        payload: Range<usize>,
        header: NalHeader,
        array: usize,
        declared_type: u8,
    ) -> Self {
        // Scan::next checked body + size with size decoded from exactly two
        // wire bytes. Its payload range therefore has this u16 length.
        debug_assert!(length.end == payload.start && length.len() == 2);
        let payload_len = payload.len();
        debug_assert!(u16::try_from(payload_len).is_ok());
        Self {
            payload_start: payload.start,
            payload_len: payload_len as u16,
            header,
            array: array as u8,
            declared_type,
        }
    }
    fn payload_range(&self) -> Range<usize> {
        // The checked end from Scan is start + the same immutable length.
        self.payload_start..self.payload_start + usize::from(self.payload_len)
    }
    fn length_offset(&self) -> usize {
        // Scan produced payload.start = length.start + 2 with a checked add.
        self.payload_start - 2
    }
}
/// Complete hvcC facts with one raw record handle. Arrays and NALs independently
/// keep up to four descriptors inline; larger tables have charged heap capacity.
/// Array headers and NAL spans are borrowed inspections.
#[derive(Debug)]
pub struct HvcConfiguration {
    raw: Bytes,
    source: HvcConfigSource,
    fields: HvcConfigFields,
    width: LengthSize,
    arrays: DescriptorStorage<HvcArrayDescriptor>,
    nals: DescriptorStorage<HvcNalDescriptor>,
    understood_end: usize,
}
impl HvcConfiguration {
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub const fn source(&self) -> HvcConfigSource {
        self.source
    }
    pub const fn fields(&self) -> HvcConfigFields {
        self.fields
    }
    pub const fn length_size(&self) -> LengthSize {
        self.width
    }
    pub const fn understood_end(&self) -> usize {
        self.understood_end
    }
    pub fn nal_count(&self) -> usize {
        self.nals.len()
    }
    pub fn array_count(&self) -> usize {
        self.arrays.len()
    }
    pub fn nal(&self, index: usize) -> Option<HvcConfigNal<'_>> {
        self.nals
            .as_slice()
            .get(index)
            .map(|descriptor| HvcConfigNal {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    pub fn nals(
        &self,
    ) -> impl ExactSizeIterator<Item = HvcConfigNal<'_>> + DoubleEndedIterator + '_ {
        self.nals
            .as_slice()
            .iter()
            .map(move |descriptor| HvcConfigNal {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    pub fn array(&self, index: usize) -> Option<HvcConfigArray<'_>> {
        self.arrays
            .as_slice()
            .get(index)
            .map(|descriptor| HvcConfigArray {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    pub fn arrays(
        &self,
    ) -> impl ExactSizeIterator<Item = HvcConfigArray<'_>> + DoubleEndedIterator + '_ {
        self.arrays
            .as_slice()
            .iter()
            .map(move |descriptor| HvcConfigArray {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    pub fn opaque_suffix(&self) -> &[u8] {
        &self.raw[self.understood_end..]
    }
    pub fn retain_opaque_suffix(&self) -> Result<Bytes> {
        self.raw.slice(self.understood_end..self.raw.len())
    }
    /// Element-count lower bound for heap descriptor storage; inline tables
    /// contribute zero and heap capacity may be larger.
    pub(crate) fn descriptor_lower_bound(&self) -> Result<usize> {
        self.nals
            .heap_len()
            .checked_mul(size_of::<HvcNalDescriptor>())
            .and_then(|n| {
                self.arrays
                    .heap_len()
                    .checked_mul(size_of::<HvcArrayDescriptor>())
                    .and_then(|a| a.checked_add(n))
            })
            .ok_or(Error::Overflow)
    }
    /// Heap descriptor capacity only; up to four entries per table are inline.
    pub fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.arrays
            .heap_capacity()
            .checked_mul(size_of::<HvcArrayDescriptor>())
            .and_then(|a| {
                self.nals
                    .heap_capacity()
                    .checked_mul(size_of::<HvcNalDescriptor>())
                    .and_then(|n| a.checked_add(n))
            })
            .ok_or(Error::Overflow)
    }
    pub fn retained_storage_bytes(&self) -> Result<usize> {
        self.descriptor_storage_bytes()?
            .checked_add(self.raw.retained_storage_bytes())
            .ok_or(Error::Overflow)
    }
    pub fn syntax_issues(&self) -> u32 {
        self.fields().syntax_issues()
            | u32::from(self.arrays().any(|a| a.reserved_bit())) << 6
            | u32::from(self.nals().any(|n| !n.type_matches_declaration())) << 7
    }
}

/// A borrowed hvcC leaf is tied to its configuration's lifetime.
///
/// ```compile_fail,E0505
/// use triergon_framing::HvcConfiguration;
/// fn escape(view: HvcConfiguration) {
///     let leaf = view.nal(0).unwrap();
///     drop(view);
///     let _ = leaf.length_field_bytes();
/// }
/// ```
///
/// ```compile_fail,E0505
/// use triergon_framing::HvcConfiguration;
/// fn escape(view: HvcConfiguration) -> usize {
///     let suffix = view.opaque_suffix();
///     drop(view);
///     suffix.len()
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct HvcConfigNal<'a> {
    raw: &'a Bytes,
    source: HvcConfigSource,
    descriptor: &'a HvcNalDescriptor,
}
impl<'a> HvcConfigNal<'a> {
    pub fn source_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.payload_range()]
    }
    pub fn length_field_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.length_offset()..self.descriptor.payload_start]
    }
    pub fn source_range(&self) -> Range<usize> {
        self.descriptor.payload_range()
    }
    pub const fn source(&self) -> HvcConfigSource {
        self.source
    }
    pub const fn header(&self) -> NalHeader {
        self.descriptor.header
    }
    pub const fn array_index(&self) -> usize {
        self.descriptor.array as usize
    }
    pub const fn declared_nal_type(&self) -> u8 {
        self.descriptor.declared_type
    }
    pub fn type_matches_declaration(&self) -> bool {
        self.header().nal_unit_type() == self.descriptor.declared_type
    }
}

/// ```compile_fail,E0505
/// use triergon_framing::HvcConfiguration;
/// fn escape(view: HvcConfiguration) -> usize {
///     let header = view.array(0).unwrap().source_header_bytes();
///     drop(view);
///     header.len()
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct HvcConfigArray<'a> {
    raw: &'a Bytes,
    source: HvcConfigSource,
    descriptor: &'a HvcArrayDescriptor,
}
impl<'a> HvcConfigArray<'a> {
    pub fn source_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.range.clone()]
    }
    pub fn source_header_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.range.start..self.descriptor.range.start + 3]
    }
    pub const fn source(&self) -> HvcConfigSource {
        self.source
    }
    pub fn completeness(&self) -> bool {
        self.source_header_bytes()[0] & 128 != 0
    }
    pub fn reserved_bit(&self) -> bool {
        self.source_header_bytes()[0] & 64 != 0
    }
    pub fn declared_nal_type(&self) -> u8 {
        self.source_header_bytes()[0] & 63
    }
    pub fn source_range(&self) -> Range<usize> {
        self.descriptor.range.clone()
    }
    pub fn nal_indices(&self) -> Range<usize> {
        self.descriptor.nal_indices()
    }
}

#[cfg(test)]
mod descriptor_guards {
    use super::*;

    const PACKED_NAL_BYTES: usize = size_of::<usize>() + 8;

    #[test]
    fn nal_descriptor_uses_one_word_and_eight_bytes() {
        assert_eq!(size_of::<HvcNalDescriptor>(), PACKED_NAL_BYTES);
    }

    #[test]
    fn packed_nals_fit_the_exact_pool_and_keep_full_source_extents() {
        let mut record = vec![
            1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 120, 240, 0, 252, 253, 248, 248, 0, 0, 15, 1,
        ];
        record.extend_from_slice(&[0xa0, 0, 2, 0xff, 0xff, 0x40, 1]);
        record.resize(65_563, 0x5a);
        record.extend_from_slice(&[0, 3, 0x40, 1, 0x91, 0xe7]);
        let mut backing = vec![0xa5; record.len() + 37];
        let record_end = 19 + record.len();
        backing[19..record_end].copy_from_slice(&record);
        let owner_bytes = backing.capacity() + Bytes::owner_overhead_bytes();
        let table_bytes = 0; // One array and two NALs stay inline, including high offsets.
        let limit = Budget::control_storage_bytes() + owner_bytes + table_bytes;
        let pool = Budget::new(limit).unwrap();
        let owner = Bytes::try_from_vec(&pool, backing)
            .map_err(|e| e.error())
            .unwrap();
        let raw = owner.slice(19..record_end).unwrap();
        let source = HvcConfigSource {
            source: SourceId(7),
            generation: u64::MAX,
            offset: u64::MAX - record.len() as u64,
        };
        let limits = HvcConfigLimits {
            maximum_record_bytes: record.len(),
            maximum_arrays: 1,
            maximum_nals: 2,
            maximum_nal_bytes: 65_535,
        };
        let mut parser = HvcConfigParser::new(&pool, source, raw, limits).unwrap();
        let cancel = AtomicBool::new(false);
        let view = (0..32)
            .find_map(
                |_| match parser.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                    BuildStep::Ready(view) => Some(view),
                    BuildStep::Yield => None,
                    BuildStep::Complete => panic!("configuration completed without output"),
                },
            )
            .expect("configuration fits the bounded work schedule");
        assert_eq!(view.descriptor_storage_bytes().unwrap(), table_bytes);
        assert_eq!(
            view.retained_storage_bytes().unwrap(),
            owner_bytes + table_bytes
        );
        assert_eq!(pool.used(), limit);
        assert_eq!(view.source(), source);
        assert_eq!(view.nal(0).unwrap().source_range(), 28..65_563);
        assert_eq!(view.nal(0).unwrap().length_field_bytes(), [0xff, 0xff]);
        assert_eq!(view.nal(0).unwrap().source_bytes(), &record[28..65_563]);
        assert_eq!(view.opaque_suffix(), [0xe7]);
        drop((view, parser, owner));
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn descriptors_hold_no_owner() {
        assert!(!std::mem::needs_drop::<HvcNalDescriptor>());
        assert!(!std::mem::needs_drop::<HvcArrayDescriptor>());
    }

    #[test]
    fn symbolic_maximum_membership_and_checked_add() {
        // 254 full arrays precede the last one: 255*65535 memberships fit u32.
        let first = 254 * 65_535;
        let array = HvcArrayDescriptor::new(0..3, first, 65_535).unwrap();
        assert_eq!(array.nal_indices(), first..255 * 65_535);
        assert_eq!(
            HvcArrayDescriptor::new(0..3, usize::MAX, 1).err(),
            Some(Error::Overflow)
        );
        let header = NalHeader::parse(Codec::Hevc, &[0x40, 0x01]).unwrap();
        let high = usize::MAX - 2;
        let nal = HvcNalDescriptor::new(&(high - 2..high), high..usize::MAX, header, 254, 32);
        assert_eq!(nal.payload_range(), high..usize::MAX);
        assert_eq!(nal.length_offset(), high - 2);
        assert_eq!((nal.array, nal.declared_type), (254, 32));
    }

    #[test]
    fn inline_and_heap_tables_keep_full_width_descriptor_extents() {
        let pool = Budget::new(4096).unwrap();
        let baseline = pool.used();
        let header = NalHeader::parse(Codec::Hevc, &[0x40, 1]).unwrap();
        for count in [4, 5] {
            let mut arrays = DescriptorStorage::new(|_| HvcArrayDescriptor {
                range: 0..0,
                first_nal: 0,
                nal_count: 0,
            });
            let mut nals =
                DescriptorStorage::new(|_| HvcNalDescriptor::new(&(0..2), 2..4, header, 0, 32));
            arrays.reserve(&pool, count).unwrap();
            nals.reserve(&pool, count).unwrap();
            for index in 0..count {
                let start = usize::MAX - 5 - index;
                arrays
                    .try_push(HvcArrayDescriptor::new(start..start + 3, index, 1).unwrap())
                    .unwrap();
                nals.try_push(HvcNalDescriptor::new(
                    &(start - 2..start),
                    start..start + 2,
                    header,
                    index,
                    32,
                ))
                .unwrap();
            }
            for index in 0..count {
                let start = usize::MAX - 5 - index;
                assert_eq!(arrays.as_slice()[index].range, start..start + 3);
                assert_eq!(arrays.as_slice()[index].nal_indices(), index..index + 1);
                assert_eq!(nals.as_slice()[index].payload_range(), start..start + 2);
                assert_eq!(nals.as_slice()[index].length_offset(), start - 2);
            }
            let heap = if count == 4 {
                0
            } else {
                count * (size_of::<HvcArrayDescriptor>() + PACKED_NAL_BYTES)
            };
            assert_eq!(pool.used(), baseline + heap);
            drop((arrays, nals));
            assert_eq!(pool.used(), baseline);
        }
    }
}
