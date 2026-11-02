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

#[derive(Clone, Debug)]
pub struct AvcConfigNal {
    payload: Bytes,
    length_field: Bytes,
    range: Range<usize>,
    kind: AvcConfigArrayKind,
    header: NalHeader,
}

impl AvcConfigNal {
    pub fn source_bytes(&self) -> &Bytes {
        &self.payload
    }
    pub fn length_field_bytes(&self) -> &Bytes {
        &self.length_field
    }
    pub fn source_range(&self) -> Range<usize> {
        self.range.clone()
    }
    pub const fn array_kind(&self) -> AvcConfigArrayKind {
        self.kind
    }
    pub const fn header(&self) -> NalHeader {
        self.header
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvcConfigArray {
    kind: AvcConfigArrayKind,
    range: Range<usize>,
    nals: Range<usize>,
    count_offset: usize,
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
        self.nals.clone()
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
pub struct AvcConfigParser {
    pool: Budget,
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
    nals: BudgetVec<AvcConfigNal>,
    failure: Option<Error>,
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
            pool: pool.clone(),
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
                nals: 0..0,
                count_offset: 0,
            }),
            array_count: 0,
            total: 0,
            understood_end: 0,
            nals: BudgetVec::new(pool),
            failure: None,
        })
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AvcConfiguration>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result {
            if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                self.failure = Some(error);
                self.raw = None;
                self.nals = BudgetVec::new(&self.pool);
            }
        }
        result
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AvcConfiguration>> {
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
                    if prefix[4] & 3 == 2 {
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
                                nals: self.total..self.total,
                                count_offset,
                            };
                            self.array_count += 1;
                        }
                        Event::GroupEnd(end) if matches!(self.phase, Phase::Count) => {
                            let slot = &mut self.arrays[self.array_count - 1];
                            slot.range.end = end;
                            slot.nals.end = self.total;
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
                                let nal = AvcConfigNal {
                                    payload: raw.slice(payload.clone())?,
                                    length_field: raw.slice(field)?,
                                    range: payload,
                                    kind,
                                    header,
                                };
                                self.nals.try_push(nal).map_err(|e| e.error)?;
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
                                if end != self.understood_end || self.nals.len() != self.total {
                                    return Err(Error::Invalid("AVC second walk"));
                                }
                                let output = AvcConfiguration {
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
                                    nals: std::mem::replace(
                                        &mut self.nals,
                                        BudgetVec::new(&self.pool),
                                    ),
                                    understood_end: end,
                                };
                                self.phase = Phase::Complete;
                                return Ok(BuildStep::Ready(output));
                            }
                        }
                        _ => {}
                    }
                    self.scan = next;
                }
                Phase::Reserve => {
                    self.nals.try_reserve_exact(self.total)?;
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
