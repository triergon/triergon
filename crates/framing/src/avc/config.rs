//! The `avcC` record grammar of ISO/IEC 14496-15:2010 with the 2006 FRExt
//! amendment. NAL bodies, sample-entry rules and record/SPS consistency are not
//! decoded or certified here.
use crate::inline_storage::InlineStorage;
use crate::retryable;
use crate::{
    BuildStep, ConfigProgress,
    nal::{Codec, LengthSize, NalHeader},
};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, SourceId, WorkBudget};

/// Where an `avcC` record comes from. The parser copies it to the record and its
/// views and does not interpret it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcConfigSource {
    /// The source the record was read from.
    pub source: SourceId,
    /// Caller-supplied configuration epoch, never inferred from NAL contents.
    pub generation: u64,
    /// Offset of the record's first byte in the source, in bytes. The record must
    /// end at or before `u64::MAX`.
    pub offset: u64,
}

/// The bounds of one `avcC` parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcConfigLimits {
    /// Largest record accepted, in bytes; above 0.
    pub maximum_record_bytes: usize,
    /// Most NAL units in the record, over all its arrays.
    pub maximum_nals: usize,
    /// Largest NAL unit accepted, in bytes; above 0.
    pub maximum_nal_bytes: usize,
}

/// Which of an `avcC` record's NAL unit arrays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvcConfigArrayKind {
    /// `sequenceParameterSetNALUnit`: the sequence parameter sets.
    Sequence,
    /// `pictureParameterSetNALUnit`: the picture parameter sets.
    Picture,
    /// `sequenceParameterSetExtNALUnit`: the sequence parameter set extensions that
    /// end a record with the high-profile fields.
    SequenceExtension,
}

/// The fixed fields of an `avcC` record as stored, reserved bits included: its
/// first six bytes and, when the record has the high-profile extension, the four
/// bytes that open it. [`AvcConfigParser`] reads the extension exactly when
/// `AVCProfileIndication` is 100, 110, 122 or 144.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcConfigFields {
    prefix: [u8; 6],
    extension: Option<[u8; 4]>,
}

impl AvcConfigFields {
    /// The first six bytes of the record, through the byte that holds
    /// `numOfSequenceParameterSets`.
    pub const fn raw_prefix(self) -> [u8; 6] {
        self.prefix
    }
    /// `configurationVersion`; 1 in every parsed record.
    pub const fn version(self) -> u8 {
        self.prefix[0]
    }
    /// `AVCProfileIndication`: the stream's `profile_idc`.
    pub const fn profile_indication(self) -> u8 {
        self.prefix[1]
    }
    /// `profile_compatibility`: the byte between `profile_idc` and `level_idc` of a
    /// sequence parameter set, with its `constraint_set` flags.
    pub const fn profile_compatibility(self) -> u8 {
        self.prefix[2]
    }
    /// `AVCLevelIndication`: the stream's `level_idc`.
    pub const fn level_indication(self) -> u8 {
        self.prefix[3]
    }
    /// `lengthSizeMinusOne`, 0 to 3: one less than the bytes in the length field
    /// before each NAL unit of a sample.
    pub const fn length_size_minus_one(self) -> u8 {
        self.prefix[4] & 3
    }
    /// The six reserved bits before `lengthSizeMinusOne`; 63 when all are set, as the
    /// record format requires.
    pub const fn length_reserved(self) -> u8 {
        self.prefix[4] >> 2
    }
    /// The three reserved bits before `numOfSequenceParameterSets`; 7 when all are
    /// set, as the record format requires.
    pub const fn sequence_count_reserved(self) -> u8 {
        self.prefix[5] >> 5
    }
    /// Physical extension bytes, including all reserved fields and count. `None`
    /// when the record has no extension.
    pub const fn raw_extension(self) -> Option<[u8; 4]> {
        self.extension
    }
    /// `chroma_format`, 0 to 3; `None` when the record has no extension.
    pub fn chroma_format(self) -> Option<u8> {
        self.extension.map(|v| v[0] & 3)
    }
    /// `bit_depth_luma_minus8` as stored, 0 to 7; `None` when the record has no
    /// extension.
    pub fn bit_depth_luma_minus8(self) -> Option<u8> {
        self.extension.map(|v| v[1] & 7)
    }
    /// `bit_depth_chroma_minus8` as stored, 0 to 7; `None` when the record has no
    /// extension.
    pub fn bit_depth_chroma_minus8(self) -> Option<u8> {
        self.extension.map(|v| v[2] & 7)
    }
    /// Local syntax and field issues as a bit set, not full decoder validity. Bits 0
    /// and 1: a reserved bit before `lengthSizeMinusOne` and before
    /// `numOfSequenceParameterSets` is clear. Bits 2, 3 and 4: a reserved bit of the
    /// extension is clear, before `chroma_format`, `bit_depth_luma_minus8` and
    /// `bit_depth_chroma_minus8`. Bits 5 and 6: `bit_depth_luma_minus8` and
    /// `bit_depth_chroma_minus8` are outside `0..=4`.
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

pub(crate) const fn profile_extension_required(profile: u8) -> bool {
    matches!(profile, 100 | 110 | 122 | 144)
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
    fn next(&mut self, raw: &[u8], limits: AvcConfigLimits, receiver: bool) -> Result<Event> {
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
                if !profile_extension_required(raw[1]) {
                    self.phase = ScanPhase::End;
                    return Ok(Event::End(self.position));
                }
                let start = self.position;
                let end = start.checked_add(4).ok_or(Error::Overflow)?;
                let Some(bytes) = raw.get(start..end) else {
                    if receiver {
                        self.phase = ScanPhase::End;
                        return Ok(Event::End(start));
                    }
                    return Err(Error::Truncated);
                };
                let bytes: [u8; 4] = bytes.try_into().map_err(|_| Error::Truncated)?;
                self.position = end;
                // A receiver can publish the base parameter sets when the
                // record ends immediately after the extension header.
                let count = if receiver && end == raw.len() {
                    0
                } else {
                    bytes[3]
                };
                self.group(
                    AvcConfigArrayKind::SequenceExtension,
                    start,
                    count,
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

/// The walk's step; past the prefix it carries the record's six fixed bytes.
#[derive(Clone, Copy)]
enum Phase {
    Prefix,
    Count([u8; 6]),
    Reserve([u8; 6]),
    Fill([u8; 6]),
}

/// The record and its NAL descriptors, held until they are published.
#[repr(C)]
struct Record {
    raw: Bytes,
    nals: AvcNalStorage,
}

/// What one step of the walk ended with.
enum Walked {
    Ready([u8; 6]),
    Yield,
    Complete,
}

/// Parses an `avcC` record (`AVCDecoderConfigurationRecord`) into an
/// [`AvcConfiguration`].
///
/// Input is one already externally framed, same-pool immutable record (without
/// box header). The first walk validates the record before reserving descriptor
/// storage. Small records reuse the validated facts if the fill fits in one step.
/// Other fills walk the record again. A work unit reads at most six fixed bytes
/// or one length and fixed NAL header. NAL bodies remain bounded spans.
/// Source delivery chunks belong to the enclosing container/core input job.
// Hot fields at the offsets the compiler gave them before the state was typed.
#[repr(C)]
pub struct AvcConfigParser {
    scan: Scan,
    // None once published or failed: the output takes it and a failure drops it,
    // so no spare pool handle or empty replacement storage is kept for either.
    record: Option<Record>,
    source: AvcConfigSource,
    limits: AvcConfigLimits,
    array_count: usize,
    total: usize,
    understood_end: usize,
    failure: Option<Error>,
    arrays: [ArrayDescriptor; 3],
    phase: Phase,
    extension: Option<[u8; 4]>,
    receiver: bool,
}

impl std::fmt::Debug for AvcConfigParser {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AvcConfigParser")
            .field("source", &self.source)
            .field("limits", &self.limits)
            .field("total", &self.total)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl AvcConfigParser {
    /// Starts the parse of `raw`, one complete record charged to `pool`. Fails when
    /// `maximum_record_bytes` or `maximum_nal_bytes` is zero, `raw` belongs to another
    /// pool, is empty or is longer than `maximum_record_bytes`, or the record's end
    /// offset in the source overflows `u64`.
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
            record: Some(Record {
                raw,
                nals: AvcNalStorage::new(|_| AvcNalDescriptor::EMPTY),
            }),
            source,
            limits,
            phase: Phase::Prefix,
            scan: Scan::new(),
            arrays: std::array::from_fn(|_| ArrayDescriptor {
                kind: AvcConfigArrayKind::Sequence,
                range: 0..0,
                first_nal: 0,
                nal_count: 0,
            }),
            array_count: 0,
            total: 0,
            understood_end: 0,
            failure: None,
            extension: None,
            receiver: false,
        })
    }

    // Crate-private: only the separately typed observed-record wrapper can
    // publish this result. The ordinary source constructor remains strict.
    pub(crate) fn new_receiver(
        pool: &Budget,
        source: AvcConfigSource,
        raw: Bytes,
        limits: AvcConfigLimits,
    ) -> Result<Self> {
        let mut parser = Self::new(pool, source, raw, limits)?;
        parser.receiver = true;
        Ok(parser)
    }

    /// Advances by at most the units in `work` and returns the record once, as
    /// [`BuildStep`] describes.
    ///
    /// Fails when `cancel` is set; when `configurationVersion` is not 1 or the sample
    /// length field is three bytes; when the record ends inside a field or a NAL unit,
    /// or before the extension that `AVCProfileIndication` 100, 110, 122 or 144
    /// requires; when a NAL unit is empty or has `forbidden_zero_bit` set; when a limit
    /// is exceeded. Every later call returns the same error. A pool that lacks room for
    /// the record's descriptors until other storage is released is `Backpressure` or
    /// `Allocation` instead, and the call can be repeated.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AvcConfiguration>> {
        let mut ready = None;
        self.step_into(work, cancel, &mut ready)?
            .into_build_step(ready)
    }

    /// Publishes into an empty caller-provided slot without returning the record. An
    /// occupied slot is rejected before any state, work or retained storage changes.
    /// Yield, completion and errors leave an initially empty slot empty. Fails as
    /// [`Self::step`] does; also when `ready` is occupied.
    pub fn step_into(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        ready: &mut Option<AvcConfiguration>,
    ) -> Result<ConfigProgress> {
        self.step_into_with(work, cancel, ready, |record| record)
    }

    pub(crate) const fn source(&self) -> AvcConfigSource {
        self.source
    }

    pub(crate) const fn failure(&self) -> Option<Error> {
        self.failure
    }

    // One record walk serves the strict and the receiver slot; `publish` makes the
    // slot's value from the record directly.
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
        let result = self.advance(work, cancel).map(|walked| match walked {
            // A record no longer held was published already.
            Walked::Ready(prefix) => match self.record.take() {
                Some(Record { raw, nals }) => {
                    *ready = Some(wrap(AvcConfiguration {
                        raw,
                        source: self.source,
                        fields: AvcConfigFields {
                            prefix,
                            extension: self.extension,
                        },
                        width: LengthSize::from_minus_one(prefix[4]),
                        arrays: self.arrays.clone(),
                        nals,
                    }));
                    ConfigProgress::Ready
                }
                None => ConfigProgress::Complete,
            },
            Walked::Yield => ConfigProgress::Yield,
            Walked::Complete => ConfigProgress::Complete,
        });
        if let Err(error) = result
            && !retryable(error)
        {
            self.failure = Some(error);
            self.record = None;
        }
        result
    }

    #[inline(never)]
    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Walked> {
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
    ) -> Result<Walked> {
        let Some(record) = &mut self.record else {
            // Published: a set `cancel` still fails the call, as it does a walk.
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            return Ok(Walked::Complete);
        };
        let raw: &[u8] = record.raw.as_ref();
        let pool = record.raw.budget();
        let nals = &mut record.nals;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(Walked::Yield);
            }
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
                    if prefix[4] & 3 == 2 && !self.receiver {
                        return Err(Error::Invalid("AVC three-byte sample length"));
                    }
                    self.phase = Phase::Count(prefix);
                }
                Phase::Count(prefix) | Phase::Fill(prefix) => {
                    let counting = matches!(self.phase, Phase::Count(_));
                    let mut next = *scan;
                    let event = next.next(raw, self.limits, self.receiver)?;
                    match event {
                        Event::Group {
                            kind,
                            start,
                            extension,
                        } if counting => {
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
                            if extension.is_some() {
                                self.extension = extension;
                            }
                        }
                        Event::GroupEnd(end) if counting => {
                            let slot = &mut self.arrays[self.array_count - 1];
                            slot.range.end = u32::try_from(end).map_err(|_| Error::Overflow)?;
                            // Each group consumed its literal count (at most255).
                            slot.nal_count = (self.total - usize::from(slot.first_nal)) as u8;
                        }
                        Event::Nal {
                            kind,
                            payload,
                            header: nal_header,
                        } => {
                            if counting {
                                let count = self.total.checked_add(1).ok_or(Error::Overflow)?;
                                check_limit(count, self.limits.maximum_nals)?;
                                if let AvcNalStorage::Inline { data, len } = nals
                                    && let Some(slot) = data.get_mut(self.total)
                                {
                                    *slot = AvcNalDescriptor::new(&payload, kind, nal_header);
                                    *len = count as u8;
                                }
                                self.total = count;
                            } else {
                                nals.try_push(AvcNalDescriptor::new(&payload, kind, nal_header))?;
                            }
                        }
                        Event::End(end) => {
                            if counting {
                                self.understood_end = end;
                                self.phase = Phase::Reserve(prefix);
                            } else {
                                if end != self.understood_end || nals.len() != self.total {
                                    return Err(Error::Invalid("AVC second walk"));
                                }
                                return Ok(Walked::Ready(prefix));
                            }
                        }
                        _ => {}
                    }
                    *scan = next;
                }
                Phase::Reserve(prefix) => {
                    nals.reserve(pool, self.total)?;

                    // A complete small record can reuse the first walk's facts.
                    // Charge the group, NAL and end events of the usual fill.
                    let fill_work = (2 * self.array_count + self.total + 1) as u64;
                    if work.remaining() >= fill_work && self.total <= INLINE_NALS {
                        work.take(fill_work);
                        if cancel.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        if nals.len() != self.total {
                            return Err(Error::Invalid("AVC second walk"));
                        }
                        return Ok(Walked::Ready(prefix));
                    }
                    nals.clear_inline();
                    *scan = Scan::new();
                    self.phase = Phase::Fill(prefix);
                }
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
        // The scan checked the end against the two-byte length as coded.
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

type AvcNalStorage = InlineStorage<AvcNalDescriptor, INLINE_NALS>;

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
    /// The whole record as it was given to the parser, trailing bytes included.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// The origin given to the parser.
    pub const fn source(&self) -> AvcConfigSource {
        self.source
    }
    /// The record's fixed fields.
    pub const fn fields(&self) -> AvcConfigFields {
        self.fields
    }
    /// Width of the length field before each NAL unit of a sample:
    /// `lengthSizeMinusOne + 1` bytes.
    pub const fn sample_length_size(&self) -> LengthSize {
        self.width
    }
    /// Offset in the record, in bytes, of the end of its last array. What follows
    /// is [`Self::trailing_bytes`].
    pub const fn understood_end(&self) -> usize {
        self.arrays[self.array_count() - 1].range.end as usize
    }
    /// Number of NAL units in the record, over all its arrays.
    pub fn nal_count(&self) -> usize {
        self.nals.len()
    }
    pub(crate) fn base_parameter_set_counts(&self) -> (usize, usize) {
        (
            usize::from(self.arrays[0].nal_count),
            usize::from(self.arrays[1].nal_count),
        )
    }
    pub(crate) fn annex_b_configuration_len(&self) -> usize {
        let picture = &self.arrays[1];
        let count = picture.nal_indices().end;
        // The first two arrays occupy the bytes after the six-byte header,
        // plus the picture count. Replace each two-byte length with four
        // start-code bytes. The extension array is never emitted here.
        picture.range.end as usize - 7 + 2 * count
    }
    /// Number of arrays: 2, or 3 when the record has the extension with its
    /// sequence parameter set extensions.
    pub const fn array_count(&self) -> usize {
        if self.fields.extension.is_some() {
            3
        } else {
            2
        }
    }
    /// NAL unit `index` in record order, counted across the arrays; `None` from
    /// [`Self::nal_count`] on.
    pub fn nal(&self, index: usize) -> Option<AvcConfigNal<'_>> {
        self.nals
            .as_slice()
            .get(index)
            .map(|descriptor| AvcConfigNal {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    /// Every NAL unit in record order: the sequence parameter sets, the picture
    /// parameter sets, then any sequence parameter set extensions.
    pub fn nals(
        &self,
    ) -> impl ExactSizeIterator<Item = AvcConfigNal<'_>> + DoubleEndedIterator + '_ {
        self.nals
            .as_slice()
            .iter()
            .map(move |descriptor| AvcConfigNal {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    /// Array `index` in record order; `None` from [`Self::array_count`] on.
    pub fn array(&self, index: usize) -> Option<AvcConfigArray<'_>> {
        self.arrays[..self.array_count()]
            .get(index)
            .map(|descriptor| AvcConfigArray {
                raw: &self.raw,
                source: self.source,
                descriptor,
            })
    }
    /// The arrays in record order: sequence, picture, then the extension's when
    /// the record has it.
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
    /// The bytes of the record after its last array, which the parser does not
    /// interpret; empty when the record ends there.
    pub fn trailing_bytes(&self) -> &[u8] {
        &self.raw[self.understood_end()..]
    }
    /// [`Self::trailing_bytes`] as an owner that shares the record's allocation and
    /// keeps all of it charged. Fails only for a range outside the record, which a
    /// parsed record rules out.
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
    /// Heap descriptor capacity only; arrays and up to four NALs are inline. Fails when
    /// the size in bytes overflows `usize`.
    pub fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.nals
            .heap_capacity()
            .checked_mul(size_of::<AvcNalDescriptor>())
            .ok_or(Error::Overflow)
    }
    /// Full hidden raw backing plus actual descriptor heap capacities. Fails when the
    /// sum in bytes overflows `usize`.
    pub fn retained_storage_bytes(&self) -> Result<usize> {
        self.descriptor_storage_bytes()?
            .checked_add(self.raw.retained_storage_bytes())
            .ok_or(Error::Overflow)
    }
}

/// One NAL unit of an `avcC` record, borrowed from its [`AvcConfiguration`].
///
/// A NAL borrow cannot escape its configuration; retaining it is explicit.
///
/// ```compile_fail,E0505
/// use triergon_framing::avc::AvcConfiguration;
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
/// use triergon_framing::avc::AvcConfiguration;
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
    /// The NAL unit, header byte included, without its length field.
    pub fn source_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.payload()]
    }
    /// The two-byte big-endian length field stored before the NAL unit.
    pub fn length_field_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.length_field()]
    }
    /// Where [`Self::source_bytes`] lies in the record, in bytes from its start.
    pub fn source_range(&self) -> Range<usize> {
        self.descriptor.payload()
    }
    /// The origin of the record.
    pub const fn source(&self) -> AvcConfigSource {
        self.source
    }
    /// The array that holds the NAL unit.
    pub const fn array_kind(&self) -> AvcConfigArrayKind {
        self.descriptor.kind
    }
    /// The NAL unit's header: its `nal_ref_idc` and `nal_unit_type`, which the
    /// array it is in does not constrain.
    pub const fn header(&self) -> NalHeader {
        self.descriptor.header
    }
}

/// One NAL unit array of an `avcC` record, borrowed from its [`AvcConfiguration`].
///
/// An array borrow is tied to the configuration in the same way.
///
/// ```compile_fail,E0505
/// use triergon_framing::avc::AvcConfiguration;
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
    /// The array as stored: its count byte, then each NAL unit after its length
    /// field. The extension's array starts three bytes earlier, with the chroma
    /// format and bit depth bytes.
    pub fn source_bytes(&self) -> &'a [u8] {
        &self.raw[self.descriptor.source_range()]
    }
    /// The origin of the record.
    pub const fn source(&self) -> AvcConfigSource {
        self.source
    }
    /// Which array this is.
    pub const fn kind(&self) -> AvcConfigArrayKind {
        self.descriptor.kind
    }
    /// Where [`Self::source_bytes`] lies in the record, in bytes from its start.
    pub fn source_range(&self) -> Range<usize> {
        self.descriptor.source_range()
    }
    /// The indices of the array's NAL units for [`AvcConfiguration::nal`].
    pub fn nal_indices(&self) -> Range<usize> {
        self.descriptor.nal_indices()
    }
    /// Offset in the record of the byte that holds the array's NAL unit count. For
    /// the sequence parameter sets its low five bits are the count.
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
