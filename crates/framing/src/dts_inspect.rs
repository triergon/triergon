//! Canonical DTS header source inspection from ETSI TS 102 114
//! V1.6.1 clauses 3.4/5.3/5.4.2. This reports source bits and validation issues;
//! publication is not bitstream acceptance, framing, decoder support or parity.
#![forbid(unsafe_code)]

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_bitstream::bits_at;
use triergon_core::{Budget, Bytes, Error, Result, SourceLocation, Step, Time, WorkBudget};

pub(super) const CORE_SYNC: u32 = 0x7ffe8001;
const EXTENSION_SYNC: u32 = 0x64582025;
const FIELD_COUNT: usize = 27;

/// Order is the primary bitstream-header syntax, including conditional HCRC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DtsCoreField {
    Sync,
    Normal,
    Short,
    CrcPresent,
    BlockCount,
    FrameSize,
    AudioMode,
    SampleRate,
    Rate,
    Fixed,
    Dynamic,
    Timestamp,
    Auxiliary,
    Hdcd,
    ExtensionId,
    Extension,
    AudioSync,
    Lfe,
    History,
    Hcrc,
    Filter,
    Version,
    CopyHistory,
    PcmResolution,
    SumFront,
    SumSurround,
    DialogWord,
}

impl DtsCoreField {
    pub const ALL: [Self; FIELD_COUNT] = [
        Self::Sync,
        Self::Normal,
        Self::Short,
        Self::CrcPresent,
        Self::BlockCount,
        Self::FrameSize,
        Self::AudioMode,
        Self::SampleRate,
        Self::Rate,
        Self::Fixed,
        Self::Dynamic,
        Self::Timestamp,
        Self::Auxiliary,
        Self::Hdcd,
        Self::ExtensionId,
        Self::Extension,
        Self::AudioSync,
        Self::Lfe,
        Self::History,
        Self::Hcrc,
        Self::Filter,
        Self::Version,
        Self::CopyHistory,
        Self::PcmResolution,
        Self::SumFront,
        Self::SumSurround,
        Self::DialogWord,
    ];

    pub const fn width(self) -> u8 {
        const WIDTHS: [u8; FIELD_COUNT] = [
            32, 1, 5, 1, 7, 14, 6, 4, 5, 1, 1, 1, 1, 1, 3, 1, 1, 2, 1, 16, 1, 4, 2, 3, 1, 1, 4,
        ];
        WIDTHS[self as usize]
    }
}

/// A partial prefix integer has exactly `observed_bits` bits. It has not been
/// shifted/zero-filled to the declared width. `value()` is None until complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldObservation {
    NotReached,
    /// HCRC is absent once source CPF=0 is observed, even on an earlier truncation.
    /// Unknown CPF remains NotReached; no missing value is filled with zero.
    NotInSyntax,
    Bits {
        offset: u16,
        width: u8,
        observed_bits: u8,
        prefix: u32,
    },
}

impl FieldObservation {
    pub const fn value(self) -> Option<u32> {
        match self {
            Self::Bits {
                width,
                observed_bits,
                prefix,
                ..
            } if width == observed_bits => Some(prefix),
            _ => None,
        }
    }

    pub fn bit_range(self) -> Option<Range<u16>> {
        match self {
            Self::Bits {
                offset,
                observed_bits,
                ..
            } => Some(offset..offset.checked_add(u16::from(observed_bits))?),
            _ => None,
        }
    }
}

/// Issues describe independently observed source constraints. None is silently
/// relaxed to reproduce native permissiveness. No decoder-ready flag is exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsInspectionIssues(u32);

impl DtsInspectionIssues {
    pub const TRUNCATED: u32 = 1;
    pub const NONCANONICAL_SYNC: u32 = 1 << 1;
    pub const FIXED_BIT: u32 = 1 << 2;
    pub const FRAME_SIZE: u32 = 1 << 3;
    pub const BLOCK_COUNT: u32 = 1 << 4;
    pub const SAMPLE_RATE_CODE: u32 = 1 << 5;
    pub const BITRATE_CODE: u32 = 1 << 6;
    pub const LFE_CODE: u32 = 1 << 7;
    pub const PCM_RESOLUTION: u32 = 1 << 8;
    pub const NORMAL_SHORT: u32 = 1 << 9;
    pub const TERMINATION: u32 = 1 << 10;
    pub const FUTURE_VERSION: u32 = 1 << 11;
    pub const UNSUPPORTED_EXTENSION: u32 = 1 << 12;
    pub const EXTENSION_SUBSTREAM: u32 = 1 << 13;

    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub const fn contains(self, flags: u32) -> bool {
        self.0 & flags == flags
    }

    pub(super) const fn source_status(self) -> DtsInspectionSourceStatus {
        use DtsInspectionIssues as I;
        let bits = self.bits();
        if bits & I::EXTENSION_SUBSTREAM != 0 {
            DtsInspectionSourceStatus::Unsupported
        } else if bits & I::NONCANONICAL_SYNC != 0 {
            DtsInspectionSourceStatus::Invalid
        } else if bits & I::TRUNCATED != 0 {
            DtsInspectionSourceStatus::Truncated
        } else if bits
            & (I::FIXED_BIT
                | I::FRAME_SIZE
                | I::BLOCK_COUNT
                | I::SAMPLE_RATE_CODE
                | I::BITRATE_CODE
                | I::LFE_CODE
                | I::PCM_RESOLUTION
                | I::NORMAL_SHORT)
            != 0
        {
            DtsInspectionSourceStatus::Invalid
        } else if bits & (I::TERMINATION | I::FUTURE_VERSION | I::UNSUPPORTED_EXTENSION) != 0 {
            DtsInspectionSourceStatus::Unsupported
        } else {
            DtsInspectionSourceStatus::NormalHeader
        }
    }
}

/// Caller-supplied identity for an already-owned record. This API does not
/// establish transport/request freshness; it preserves the supplied generation.
pub type DtsInspectionIdentity = crate::DtsSourceIdentity;

/// Separately named current-syntax views. Unknown or incomplete fields remain
/// None. Even a present view is not complete-carriage/coded-audio validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsInspectionViews {
    /// Arithmetic view of coded FSIZE+1, including invalid undersized codes.
    pub declared_frame_bytes: Option<u32>,
    pub core_rate_hz: Option<u32>,
    /// Core samples per channel from a complete NBLKS in 5..=127, before any
    /// termination adjustment. Normal-frame restrictions apply to `normal_window`.
    pub coded_window_samples: Option<u32>,
    pub full_bandwidth_channels: Option<u8>,
    pub nominal_bitrate: Option<u32>,
    pub open_bitrate: Option<bool>,
    /// Only normal current-version headers with normal SHORT/NBLKS and a known
    /// rate have this view. No termination or future-version time is invented.
    pub normal_window: Option<Time>,
    /// Defined here only for versions6/7, not unspecified placeholder words.
    pub dialog_gain_db: Option<i8>,
}

/// Classification under the existing canonical normal-header source contract.
/// NormalHeader certifies neither full carriage nor audio/decoder validity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsInspectionSourceStatus {
    NormalHeader,
    Truncated,
    Invalid,
    Unsupported,
}

#[derive(Clone, Debug)]
pub struct DtsInspection {
    raw: Bytes,
    identity: DtsInspectionIdentity,
    fields: [FieldObservation; FIELD_COUNT],
    issues: DtsInspectionIssues,
    views: DtsInspectionViews,
    required_bits: Option<u16>,
}

impl DtsInspection {
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub const fn identity(&self) -> DtsInspectionIdentity {
        self.identity
    }
    pub const fn field(&self, field: DtsCoreField) -> FieldObservation {
        self.fields[field as usize]
    }
    pub const fn issues(&self) -> DtsInspectionIssues {
        self.issues
    }
    pub const fn views(&self) -> DtsInspectionViews {
        self.views
    }
    pub const fn required_header_bits(&self) -> Option<u16> {
        self.required_bits
    }
    pub fn syntax_complete(&self) -> bool {
        self.field(DtsCoreField::Sync).value() == Some(CORE_SYNC)
            && self.field(DtsCoreField::DialogWord).value().is_some()
    }
    pub const fn source_status(&self) -> DtsInspectionSourceStatus {
        self.issues.source_status()
    }
}

/// Core step protocol. Ready occurs once, then End at the supplied record end.
/// Owned-record inspection never requests external input. End is record
/// completion, not a transport-EOF observation.
pub type DtsInspectionStep = Step<DtsInspection>;

#[derive(Clone, Copy)]
enum Phase {
    Read,
    Validate,
    Publish,
    Complete,
}

/// Inspects at most120 bits of a bounded complete source prefix/record. Every
/// source byte is retained unchanged, including any tail beyond the header.
/// All metadata has fixed inline size. Steps allocate/copy no dynamic storage;
/// full-pool pressure therefore needs no retry allocation to publish a report.
/// Zero work does nothing, work1 progresses, and cancellation is sticky.
pub struct DtsHeaderInspector {
    raw: Option<Bytes>,
    identity: DtsInspectionIdentity,
    fields: [FieldObservation; FIELD_COUNT],
    current: usize,
    position: u16,
    field_start: u16,
    source_bits: u16,
    record_end: SourceLocation,
    phase: Phase,
    issues: DtsInspectionIssues,
    views: Option<DtsInspectionViews>,
    failure: Option<Error>,
}

impl DtsHeaderInspector {
    pub fn new(
        pool: &Budget,
        identity: DtsInspectionIdentity,
        source: Bytes,
        maximum_record_bytes: usize,
    ) -> Result<Self> {
        if !source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if maximum_record_bytes == 0 {
            return Err(Error::Invalid("zero DTS inspection byte policy"));
        }
        if source.len() > maximum_record_bytes {
            return Err(Error::Limit {
                requested: source.len(),
                maximum: maximum_record_bytes,
            });
        }
        let end = identity
            .offset
            .checked_add(u64::try_from(source.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let source_bits = u16::try_from(source.len().min(15) * 8).map_err(|_| Error::Overflow)?;
        Ok(Self {
            raw: Some(source),
            identity,
            fields: [FieldObservation::NotReached; FIELD_COUNT],
            current: 0,
            position: 0,
            field_start: 0,
            source_bits,
            record_end: SourceLocation {
                source: identity.source,
                offset: end,
            },
            phase: Phase::Read,
            issues: DtsInspectionIssues(0),
            views: None,
            failure: None,
        })
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DtsInspectionStep> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result {
            self.failure = Some(error);
            self.raw = None;
            self.views = None;
        }
        result
    }

    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<DtsInspectionStep> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Complete => return Ok(DtsInspectionStep::End(self.record_end)),
                Phase::Read => {
                    if self.current == FIELD_COUNT {
                        self.phase = Phase::Validate;
                        continue;
                    }
                    let field = DtsCoreField::ALL[self.current];
                    if field == DtsCoreField::Hcrc
                        && self.value(DtsCoreField::CrcPresent) == Some(0)
                    {
                        if !work.try_charge(1) {
                            return Ok(DtsInspectionStep::Yield);
                        }
                        self.fields[self.current] = FieldObservation::NotInSyntax;
                        self.current += 1;
                        continue;
                    }
                    if self.position == self.source_bits {
                        self.phase = Phase::Validate;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(DtsInspectionStep::Yield);
                    }
                    self.position += 1;
                    let width = field.width();
                    let observed = self.position - self.field_start;
                    if observed < u16::from(width) && self.position < self.source_bits {
                        continue;
                    }
                    // A budget yield preserves the cursor. Extract the prefix once,
                    // when the field is complete or the source ends inside it.
                    let raw = self
                        .raw
                        .as_ref()
                        .ok_or(Error::Invalid("missing DTS inspection source"))?;
                    let prefix = bits_at(
                        raw.as_ref(),
                        usize::from(self.field_start),
                        u32::from(observed),
                    )
                    .ok_or(Error::Truncated)?;
                    self.fields[self.current] = FieldObservation::Bits {
                        offset: self.field_start,
                        width,
                        observed_bits: observed as u8,
                        prefix,
                    };
                    self.field_start = self.position;
                    if observed == u16::from(width) {
                        self.current += 1;
                        if field == DtsCoreField::Sync && prefix != CORE_SYNC {
                            self.phase = Phase::Validate;
                        }
                    }
                }
                Phase::Validate => {
                    if !work.try_charge(1) {
                        return Ok(DtsInspectionStep::Yield);
                    }
                    // Conditional absence is known from CPF source bits even if
                    // an earlier partial field stopped the incremental walk.
                    if self.value(DtsCoreField::CrcPresent) == Some(0) {
                        self.fields[DtsCoreField::Hcrc as usize] = FieldObservation::NotInSyntax;
                    }
                    let (issues, views) = validate_header(|field| self.value(field))?;
                    self.issues = issues;
                    self.views = Some(views);
                    self.phase = Phase::Publish;
                }
                Phase::Publish => {
                    if !work.try_charge(1) {
                        return Ok(DtsInspectionStep::Yield);
                    }
                    let required_bits = required_header_bits(
                        self.value(DtsCoreField::Sync),
                        self.value(DtsCoreField::CrcPresent),
                    );
                    let output = DtsInspection {
                        raw: self
                            .raw
                            .take()
                            .ok_or(Error::Invalid("missing DTS publication source"))?,
                        identity: self.identity,
                        fields: self.fields,
                        issues: self.issues,
                        views: self
                            .views
                            .take()
                            .ok_or(Error::Invalid("missing DTS publication views"))?,
                        required_bits,
                    };
                    self.phase = Phase::Complete;
                    return Ok(DtsInspectionStep::Ready(output));
                }
            }
        }
    }

    fn value(&self, field: DtsCoreField) -> Option<u32> {
        self.fields[field as usize].value()
    }
}

pub(super) fn required_header_bits(sync: Option<u32>, cpf: Option<u32>) -> Option<u16> {
    if sync == Some(CORE_SYNC) {
        cpf.map(|v| if v == 0 { 104 } else { 120 })
    } else {
        None
    }
}

/// Interpret the complete fields available in either header representation.
pub(super) fn validate_header(
    value: impl Fn(DtsCoreField) -> Option<u32>,
) -> Result<(DtsInspectionIssues, DtsInspectionViews)> {
    use DtsCoreField as F;
    use DtsInspectionIssues as I;
    let mut issues = 0;
    let sync = value(F::Sync);
    let dialog = value(F::DialogWord);
    let frame_size = value(F::FrameSize);
    let short = value(F::Short);
    if sync.is_some_and(|value| value != CORE_SYNC) {
        issues |= I::NONCANONICAL_SYNC;
    } else if dialog.is_none() {
        issues |= I::TRUNCATED;
    }
    if sync == Some(EXTENSION_SYNC) {
        issues |= I::EXTENSION_SUBSTREAM;
    }
    if value(F::Fixed).is_some_and(|value| value != 0) {
        issues |= I::FIXED_BIT;
    }
    if frame_size.is_some_and(|value| value < 95) {
        issues |= I::FRAME_SIZE;
    }
    let normal = value(F::Normal);
    let blocks = value(F::BlockCount);
    let coded_window_samples = blocks.filter(|&n| n >= 5).map(|n| (n + 1) * 32);
    let normal_blocks = matches!(blocks, Some(7 | 15 | 31 | 63 | 127));
    if blocks.is_some() && (coded_window_samples.is_none() || (normal == Some(1) && !normal_blocks))
    {
        issues |= I::BLOCK_COUNT;
    }
    if normal == Some(1) && short.is_some_and(|value| value != 31) {
        issues |= I::NORMAL_SHORT;
    }
    if normal == Some(0) {
        issues |= I::TERMINATION;
    }
    let rate_code = value(F::SampleRate);
    let rate = rate_code.and_then(core_rate);
    if rate_code.is_some() && rate.is_none() {
        issues |= I::SAMPLE_RATE_CODE;
    }
    let bitrate_code = value(F::Rate);
    let nominal_bitrate = bitrate_code.and_then(bitrate);
    if bitrate_code.is_some_and(|value| value != 29) && nominal_bitrate.is_none() {
        issues |= I::BITRATE_CODE;
    }
    if value(F::Lfe) == Some(3) {
        issues |= I::LFE_CODE;
    }
    if value(F::PcmResolution).is_some_and(|value| !matches!(value, 0 | 1 | 2 | 3 | 5 | 6)) {
        issues |= I::PCM_RESOLUTION;
    }
    let version = value(F::Version);
    if version.is_some_and(|value| value > 7) {
        issues |= I::FUTURE_VERSION;
    }
    if value(F::Extension) == Some(1)
        && value(F::ExtensionId).is_some_and(|value| !matches!(value, 0 | 2 | 6))
    {
        issues |= I::UNSUPPORTED_EXTENSION;
    }
    let normal_window = if normal == Some(1)
        && short == Some(31)
        && normal_blocks
        && version.is_some_and(|value| value <= 7)
    {
        match (coded_window_samples, rate) {
            (Some(samples), Some(hz)) => Some(Time::new(i128::from(samples), u128::from(hz))?),
            _ => None,
        }
    } else {
        None
    };
    let dialog_gain_db = dialog.and_then(|raw| {
        let raw = i8::try_from(raw).ok()?;
        match version {
            Some(6) => Some(-16 - raw),
            Some(7) => Some(-raw),
            _ => None,
        }
    });
    const CHANNELS: [u8; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];
    let views = DtsInspectionViews {
        declared_frame_bytes: frame_size.map(|value| value + 1),
        core_rate_hz: rate,
        coded_window_samples,
        full_bandwidth_channels: value(F::AudioMode)
            .and_then(|value| usize::try_from(value).ok())
            .and_then(|index| CHANNELS.get(index).copied()),
        nominal_bitrate,
        open_bitrate: bitrate_code.map(|value| value == 29),
        normal_window,
        dialog_gain_db,
    };
    Ok((DtsInspectionIssues(issues), views))
}

fn core_rate(code: u32) -> Option<u32> {
    match code {
        1 => Some(8000),
        2 => Some(16000),
        3 => Some(32000),
        6 => Some(11025),
        7 => Some(22050),
        8 => Some(44100),
        11 => Some(12000),
        12 => Some(24000),
        13 => Some(48000),
        _ => None,
    }
}

fn bitrate(code: u32) -> Option<u32> {
    const RATES: [u32; 25] = [
        32000, 56000, 64000, 96000, 112000, 128000, 192000, 224000, 256000, 320000, 384000, 448000,
        512000, 576000, 640000, 768000, 960000, 1024000, 1152000, 1280000, 1344000, 1408000,
        1411200, 1472000, 1536000,
    ];
    usize::try_from(code)
        .ok()
        .and_then(|index| RATES.get(index).copied())
}
