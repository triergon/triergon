//! ISO/IEC14496-15:2010 / incorporated2006 FRExt record grammar.
//! The private source manifest pins the primary clauses. NAL bodies, sample-entry
//! rules and record/SPS consistency are not decoded or certified here.
use crate::{BuildStep, Codec, LengthSize, NalHeader};
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

#[derive(Clone)]
pub struct AvcConfigNal {
    payload: Bytes,
    length_field: Bytes,
    // The checked source end is start+payload.len(). Both bytes and this
    // source-relative start are immutable; no offset or length is narrowed.
    range_start: usize,
    kind: AvcConfigArrayKind,
    header: NalHeader,
}

// Shows the derived ranges and counts instead of their stored parts.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for AvcConfigNal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AvcConfigNal")
            .field("payload", &self.payload)
            .field("length_field", &self.length_field)
            .field("range", &self.source_range())
            .field("kind", &self.kind)
            .field("header", &self.header)
            .finish()
    }
}
impl AvcConfigNal {
    pub fn source_bytes(&self) -> &Bytes {
        &self.payload
    }
    pub fn length_field_bytes(&self) -> &Bytes {
        &self.length_field
    }
    pub fn source_range(&self) -> Range<usize> {
        // Construction already checked start..end against the original record
        // and sliced exactly that span. Its immutable length is end-start,
        // including when the record itself is a view of a larger owner.
        self.range_start..self.range_start + self.payload.len()
    }
    pub const fn array_kind(&self) -> AvcConfigArrayKind {
        self.kind
    }
    pub const fn header(&self) -> NalHeader {
        self.header
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AvcConfigArray {
    kind: AvcConfigArrayKind,
    range: Range<usize>,
    first_nal: u16,
    nal_count: u8,
    count_offset: usize,
}

// Shows the derived ranges and counts instead of their stored parts.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for AvcConfigArray {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AvcConfigArray")
            .field("kind", &self.kind)
            .field("range", &self.range)
            .field("nals", &self.nal_indices())
            .field("count_offset", &self.count_offset)
            .finish()
    }
}
impl AvcConfigArray {
    pub const fn kind(&self) -> AvcConfigArrayKind {
        self.kind
    }
    /// Includes the actual count byte and every length/payload byte. For the
    /// extension array its preceding three format/depth bytes are also included.
    pub fn source_range(&self) -> Range<usize> {
        self.range.clone()
    }
    pub fn nal_indices(&self) -> Range<usize> {
        let start = usize::from(self.first_nal);
        start..start + usize::from(self.nal_count)
    }
    pub const fn count_offset(&self) -> usize {
        self.count_offset
    }
}

pub struct AvcConfiguration {
    raw: Bytes,
    source: AvcConfigSource,
    fields: AvcConfigFields,
    width: LengthSize,
    arrays: [AvcConfigArray; 3],
    array_count: usize,
    nals: BudgetVec<AvcConfigNal>,
    understood_end: usize,
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
    pub fn arrays(&self) -> &[AvcConfigArray] {
        &self.arrays[..self.array_count]
    }
    pub fn nals(&self) -> &[AvcConfigNal] {
        self.nals.as_slice()
    }
    pub const fn understood_end(&self) -> usize {
        self.understood_end
    }
    /// Compatible extension data is retained verbatim, never discarded or used
    /// as an inferred additional parameter-set array.
    pub fn trailing_bytes(&self) -> Result<Bytes> {
        self.raw.slice(self.understood_end..self.raw.len())
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
    remaining: usize,
    kind: AvcConfigArrayKind,
    extension: Option<[u8; 4]>,
}

enum Event {
    Group {
        kind: AvcConfigArrayKind,
        start: usize,
        count_offset: usize,
    },
    Nal {
        kind: AvcConfigArrayKind,
        field: Range<usize>,
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
            extension: None,
        }
    }

    // Single caller per walk; inlining lets the event stay in registers.
    #[inline(always)]
    fn next(&mut self, raw: &[u8], limits: AvcConfigLimits) -> Result<Event> {
        match self.phase {
            ScanPhase::Sequence => self.group(
                AvcConfigArrayKind::Sequence,
                5,
                5,
                usize::from(raw[5] & 31),
                ScanPhase::Picture,
            ),
            ScanPhase::Picture => {
                let count_offset = self.position;
                let count = usize::from(*raw.get(self.position).ok_or(Error::Truncated)?);
                self.position = self.position.checked_add(1).ok_or(Error::Overflow)?;
                self.group(
                    AvcConfigArrayKind::Picture,
                    count_offset,
                    count_offset,
                    count,
                    ScanPhase::Extension,
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
                self.extension = Some(bytes);
                self.position = end;
                self.group(
                    AvcConfigArrayKind::SequenceExtension,
                    start,
                    end - 1,
                    usize::from(bytes[3]),
                    ScanPhase::End,
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
                    field: start..body,
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
        count_offset: usize,
        count: usize,
        after: ScanPhase,
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
            count_offset,
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
/// box header). Two resumable grammar walks count then publish exact-capacity
/// descriptors. A work unit reads at most six fixed bytes or one length+fixed
/// NAL header; opaque NAL bodies are bounded spans, never copied/scanned.
/// Source delivery chunks belong to the enclosing container/core input job.
struct AvcParser<S: AvcStorage> {
    raw: Option<Bytes>,
    source: AvcConfigSource,
    limits: AvcConfigLimits,
    phase: Phase,
    scan: Scan,
    fields: Option<AvcConfigFields>,
    width: Option<LengthSize>,
    arrays: [AvcConfigArray; 3],
    array_count: usize,
    total: usize,
    understood_end: usize,
    // Taken by the published output and dropped by a latched failure, so no
    // spare pool handle or empty replacement vector is kept for either.
    nals: Option<BudgetVec<S::Nal>>,
    failure: Option<Error>,
    observed_width_three: bool,
}

impl<S: AvcStorage> AvcParser<S> {
    fn new(
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
            arrays: std::array::from_fn(|_| AvcConfigArray {
                kind: AvcConfigArrayKind::Sequence,
                range: 0..0,
                first_nal: 0,
                nal_count: 0,
                count_offset: 0,
            }),
            array_count: 0,
            total: 0,
            understood_end: 0,
            nals: Some(BudgetVec::new(pool)),
            failure: None,
            observed_width_three: false,
        })
    }

    // Crate-private: only the separately typed observed-record wrapper can
    // publish this result. The ordinary source constructor remains strict.
    fn new_observed_width(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
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
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.phase, Phase::Complete) {
                return Ok(BuildStep::Complete);
            }
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            let raw = self
                .raw
                .as_ref()
                .ok_or(Error::Invalid("missing AVC record"))?;
            match self.phase {
                Phase::Prefix => {
                    if raw.as_ref()[0] != 1 {
                        return Err(Error::Unsupported("AVC configuration version"));
                    }
                    let prefix: [u8; 6] = raw
                        .as_ref()
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
                    let mut next = self.scan;
                    let event = next.next(raw.as_ref(), self.limits)?;
                    match event {
                        Event::Group {
                            kind,
                            start,
                            count_offset,
                        } if matches!(self.phase, Phase::Count) => {
                            let slot = self
                                .arrays
                                .get_mut(self.array_count)
                                .ok_or(Error::Invalid("AVC array count"))?;
                            *slot = AvcConfigArray {
                                kind,
                                range: start..start,
                                // At most31+255+255 NALs in this grammar.
                                first_nal: self.total as u16,
                                nal_count: 0,
                                count_offset,
                            };
                            self.array_count += 1;
                        }
                        Event::GroupEnd(end) if matches!(self.phase, Phase::Count) => {
                            let slot = &mut self.arrays[self.array_count - 1];
                            slot.range.end = end;
                            // Each group consumed its literal count (at most255).
                            slot.nal_count = (self.total - usize::from(slot.first_nal)) as u8;
                        }
                        Event::Nal {
                            kind,
                            field,
                            payload,
                            header,
                        } => {
                            if matches!(self.phase, Phase::Count) {
                                let count = self.total.checked_add(1).ok_or(Error::Overflow)?;
                                check_limit(count, self.limits.maximum_nals)?;
                                self.total = count;
                            } else {
                                let nal = S::nal(raw, field, payload, kind, header)?;
                                self.nals
                                    .as_mut()
                                    .ok_or(Error::Invalid("missing AVC descriptors"))?
                                    .try_push(nal)
                                    .map_err(|e| e.error)?;
                            }
                        }
                        Event::End(end) => {
                            if matches!(self.phase, Phase::Count) {
                                self.understood_end = end;
                                self.fields
                                    .as_mut()
                                    .ok_or(Error::Invalid("missing AVC fields"))?
                                    .extension = next.extension;
                                self.phase = Phase::Reserve;
                            } else {
                                if end != self.understood_end
                                    || self.nals.as_ref().map_or(0, BudgetVec::len) != self.total
                                {
                                    return Err(Error::Invalid("AVC second walk"));
                                }
                                let output = S::finish(AvcBuild {
                                    raw: self
                                        .raw
                                        .take()
                                        .ok_or(Error::Invalid("missing AVC record"))?,
                                    source: self.source,
                                    fields: self
                                        .fields
                                        .ok_or(Error::Invalid("missing AVC fields"))?,
                                    width: self.width.ok_or(Error::Invalid("missing AVC width"))?,
                                    arrays: self.arrays.clone(),
                                    array_count: self.array_count,
                                    nals: self
                                        .nals
                                        .take()
                                        .ok_or(Error::Invalid("missing AVC descriptors"))?,
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
                Phase::Reserve => {
                    self.nals
                        .as_mut()
                        .ok_or(Error::Invalid("missing AVC descriptors"))?
                        .try_reserve_exact(self.total)?;
                    self.scan = Scan::new();
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

// The default eager API and the compact API share this statically selected
// engine. Only Fill storage and the resulting owned representation differ.
trait AvcStorage {
    type Nal;
    type Output;
    fn nal(
        raw: &Bytes,
        field: Range<usize>,
        payload: Range<usize>,
        kind: AvcConfigArrayKind,
        header: NalHeader,
    ) -> Result<Self::Nal>;
    fn finish(data: AvcBuild<Self::Nal>) -> Self::Output;
}

#[derive(Debug)]
struct AvcBuild<N> {
    raw: Bytes,
    source: AvcConfigSource,
    fields: AvcConfigFields,
    width: LengthSize,
    arrays: [AvcConfigArray; 3],
    array_count: usize,
    nals: BudgetVec<N>,
    understood_end: usize,
}

struct EagerAvc;
impl AvcStorage for EagerAvc {
    type Nal = AvcConfigNal;
    type Output = AvcConfiguration;
    fn nal(
        raw: &Bytes,
        field: Range<usize>,
        payload: Range<usize>,
        kind: AvcConfigArrayKind,
        header: NalHeader,
    ) -> Result<Self::Nal> {
        Ok(AvcConfigNal {
            payload: raw.slice(payload.clone())?,
            length_field: raw.slice(field)?,
            range_start: payload.start,
            kind,
            header,
        })
    }
    fn finish(data: AvcBuild<Self::Nal>) -> Self::Output {
        AvcConfiguration {
            raw: data.raw,
            source: data.source,
            fields: data.fields,
            width: data.width,
            arrays: data.arrays,
            array_count: data.array_count,
            nals: data.nals,
            understood_end: data.understood_end,
        }
    }
}

/// Existing eager API; the complete original owned result is preserved.
pub struct AvcConfigParser {
    inner: AvcParser<EagerAvc>,
}
impl AvcConfigParser {
    pub fn new(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: AvcParser::new(pool, source, raw, limits)?,
        })
    }
    pub(crate) fn new_observed_width(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: AvcParser::new_observed_width(pool, source, raw, limits)?,
        })
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AvcConfiguration>> {
        self.inner.step(work, cancel)
    }
}

#[derive(Clone, Debug)]
struct AvcNalDescriptor {
    payload: Range<usize>,
    // Both endpoints were physically validated by Scan; no source offset is
    // narrowed. Keeping the explicit field start avoids inferring its location.
    length_offset: usize,
    kind: AvcConfigArrayKind,
    header: NalHeader,
}
struct CompactAvc;
impl AvcStorage for CompactAvc {
    type Nal = AvcNalDescriptor;
    type Output = AvcConfigurationView;
    fn nal(
        _raw: &Bytes,
        field: Range<usize>,
        payload: Range<usize>,
        kind: AvcConfigArrayKind,
        header: NalHeader,
    ) -> Result<Self::Nal> {
        Ok(AvcNalDescriptor {
            payload,
            length_offset: field.start,
            kind,
            header,
        })
    }
    fn finish(data: AvcBuild<Self::Nal>) -> Self::Output {
        AvcConfigurationView { data }
    }
}

/// Complete AVC configuration facts with one raw record handle and charged
/// descriptors. Borrowed inspection creates no per-NAL owning byte aliases.
#[derive(Debug)]
pub struct AvcConfigurationView {
    data: AvcBuild<AvcNalDescriptor>,
}
impl AvcConfigurationView {
    pub fn source_bytes(&self) -> &Bytes {
        &self.data.raw
    }
    pub const fn source(&self) -> AvcConfigSource {
        self.data.source
    }
    pub const fn fields(&self) -> AvcConfigFields {
        self.data.fields
    }
    pub const fn sample_length_size(&self) -> LengthSize {
        self.data.width
    }
    pub const fn understood_end(&self) -> usize {
        self.data.understood_end
    }
    pub fn nal_count(&self) -> usize {
        self.data.nals.len()
    }
    pub const fn array_count(&self) -> usize {
        self.data.array_count
    }
    pub fn nal(&self, index: usize) -> Option<AvcConfigNalRef<'_>> {
        self.data
            .nals
            .as_slice()
            .get(index)
            .map(|descriptor| AvcConfigNalRef {
                raw: &self.data.raw,
                source: self.data.source,
                descriptor,
            })
    }
    pub fn nals(
        &self,
    ) -> impl ExactSizeIterator<Item = AvcConfigNalRef<'_>> + DoubleEndedIterator + '_ {
        self.data
            .nals
            .as_slice()
            .iter()
            .map(move |descriptor| AvcConfigNalRef {
                raw: &self.data.raw,
                source: self.data.source,
                descriptor,
            })
    }
    pub fn array(&self, index: usize) -> Option<AvcConfigArrayRef<'_>> {
        self.data.arrays[..self.data.array_count]
            .get(index)
            .map(|descriptor| AvcConfigArrayRef {
                raw: &self.data.raw,
                source: self.data.source,
                descriptor,
            })
    }
    pub fn arrays(
        &self,
    ) -> impl ExactSizeIterator<Item = AvcConfigArrayRef<'_>> + DoubleEndedIterator + '_ {
        self.data.arrays[..self.data.array_count]
            .iter()
            .map(move |descriptor| AvcConfigArrayRef {
                raw: &self.data.raw,
                source: self.data.source,
                descriptor,
            })
    }
    pub fn trailing_bytes(&self) -> &[u8] {
        &self.data.raw[self.data.understood_end..]
    }
    pub fn retain_trailing_bytes(&self) -> Result<Bytes> {
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
            .checked_mul(size_of::<AvcNalDescriptor>())
            .ok_or(Error::Overflow)
    }
    /// Heap descriptor capacity only; the three array descriptors are inline.
    pub fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.data
            .nals
            .capacity()
            .checked_mul(size_of::<AvcNalDescriptor>())
            .ok_or(Error::Overflow)
    }
    /// Full hidden raw backing plus actual descriptor heap capacities.
    pub fn retained_storage_bytes(&self) -> Result<usize> {
        self.descriptor_storage_bytes()?
            .checked_add(self.data.raw.retained_storage_bytes())
            .ok_or(Error::Overflow)
    }
}

/// A NAL borrow cannot escape its configuration; retaining it is explicit.
///
/// ```compile_fail,E0505
/// use triergon_framing::AvcConfigurationView;
/// fn escape(view: AvcConfigurationView) {
///     let leaf = view.nal(0).unwrap();
///     drop(view);
///     let _ = leaf.source_bytes();
/// }
/// ```
///
/// Borrowed bytes carry the view lifetime too, not the ref's:
///
/// ```compile_fail,E0505
/// use triergon_framing::AvcConfigurationView;
/// fn escape(view: AvcConfigurationView) -> usize {
///     let bytes = view.nal(0).unwrap().length_field_bytes();
///     drop(view);
///     bytes.len()
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct AvcConfigNalRef<'a> {
    raw: &'a Bytes,
    source: AvcConfigSource,
    descriptor: &'a AvcNalDescriptor,
}
impl<'a> AvcConfigNalRef<'a> {
    pub fn source_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.payload.clone()]
    }
    pub fn length_field_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.length_offset..self.descriptor.length_offset + 2]
    }
    pub fn source_range(&self) -> Range<usize> {
        self.descriptor.payload.clone()
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
    /// One complete raw-record alias and copied leaf facts; no table is retained.
    pub fn retain(&self) -> RetainedAvcConfigNal {
        RetainedAvcConfigNal {
            raw: Bytes::clone(self.raw),
            source: self.source,
            descriptor: (*self.descriptor).clone(),
        }
    }
    /// An owning export remains charged to its original pool; no implicit move.
    pub fn retain_in(&self, pool: &Budget) -> Result<RetainedAvcConfigNal> {
        if !self.raw.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        Ok(self.retain())
    }
}

/// Independent leaf with exactly one complete record handle. Collections of
/// these values must account their own allocation, e.g. with BudgetVec.
#[derive(Clone, Debug)]
pub struct RetainedAvcConfigNal {
    raw: Bytes,
    source: AvcConfigSource,
    descriptor: AvcNalDescriptor,
}
impl RetainedAvcConfigNal {
    pub fn view(&self) -> AvcConfigNalRef<'_> {
        AvcConfigNalRef {
            raw: &self.raw,
            source: self.source,
            descriptor: &self.descriptor,
        }
    }
    pub fn source_bytes(&self) -> &[u8] {
        &self.raw[self.descriptor.payload.clone()]
    }
    pub fn length_field_bytes(&self) -> &[u8] {
        &self.raw[self.descriptor.length_offset..self.descriptor.length_offset + 2]
    }
    pub fn source_range(&self) -> Range<usize> {
        self.descriptor.payload.clone()
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
    pub fn retained_storage_bytes(&self) -> usize {
        self.raw.retained_storage_bytes()
    }
    pub fn belongs_to(&self, pool: &Budget) -> bool {
        self.raw.belongs_to(pool)
    }
}

/// An array borrow is tied to the view in the same way.
///
/// ```compile_fail,E0505
/// use triergon_framing::AvcConfigurationView;
/// fn escape(view: AvcConfigurationView) -> usize {
///     let array = view.array(0).unwrap();
///     drop(view);
///     array.source_bytes().len()
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct AvcConfigArrayRef<'a> {
    raw: &'a Bytes,
    source: AvcConfigSource,
    descriptor: &'a AvcConfigArray,
}
impl<'a> AvcConfigArrayRef<'a> {
    pub fn source_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.range.clone()]
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
        self.descriptor.count_offset
    }
    pub fn retain(&self) -> RetainedAvcConfigArray {
        RetainedAvcConfigArray {
            raw: Bytes::clone(self.raw),
            source: self.source,
            descriptor: (*self.descriptor).clone(),
        }
    }
    pub fn retain_in(&self, pool: &Budget) -> Result<RetainedAvcConfigArray> {
        if !self.raw.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        Ok(self.retain())
    }
}
#[derive(Clone, Debug)]
pub struct RetainedAvcConfigArray {
    raw: Bytes,
    source: AvcConfigSource,
    descriptor: AvcConfigArray,
}
impl RetainedAvcConfigArray {
    pub fn view(&self) -> AvcConfigArrayRef<'_> {
        AvcConfigArrayRef {
            raw: &self.raw,
            source: self.source,
            descriptor: &self.descriptor,
        }
    }
    pub fn source_bytes(&self) -> &[u8] {
        &self.raw[self.descriptor.range.clone()]
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
        self.descriptor.count_offset
    }
    pub fn retained_storage_bytes(&self) -> usize {
        self.raw.retained_storage_bytes()
    }
    pub fn belongs_to(&self, pool: &Budget) -> bool {
        self.raw.belongs_to(pool)
    }
}

/// Compact output from the exact same checked Count/Reserve/Fill walk.
pub struct AvcConfigViewParser {
    inner: AvcParser<CompactAvc>,
}
impl AvcConfigViewParser {
    pub fn new(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: AvcParser::new(pool, source, raw, limits)?,
        })
    }
    pub(crate) fn new_observed_width(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        Ok(Self {
            inner: AvcParser::new_observed_width(pool, source, raw, limits)?,
        })
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AvcConfigurationView>> {
        self.inner.step(work, cancel)
    }
}

#[cfg(test)]
mod compact_descriptor_guards {
    use super::*;

    #[test]
    fn compact_descriptors_hold_no_owner_and_eager_leaves_hold_two() {
        // No drop glue proves the compact NAL/array descriptors contain no
        // Bytes/Arc owner; the eager leaf keeps its two owning aliases.
        assert!(!std::mem::needs_drop::<AvcNalDescriptor>());
        assert!(!std::mem::needs_drop::<AvcConfigArray>());
        assert!(std::mem::needs_drop::<AvcConfigNal>());
        assert!(size_of::<AvcNalDescriptor>() < size_of::<AvcConfigNal>());
        // The view owns exactly the record handle plus one charged table.
        assert_eq!(
            size_of::<AvcConfigurationView>(),
            size_of::<AvcBuild<AvcNalDescriptor>>()
        );
    }

    #[test]
    fn compact_fill_keeps_full_width_offsets_without_slicing() {
        let pool = Budget::new(4096).unwrap();
        let raw = Bytes::copy_from_slice(&pool, &[0, 2, 0x67, 0x80]).unwrap();
        let header = NalHeader::parse(Codec::Avc, &[0x67]).unwrap();
        let high = usize::MAX - 3;
        let descriptor = CompactAvc::nal(
            &raw,
            high - 2..high,
            high..usize::MAX,
            AvcConfigArrayKind::SequenceExtension,
            header,
        )
        .unwrap();
        // No physical check is repeated after Scan; the extents are copied.
        assert_eq!(descriptor.payload, high..usize::MAX);
        assert_eq!(descriptor.length_offset, high - 2);
        // The eager adapter still slices, so an unchecked span fails there.
        assert_eq!(
            EagerAvc::nal(
                &raw,
                high - 2..high,
                high..usize::MAX,
                AvcConfigArrayKind::Sequence,
                header
            )
            .err(),
            Some(Error::Invalid("byte span range"))
        );
    }
}
