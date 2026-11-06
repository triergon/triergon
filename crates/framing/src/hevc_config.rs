//! Version1 hvcC source-wire facts.
//! Raw syntax, fixed NAL headers and bounded extents; no RBSP/decoder certificate.
use crate::{BuildStep, Codec, LengthSize, NalHeader};
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
#[derive(Clone)]
pub struct HvcConfigArray {
    header: Bytes,
    range: Range<usize>,
    first_nal: u32,
    nal_count: u16,
}
// Shows the derived ranges and counts instead of their stored parts.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for HvcConfigArray {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HvcConfigArray")
            .field("header", &self.header)
            .field("range", &self.range)
            .field("nals", &self.nal_indices())
            .finish()
    }
}
impl HvcConfigArray {
    pub fn source_header_bytes(&self) -> &Bytes {
        &self.header
    }
    pub fn completeness(&self) -> bool {
        self.header[0] & 128 != 0
    }
    pub fn reserved_bit(&self) -> bool {
        self.header[0] & 64 != 0
    }
    pub fn declared_nal_type(&self) -> u8 {
        self.header[0] & 63
    }
    pub fn source_range(&self) -> Range<usize> {
        self.range.clone()
    }
    pub fn nal_indices(&self) -> Range<usize> {
        let start = self.first_nal as usize;
        start..start + usize::from(self.nal_count)
    }
}
#[derive(Clone)]
pub struct HvcConfigNal {
    payload: Bytes,
    length: Bytes,
    // The checked source end is start+payload.len(). Both bytes and this
    // source-relative start are immutable; no offset or length is narrowed.
    range_start: usize,
    header: NalHeader,
    // Scan admits at most the literal u8 array_count, so NAL indices refer
    // to arrays0..254. The public getter still returns the exact usize value.
    array: u8,
    declared_type: u8,
}
// Shows the derived ranges and counts instead of their stored parts.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for HvcConfigNal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HvcConfigNal")
            .field("payload", &self.payload)
            .field("length", &self.length)
            .field("range", &self.source_range())
            .field("header", &self.header)
            .field("array", &self.array)
            .field("declared_type", &self.declared_type)
            .finish()
    }
}
impl HvcConfigNal {
    pub fn source_bytes(&self) -> &Bytes {
        &self.payload
    }
    pub fn length_field_bytes(&self) -> &Bytes {
        &self.length
    }
    pub fn source_range(&self) -> Range<usize> {
        // Construction already checked start..end against the original record
        // and sliced exactly that span. Its immutable length is end-start,
        // including when the record itself is a view of a larger owner.
        self.range_start..self.range_start + self.payload.len()
    }
    pub const fn header(&self) -> NalHeader {
        self.header
    }
    pub const fn array_index(&self) -> usize {
        self.array as usize
    }
    pub const fn declared_nal_type(&self) -> u8 {
        self.declared_type
    }
    pub fn type_matches_declaration(&self) -> bool {
        self.header.nal_unit_type() == self.declared_type
    }
}
pub struct HvcConfiguration {
    raw: Bytes,
    source: HvcConfigSource,
    fields: HvcConfigFields,
    width: LengthSize,
    arrays: BudgetVec<HvcConfigArray>,
    nals: BudgetVec<HvcConfigNal>,
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
    pub fn arrays(&self) -> &[HvcConfigArray] {
        self.arrays.as_slice()
    }
    pub fn nals(&self) -> &[HvcConfigNal] {
        self.nals.as_slice()
    }
    pub const fn understood_end(&self) -> usize {
        self.understood_end
    }
    pub fn opaque_suffix(&self) -> Result<Bytes> {
        self.raw.slice(self.understood_end..self.raw.len())
    }
    /// Unique retained owners: full source backing plus actual descriptor
    /// capacities. Aliasing NAL/array/suffix slices do not multiply the charge.
    pub fn retained_storage_bytes(&self) -> Result<usize> {
        self.arrays
            .capacity()
            .checked_mul(size_of::<HvcConfigArray>())
            .and_then(|a| {
                self.nals
                    .capacity()
                    .checked_mul(size_of::<HvcConfigNal>())
                    .and_then(|n| a.checked_add(n))
            })
            .and_then(|n| n.checked_add(self.raw.retained_storage_bytes()))
            .ok_or(Error::Overflow)
    }
    /// Includes reserved array-header bits and declared-vs-actual fixed NAL type.
    pub fn syntax_issues(&self) -> u32 {
        self.fields.syntax_issues()
            | u32::from(self.arrays().iter().any(HvcConfigArray::reserved_bit)) << 6
            | u32::from(self.nals().iter().any(|n| !n.type_matches_declaration())) << 7
    }
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
    index: usize,
    array_count: usize,
    remaining: usize,
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
    const fn new(array_count: usize) -> Self {
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
                let count = usize::from(u16::from_be_bytes([header[1], header[2]]));
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
                    count,
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
    Complete,
}

/// One externally framed, immutable, same-pool hvcC record without a box header.
/// Two resumable grammar walks validate/count and then fill exact-capacity
/// descriptors. Source bytes, including unknown NAL bodies, all arrays and any
/// suffix, are retained without rewriting. Fixed-header validity is distinct
/// from array/type/reserved-pattern issues and from decoder validity.
struct HvcParser<S: HvcStorage> {
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
    arrays: Option<BudgetVec<S::Array>>,
    nals: Option<BudgetVec<S::Nal>>,
    failure: Option<Error>,
    observed_width_three: bool,
}

impl<S: HvcStorage> HvcParser<S> {
    fn new(
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
            arrays: Some(BudgetVec::new(pool)),
            nals: Some(BudgetVec::new(pool)),
            failure: None,
            observed_width_three: false,
        })
    }

    // Crate-private: only the separately typed observed-record wrapper can
    // publish this result. The ordinary source constructor remains strict.
    fn new_observed_width(
        pool: &Budget,
        source: HvcConfigSource,
        raw: Bytes,
        limits: HvcConfigLimits,
    ) -> Result<Self> {
        let mut parser = Self::new(pool, source, raw, limits)?;
        parser.observed_width_three = true;
        Ok(parser)
    }

    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStep<S::Output>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance(work, cancel);
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

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<S::Output>> {
        loop {
            if matches!(self.phase, Phase::Complete) {
                return Ok(BuildStep::Complete);
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
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
                    let count = usize::from(fields.array_count());
                    check_limit(count, self.limits.maximum_arrays)?;
                    self.width = Some(LengthSize::new(fields.length_size_minus_one() + 1)?);
                    self.fields = Some(fields);
                    self.scan = Scan::new(count);
                    self.phase = Phase::Count;
                }
                Phase::Count | Phase::Fill => {
                    // Commit scan state only after the event's work succeeds.
                    let mut next = self.scan;
                    let event = next.next(raw.as_ref(), self.limits)?;
                    match event {
                        Event::Array { header, count } => {
                            if matches!(self.phase, Phase::Count) {
                                let total = self.total.checked_add(count).ok_or(Error::Overflow)?;
                                check_limit(total, self.limits.maximum_nals)?;
                                self.total = total;
                            } else {
                                let first = self.nals.as_ref().map_or(0, BudgetVec::len);
                                self.arrays
                                    .as_mut()
                                    .ok_or(Error::Invalid("missing hvcC descriptors"))?
                                    .try_push(S::array(raw, header, first, count)?)
                                    .map_err(|e| e.error)?;
                            }
                        }
                        Event::Nal {
                            length,
                            payload,
                            header,
                        } if matches!(self.phase, Phase::Fill) => {
                            self.nals
                                .as_mut()
                                .ok_or(Error::Invalid("missing hvcC descriptors"))?
                                .try_push(S::nal(
                                    raw,
                                    length,
                                    payload,
                                    header,
                                    next.index,
                                    next.declared_type,
                                )?)
                                .map_err(|e| e.error)?;
                        }
                        Event::ArrayEnd(end) if matches!(self.phase, Phase::Fill) => {
                            let array = self
                                .arrays
                                .as_mut()
                                .ok_or(Error::Invalid("missing hvcC descriptors"))?
                                .as_mut_slice()
                                .last_mut()
                                .ok_or(Error::Invalid("missing hvcC array"))?;
                            if S::array_nal_end(array)
                                != self.nals.as_ref().map_or(0, BudgetVec::len)
                            {
                                return Err(Error::Invalid("hvcC array second walk"));
                            }
                            S::set_array_end(array, end);
                        }
                        Event::End(end) => {
                            if matches!(self.phase, Phase::Count) {
                                self.understood_end = end;
                                // Detect an intrinsically impossible live-set without
                                // blaming caller retention. Capacity is charged by
                                // BudgetVec, including any allocator over-allocation.
                                let array_bytes = self
                                    .scan
                                    .array_count
                                    .checked_mul(size_of::<S::Array>())
                                    .ok_or(Error::Overflow)?;
                                let nal_bytes = self
                                    .total
                                    .checked_mul(size_of::<S::Nal>())
                                    .ok_or(Error::Overflow)?;
                                let minimum = Budget::control_storage_bytes()
                                    .checked_add(raw.retained_storage_bytes())
                                    .and_then(|n| n.checked_add(array_bytes))
                                    .and_then(|n| n.checked_add(nal_bytes))
                                    .ok_or(Error::Overflow)?;
                                let limit = self
                                    .nals
                                    .as_ref()
                                    .ok_or(Error::Invalid("missing hvcC descriptors"))?
                                    .budget()
                                    .limit();
                                check_limit(minimum, limit)?;
                                self.phase = Phase::ReserveArrays;
                            } else {
                                if end != self.understood_end
                                    || self.nals.as_ref().map_or(0, BudgetVec::len) != self.total
                                    || self.arrays.as_ref().map_or(0, BudgetVec::len)
                                        != next.array_count
                                {
                                    return Err(Error::Invalid("hvcC second walk"));
                                }
                                let fields =
                                    self.fields.ok_or(Error::Invalid("missing hvcC fields"))?;
                                let width = self
                                    .width
                                    .ok_or(Error::Invalid("missing hvcC length width"))?;
                                let output = S::finish(HvcBuild {
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
                                    understood_end: end,
                                });
                                self.phase = Phase::Complete;
                                return Ok(BuildStep::Ready(output));
                            }
                        }
                        _ => {}
                    }
                    self.scan = next;
                }
                Phase::ReserveArrays => {
                    self.arrays
                        .as_mut()
                        .ok_or(Error::Invalid("missing hvcC descriptors"))?
                        .try_reserve_exact(self.scan.array_count)?;
                    self.phase = Phase::ReserveNals;
                }
                Phase::ReserveNals => {
                    self.nals
                        .as_mut()
                        .ok_or(Error::Invalid("missing hvcC descriptors"))?
                        .try_reserve_exact(self.total)?;
                    self.scan = Scan::new(self.scan.array_count);
                    self.phase = Phase::Fill;
                }
                Phase::Complete => return Ok(BuildStep::Complete),
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

trait HvcStorage {
    type Array;
    type Nal;
    type Output;
    fn array(raw: &Bytes, header: Range<usize>, first: usize, count: usize) -> Result<Self::Array>;
    fn nal(
        raw: &Bytes,
        length: Range<usize>,
        payload: Range<usize>,
        header: NalHeader,
        array: usize,
        declared_type: u8,
    ) -> Result<Self::Nal>;
    fn array_nal_end(array: &Self::Array) -> usize;
    fn set_array_end(array: &mut Self::Array, end: usize);
    fn finish(data: HvcBuild<Self::Array, Self::Nal>) -> Self::Output;
}
#[derive(Debug)]
struct HvcBuild<A, N> {
    raw: Bytes,
    source: HvcConfigSource,
    fields: HvcConfigFields,
    width: LengthSize,
    arrays: BudgetVec<A>,
    nals: BudgetVec<N>,
    understood_end: usize,
}
struct EagerHvc;
impl HvcStorage for EagerHvc {
    type Array = HvcConfigArray;
    type Nal = HvcConfigNal;
    type Output = HvcConfiguration;
    fn array(raw: &Bytes, header: Range<usize>, first: usize, count: usize) -> Result<Self::Array> {
        Ok(HvcConfigArray {
            header: raw.slice(header.clone())?,
            range: header,
            first_nal: {
                // Preserve the existing checked-add after the checked slice.
                first.checked_add(count).ok_or(Error::Overflow)?;
                first as u32
            },
            nal_count: count as u16,
        })
    }
    fn nal(
        raw: &Bytes,
        length: Range<usize>,
        payload: Range<usize>,
        header: NalHeader,
        array: usize,
        declared_type: u8,
    ) -> Result<Self::Nal> {
        Ok(HvcConfigNal {
            payload: raw.slice(payload.clone())?,
            length: raw.slice(length)?,
            range_start: payload.start,
            header,
            array: array as u8,
            declared_type,
        })
    }
    fn array_nal_end(array: &Self::Array) -> usize {
        array.nal_indices().end
    }
    fn set_array_end(array: &mut Self::Array, end: usize) {
        array.range.end = end;
    }
    fn finish(data: HvcBuild<Self::Array, Self::Nal>) -> Self::Output {
        HvcConfiguration {
            raw: data.raw,
            source: data.source,
            fields: data.fields,
            width: data.width,
            arrays: data.arrays,
            nals: data.nals,
            understood_end: data.understood_end,
        }
    }
}
/// Existing eager API, delegated to the shared checked engine.
pub struct HvcConfigParser {
    inner: HvcParser<EagerHvc>,
}
impl HvcConfigParser {
    pub fn new(
        pool: &Budget,
        source: HvcConfigSource,
        raw: Bytes,
        limits: HvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: HvcParser::new(pool, source, raw, limits)?,
        })
    }
    pub(crate) fn new_observed_width(
        pool: &Budget,
        source: HvcConfigSource,
        raw: Bytes,
        limits: HvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: HvcParser::new_observed_width(pool, source, raw, limits)?,
        })
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HvcConfiguration>> {
        self.inner.step(work, cancel)
    }
}

#[derive(Clone, Debug)]
struct HvcArrayDescriptor {
    range: Range<usize>,
    first_nal: u32,
    nal_count: u16,
}
impl HvcArrayDescriptor {
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
    fn payload_range(&self) -> Range<usize> {
        // The checked end from Scan is start + the same immutable length.
        self.payload_start..self.payload_start + usize::from(self.payload_len)
    }
    fn length_offset(&self) -> usize {
        // Scan produced payload.start = length.start + 2 with a checked add.
        self.payload_start - 2
    }
}
struct CompactHvc;
impl HvcStorage for CompactHvc {
    type Array = HvcArrayDescriptor;
    type Nal = HvcNalDescriptor;
    type Output = HvcConfigurationView;
    fn array(
        _raw: &Bytes,
        header: Range<usize>,
        first: usize,
        count: usize,
    ) -> Result<Self::Array> {
        // Scan already physically checked the three literal header bytes.
        first.checked_add(count).ok_or(Error::Overflow)?;
        Ok(HvcArrayDescriptor {
            range: header,
            first_nal: first as u32,
            nal_count: count as u16,
        })
    }
    fn nal(
        _raw: &Bytes,
        length: Range<usize>,
        payload: Range<usize>,
        header: NalHeader,
        array: usize,
        declared_type: u8,
    ) -> Result<Self::Nal> {
        // Scan::next checked body + size with size decoded from exactly two
        // wire bytes. Its payload range therefore has this u16 length.
        debug_assert!(length.end == payload.start && length.len() == 2);
        let payload_len = payload.len();
        debug_assert!(u16::try_from(payload_len).is_ok());
        Ok(HvcNalDescriptor {
            payload_start: payload.start,
            payload_len: payload_len as u16,
            header,
            array: array as u8,
            declared_type,
        })
    }
    fn array_nal_end(array: &Self::Array) -> usize {
        array.nal_indices().end
    }
    fn set_array_end(array: &mut Self::Array, end: usize) {
        array.range.end = end;
    }
    fn finish(data: HvcBuild<Self::Array, Self::Nal>) -> Self::Output {
        HvcConfigurationView { data }
    }
}

/// Complete hvcC facts with one raw record handle and independently charged
/// descriptor capacities; array headers and NAL spans are borrowed inspections.
#[derive(Debug)]
pub struct HvcConfigurationView {
    data: HvcBuild<HvcArrayDescriptor, HvcNalDescriptor>,
}
impl HvcConfigurationView {
    pub fn source_bytes(&self) -> &Bytes {
        &self.data.raw
    }
    pub const fn source(&self) -> HvcConfigSource {
        self.data.source
    }
    pub const fn fields(&self) -> HvcConfigFields {
        self.data.fields
    }
    pub const fn length_size(&self) -> LengthSize {
        self.data.width
    }
    pub const fn understood_end(&self) -> usize {
        self.data.understood_end
    }
    pub fn nal_count(&self) -> usize {
        self.data.nals.len()
    }
    pub fn array_count(&self) -> usize {
        self.data.arrays.len()
    }
    pub fn nal(&self, index: usize) -> Option<HvcConfigNalRef<'_>> {
        self.data
            .nals
            .as_slice()
            .get(index)
            .map(|descriptor| HvcConfigNalRef {
                raw: &self.data.raw,
                source: self.data.source,
                descriptor,
            })
    }
    pub fn nals(
        &self,
    ) -> impl ExactSizeIterator<Item = HvcConfigNalRef<'_>> + DoubleEndedIterator + '_ {
        self.data
            .nals
            .as_slice()
            .iter()
            .map(move |descriptor| HvcConfigNalRef {
                raw: &self.data.raw,
                source: self.data.source,
                descriptor,
            })
    }
    pub fn array(&self, index: usize) -> Option<HvcConfigArrayRef<'_>> {
        self.data
            .arrays
            .as_slice()
            .get(index)
            .map(|descriptor| HvcConfigArrayRef {
                raw: &self.data.raw,
                source: self.data.source,
                descriptor,
            })
    }
    pub fn arrays(
        &self,
    ) -> impl ExactSizeIterator<Item = HvcConfigArrayRef<'_>> + DoubleEndedIterator + '_ {
        self.data
            .arrays
            .as_slice()
            .iter()
            .map(move |descriptor| HvcConfigArrayRef {
                raw: &self.data.raw,
                source: self.data.source,
                descriptor,
            })
    }
    pub fn opaque_suffix(&self) -> &[u8] {
        &self.data.raw[self.data.understood_end..]
    }
    pub fn retain_opaque_suffix(&self) -> Result<Bytes> {
        self.data
            .raw
            .slice(self.data.understood_end..self.data.raw.len())
    }
    /// Element-count lower bound for live descriptor storage, as the eager
    /// record's lower bound counts its leaves (capacity may be larger).
    pub(crate) fn descriptor_lower_bound(&self) -> Result<usize> {
        self.data
            .nals
            .len()
            .checked_mul(size_of::<HvcNalDescriptor>())
            .and_then(|n| {
                self.data
                    .arrays
                    .len()
                    .checked_mul(size_of::<HvcArrayDescriptor>())
                    .and_then(|a| a.checked_add(n))
            })
            .ok_or(Error::Overflow)
    }
    pub fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.data
            .arrays
            .capacity()
            .checked_mul(size_of::<HvcArrayDescriptor>())
            .and_then(|a| {
                self.data
                    .nals
                    .capacity()
                    .checked_mul(size_of::<HvcNalDescriptor>())
                    .and_then(|n| a.checked_add(n))
            })
            .ok_or(Error::Overflow)
    }
    pub fn retained_storage_bytes(&self) -> Result<usize> {
        self.descriptor_storage_bytes()?
            .checked_add(self.data.raw.retained_storage_bytes())
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
/// use triergon_framing::HvcConfigurationView;
/// fn escape(view: HvcConfigurationView) {
///     let leaf = view.nal(0).unwrap();
///     drop(view);
///     let _ = leaf.length_field_bytes();
/// }
/// ```
///
/// ```compile_fail,E0505
/// use triergon_framing::HvcConfigurationView;
/// fn escape(view: HvcConfigurationView) -> usize {
///     let suffix = view.opaque_suffix();
///     drop(view);
///     suffix.len()
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct HvcConfigNalRef<'a> {
    raw: &'a Bytes,
    source: HvcConfigSource,
    descriptor: &'a HvcNalDescriptor,
}
impl<'a> HvcConfigNalRef<'a> {
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
    pub fn retain(&self) -> RetainedHvcConfigNal {
        RetainedHvcConfigNal {
            raw: Bytes::clone(self.raw),
            source: self.source,
            descriptor: (*self.descriptor).clone(),
        }
    }
    pub fn retain_in(&self, pool: &Budget) -> Result<RetainedHvcConfigNal> {
        if !self.raw.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        Ok(self.retain())
    }
}
#[derive(Clone, Debug)]
pub struct RetainedHvcConfigNal {
    raw: Bytes,
    source: HvcConfigSource,
    descriptor: HvcNalDescriptor,
}
impl RetainedHvcConfigNal {
    pub fn view(&self) -> HvcConfigNalRef<'_> {
        HvcConfigNalRef {
            raw: &self.raw,
            source: self.source,
            descriptor: &self.descriptor,
        }
    }
    pub fn source_bytes(&self) -> &[u8] {
        &self.raw[self.descriptor.payload_range()]
    }
    pub fn length_field_bytes(&self) -> &[u8] {
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
    pub fn retained_storage_bytes(&self) -> usize {
        self.raw.retained_storage_bytes()
    }
    pub fn belongs_to(&self, pool: &Budget) -> bool {
        self.raw.belongs_to(pool)
    }
}

/// ```compile_fail,E0505
/// use triergon_framing::HvcConfigurationView;
/// fn escape(view: HvcConfigurationView) -> usize {
///     let header = view.array(0).unwrap().source_header_bytes();
///     drop(view);
///     header.len()
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct HvcConfigArrayRef<'a> {
    raw: &'a Bytes,
    source: HvcConfigSource,
    descriptor: &'a HvcArrayDescriptor,
}
impl<'a> HvcConfigArrayRef<'a> {
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
    pub fn retain(&self) -> RetainedHvcConfigArray {
        RetainedHvcConfigArray {
            raw: Bytes::clone(self.raw),
            source: self.source,
            descriptor: (*self.descriptor).clone(),
        }
    }
    pub fn retain_in(&self, pool: &Budget) -> Result<RetainedHvcConfigArray> {
        if !self.raw.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        Ok(self.retain())
    }
}
#[derive(Clone, Debug)]
pub struct RetainedHvcConfigArray {
    raw: Bytes,
    source: HvcConfigSource,
    descriptor: HvcArrayDescriptor,
}
impl RetainedHvcConfigArray {
    pub fn view(&self) -> HvcConfigArrayRef<'_> {
        HvcConfigArrayRef {
            raw: &self.raw,
            source: self.source,
            descriptor: &self.descriptor,
        }
    }
    pub fn source_bytes(&self) -> &[u8] {
        &self.raw[self.descriptor.range.clone()]
    }
    pub fn source_header_bytes(&self) -> &[u8] {
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
    pub fn retained_storage_bytes(&self) -> usize {
        self.raw.retained_storage_bytes()
    }
    pub fn belongs_to(&self, pool: &Budget) -> bool {
        self.raw.belongs_to(pool)
    }
}

/// View output using the same hvcC Scan/Event transitions.
pub struct HvcConfigViewParser {
    inner: HvcParser<CompactHvc>,
}
impl HvcConfigViewParser {
    pub fn new(
        pool: &Budget,
        source: HvcConfigSource,
        raw: Bytes,
        limits: HvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: HvcParser::new(pool, source, raw, limits)?,
        })
    }
    pub(crate) fn new_observed_width(
        pool: &Budget,
        source: HvcConfigSource,
        raw: Bytes,
        limits: HvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: HvcParser::new_observed_width(pool, source, raw, limits)?,
        })
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HvcConfigurationView>> {
        self.inner.step(work, cancel)
    }
}

#[cfg(test)]
mod compact_descriptor_guards {
    use super::*;

    const PACKED_NAL_BYTES: usize = size_of::<usize>() + 8;

    #[test]
    fn compact_nal_descriptor_uses_one_word_and_eight_bytes() {
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
        let table_bytes = size_of::<HvcArrayDescriptor>() + 2 * PACKED_NAL_BYTES;
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
        let mut parser = HvcConfigViewParser::new(&pool, source, raw, limits).unwrap();
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
        let leaf = view.nal(1).unwrap().retain();
        drop((view, parser, owner));
        assert_eq!(pool.used(), Budget::control_storage_bytes() + owner_bytes);
        assert_eq!(leaf.source(), source);
        assert_eq!(leaf.source_range(), 65_565..65_568);
        assert_eq!(leaf.length_field_bytes(), [0, 3]);
        assert_eq!(leaf.source_bytes(), [0x40, 1, 0x91]);
        assert!(format!("{leaf:?}").contains("payload: 65565..65568"));
        drop(leaf);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn compact_descriptors_hold_no_owner_and_eager_leaves_do() {
        assert!(!std::mem::needs_drop::<HvcNalDescriptor>());
        assert!(!std::mem::needs_drop::<HvcArrayDescriptor>());
        assert!(std::mem::needs_drop::<HvcConfigNal>());
        assert!(std::mem::needs_drop::<HvcConfigArray>());
        assert!(size_of::<HvcNalDescriptor>() < size_of::<HvcConfigNal>());
        assert!(size_of::<HvcArrayDescriptor>() < size_of::<HvcConfigArray>());
    }

    #[test]
    fn symbolic_maximum_membership_and_checked_add_match_eager_order() {
        let pool = Budget::new(4096).unwrap();
        let raw = Bytes::copy_from_slice(&pool, &[0x20, 0xff, 0xff]).unwrap();
        // 254 full arrays precede the last one: 255*65535 memberships fit u32.
        let first = 254 * 65_535;
        let array = CompactHvc::array(&raw, 0..3, first, 65_535).unwrap();
        assert_eq!(array.nal_indices(), first..255 * 65_535);
        let eager = EagerHvc::array(&raw, 0..3, first, 65_535).unwrap();
        assert_eq!(eager.nal_indices(), array.nal_indices());
        assert_eq!(eager.source_range(), array.range);
        // The checked membership add still fails with Overflow; the eager
        // adapter reports its earlier physical slice failure first.
        assert_eq!(
            CompactHvc::array(&raw, 0..3, usize::MAX, 1).err(),
            Some(Error::Overflow)
        );
        assert_eq!(
            EagerHvc::array(&raw, 0..3, usize::MAX, 1).err(),
            Some(Error::Overflow)
        );
        assert_eq!(
            EagerHvc::array(&raw, 1..4, usize::MAX, 1).err(),
            Some(Error::Invalid("byte span range"))
        );
        let header = NalHeader::parse(Codec::Hevc, &[0x40, 0x01]).unwrap();
        let high = usize::MAX - 2;
        let nal = CompactHvc::nal(&raw, high - 2..high, high..usize::MAX, header, 254, 32).unwrap();
        assert_eq!(nal.payload_range(), high..usize::MAX);
        assert_eq!(nal.length_offset(), high - 2);
        assert_eq!((nal.array, nal.declared_type), (254, 32));
    }
}
