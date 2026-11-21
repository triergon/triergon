//! The core frame header of a canonical DTS stream field by field (ETSI TS 102 114
//! V1.6.1 §3.4, §5.3, §5.4.2): the source bits of each field and the constraints
//! the header breaks. An inspection says nothing about the frame body or whether a
//! decoder accepts the stream.
use super::DtsSourceIdentity;
use super::core_frame::{BITRATES, CHANNELS, CORE_SYNC, EXTENSION_SYNC, core_rate};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_bitstream::bits_at;
use triergon_core::{Budget, Bytes, Error, Result, SourceLocation, Step, Time, WorkBudget};

const FIELD_COUNT: usize = 27;

/// A field of the core frame header (ETSI TS 102 114 V1.6.1 clause 5.3,
/// Table 5-1). The variants are in bitstream order, including the conditional
/// `HCRC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DtsCoreField {
    /// `SYNC`, 32 bits: the sync word, `0x7FFE8001` for a core frame.
    Sync,
    /// `FTYPE`, 1 bit: 1 for a normal frame, 0 for a termination frame.
    Normal,
    /// `SHORT`, 5 bits: the deficit sample count, 31 in a normal frame.
    Short,
    /// `CPF`, 1 bit: whether the header carries `HCRC`.
    CrcPresent,
    /// `NBLKS`, 7 bits: the number of 32-sample blocks per channel, minus one.
    BlockCount,
    /// `FSIZE`, 14 bits: the frame size in bytes, minus one.
    FrameSize,
    /// `AMODE`, 6 bits: the audio channel arrangement code.
    AudioMode,
    /// `SFREQ`, 4 bits: the core sampling frequency code.
    SampleRate,
    /// `RATE`, 5 bits: the transmission bit rate code.
    Rate,
    /// `FixedBit`, 1 bit: reserved, 0.
    Fixed,
    /// `DYNF`, 1 bit: the embedded dynamic range flag.
    Dynamic,
    /// `TIMEF`, 1 bit: the embedded time stamp flag.
    Timestamp,
    /// `AUXF`, 1 bit: the auxiliary data flag.
    Auxiliary,
    /// `HDCD`, 1 bit: the HDCD source flag.
    Hdcd,
    /// `EXT_AUDIO_ID`, 3 bits: the extension audio descriptor.
    ExtensionId,
    /// `EXT_AUDIO`, 1 bit: whether the frame carries an extension.
    Extension,
    /// `ASPF`, 1 bit: the audio sync word insertion flag.
    AudioSync,
    /// `LFF`, 2 bits: the low frequency effects flag.
    Lfe,
    /// `HFLAG`, 1 bit: the predictor history flag.
    History,
    /// `HCRC`, 16 bits: the header CRC word, in the header only when `CPF` is 1.
    Hcrc,
    /// `FILTS`, 1 bit: the multirate interpolator switch.
    Filter,
    /// `VERNUM`, 4 bits: the encoder software revision.
    Version,
    /// `CHIST`, 2 bits: the copy history.
    CopyHistory,
    /// `PCMR`, 3 bits: the source PCM resolution code.
    PcmResolution,
    /// `SUMF`, 1 bit: the front sum/difference flag.
    SumFront,
    /// `SUMS`, 1 bit: the surround sum/difference flag.
    SumSurround,
    /// `DIALNORM`, 4 bits: the dialog normalization for `VERNUM` 6 and 7,
    /// unspecified for other versions.
    DialogWord,
}

impl DtsCoreField {
    /// Every field, in bitstream order.
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

    /// Width of the field in bits, 1 to 32.
    pub const fn width(self) -> u8 {
        const WIDTHS: [u8; FIELD_COUNT] = [
            32, 1, 5, 1, 7, 14, 6, 4, 5, 1, 1, 1, 1, 1, 3, 1, 1, 2, 1, 16, 1, 4, 2, 3, 1, 1, 4,
        ];
        WIDTHS[self as usize]
    }
}

/// What the source holds of one header field. When the source ends inside a
/// field, `prefix` holds exactly `observed_bits` bits, not shifted or zero-filled
/// to the declared width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldObservation {
    /// No bit of the field was read: the source ends before it, or a
    /// [`DtsHeaderInspector`] stopped at a sync word that is not the core sync
    /// word.
    NotReached,
    /// The field is not in this header: `HCRC` when `CPF` is 0, also when the
    /// source ends before the place `HCRC` would have. While `CPF` is unknown
    /// `HCRC` is `NotReached`; no missing value is filled with zero.
    NotInSyntax,
    /// At least one bit of the field was read.
    Bits {
        /// Offset of the field's first bit from the start of the inspected bytes,
        /// in bits.
        offset: u16,
        /// Width of the complete field in bits, 1 to 32.
        width: u8,
        /// How many of the field's bits the source holds: `width` for a complete
        /// field, 1 to `width - 1` when the source ends inside it.
        observed_bits: u8,
        /// The observed bits as an unsigned integer, the first bit most
        /// significant: the field's value when it is complete.
        prefix: u32,
    },
}

impl FieldObservation {
    /// The value of a complete field; `None` when it is partial, not reached or
    /// not in the header.
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

    /// The bits read for the field, as bit offsets from the start of the
    /// inspected bytes: `offset..offset + observed_bits`. `None` when no bit was
    /// read.
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

/// The constraints of the specification that an inspected header breaks, as a
/// set of the flags below. Each flag is set from the complete fields it names,
/// independently of the others. None is relaxed to match a more permissive
/// decoder. An empty set does not say that a decoder can use the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsInspectionIssues(u32);

impl DtsInspectionIssues {
    /// The source ends before the last header field is complete. Not set when
    /// the sync word is complete and is not the core sync word.
    pub const TRUNCATED: u32 = 1;
    /// The first 32 bits are not the core sync word `0x7FFE8001`.
    pub const NONCANONICAL_SYNC: u32 = 1 << 1;
    /// The reserved bit after `RATE` is 1.
    pub const FIXED_BIT: u32 = 1 << 2;
    /// `FSIZE` is below 95.
    pub const FRAME_SIZE: u32 = 1 << 3;
    /// `NBLKS` is below 5, or a normal frame has an `NBLKS` other than 7, 15,
    /// 31, 63 and 127.
    pub const BLOCK_COUNT: u32 = 1 << 4;
    /// `SFREQ` is a code without a sampling frequency.
    pub const SAMPLE_RATE_CODE: u32 = 1 << 5;
    /// `RATE` is neither a nominal bit rate, 0 to 24, nor 29, the open rate.
    pub const BITRATE_CODE: u32 = 1 << 6;
    /// `LFF` is 3.
    pub const LFE_CODE: u32 = 1 << 7;
    /// `PCMR` is 4 or 7.
    pub const PCM_RESOLUTION: u32 = 1 << 8;
    /// A normal frame has a `SHORT` other than 31.
    pub const NORMAL_SHORT: u32 = 1 << 9;
    /// `FTYPE` is 0: a termination frame.
    pub const TERMINATION: u32 = 1 << 10;
    /// `VERNUM` is above 7.
    pub const FUTURE_VERSION: u32 = 1 << 11;
    /// `EXT_AUDIO` is 1 with an `EXT_AUDIO_ID` other than 0, 2 and 6.
    pub const UNSUPPORTED_EXTENSION: u32 = 1 << 12;
    /// The first 32 bits are the extension substream sync word `0x64582025`.
    /// Set together with [`NONCANONICAL_SYNC`](Self::NONCANONICAL_SYNC).
    pub const EXTENSION_SUBSTREAM: u32 = 1 << 13;

    /// The set as a mask of the flags above.
    pub const fn bits(self) -> u32 {
        self.0
    }
    /// Whether no flag is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    /// Whether every flag in the mask `flags` is set.
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

/// The values an inspected header's complete fields stand for. Each is `None`
/// while a field it needs is unknown or incomplete, or holds a code without a
/// value. Even a present value does not validate the rest of the frame or its
/// coded audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsInspectionViews {
    /// `FSIZE + 1`, the declared frame size in bytes: 1 to 16384, computed also
    /// for the invalid sizes below 96.
    pub declared_frame_bytes: Option<u32>,
    /// Core sampling frequency in Hz, for an `SFREQ` code that has one: 8000,
    /// 11025, 12000, 16000, 22050, 24000, 32000, 44100 or 48000.
    pub core_rate_hz: Option<u32>,
    /// Core samples per channel from a complete NBLKS in 5..=127, before any
    /// termination adjustment. Normal-frame restrictions apply to `normal_window`.
    pub coded_window_samples: Option<u32>,
    /// Number of full-bandwidth channels, 1 to 8, for `AMODE` 0 to 15; `None`
    /// for the arrangement codes above 15. The LFE channel is not counted.
    pub full_bandwidth_channels: Option<u8>,
    /// Bit rate in bits per second for `RATE` 0 to 24, 32000 to 1536000; `None`
    /// for every other code.
    pub nominal_bitrate: Option<u32>,
    /// Whether `RATE` is 29, the open bit rate; `None` while `RATE` is
    /// incomplete.
    pub open_bitrate: Option<bool>,
    /// Duration of a normal frame, `coded_window_samples / core_rate_hz`
    /// seconds: present only for `FTYPE` 1, `SHORT` 31, `NBLKS` 7, 15, 31, 63 or
    /// 127, an `SFREQ` code with a frequency and `VERNUM` 0 to 7. No time is
    /// made up for a termination frame or a later version.
    pub normal_window: Option<Time>,
    /// Dialog normalization gain in dB: `-(16 + DIALNORM)` for `VERNUM` 6 and
    /// `-DIALNORM` for `VERNUM` 7. `None` for the other versions, whose
    /// `DIALNORM` bits are unspecified.
    pub dialog_gain_db: Option<i8>,
}

/// The class of an inspected header, the first of these that applies: an
/// extension substream sync word (`Unsupported`), another sync word than the
/// core one (`Invalid`), a header the source ends inside (`Truncated`), a field
/// the specification rules out (`Invalid`), a termination frame, later version
/// or reserved extension (`Unsupported`), and otherwise `NormalHeader`.
/// `NormalHeader` validates neither the rest of the frame nor its audio, and
/// does not say that a decoder accepts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsInspectionSourceStatus {
    /// A complete header of a normal core frame without any issue.
    NormalHeader,
    /// The source ends before the header is complete.
    Truncated,
    /// The sync word is neither the core nor the extension substream sync word,
    /// or a field holds a value the specification rules out.
    Invalid,
    /// An extension substream, a termination frame, `VERNUM` above 7 or a
    /// reserved extension code.
    Unsupported,
}

/// The result of a [`DtsHeaderInspector`]: what the record holds of every header
/// field, the constraints the header breaks, the values its fields stand for,
/// and the record itself.
#[derive(Clone, Debug)]
pub struct DtsInspection {
    raw: Bytes,
    identity: DtsSourceIdentity,
    fields: [FieldObservation; FIELD_COUNT],
    issues: DtsInspectionIssues,
    views: DtsInspectionViews,
    required_bits: Option<u16>,
}

impl DtsInspection {
    /// The whole record exactly as it was given, including any bytes after the
    /// header.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// Where the record came from, as the caller stated it.
    pub const fn identity(&self) -> DtsSourceIdentity {
        self.identity
    }
    /// What the record holds of `field`.
    pub const fn field(&self, field: DtsCoreField) -> FieldObservation {
        self.fields[field as usize]
    }
    /// The constraints the header breaks.
    pub const fn issues(&self) -> DtsInspectionIssues {
        self.issues
    }
    /// The values the complete fields stand for.
    pub const fn views(&self) -> DtsInspectionViews {
        self.views
    }
    /// Length of the header in bits, sync word included: 104, or 120 with
    /// `HCRC`. `None` unless the core sync word and `CPF` are both complete.
    pub const fn required_header_bits(&self) -> Option<u16> {
        self.required_bits
    }
    /// Whether the record begins with the core sync word and holds every header
    /// field completely.
    pub fn syntax_complete(&self) -> bool {
        self.field(DtsCoreField::Sync).value() == Some(CORE_SYNC)
            && self.field(DtsCoreField::DialogWord).value().is_some()
    }
    /// The header's class, derived from [`issues`](Self::issues).
    pub const fn source_status(&self) -> DtsInspectionSourceStatus {
        self.issues.source_status()
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Read,
    Validate,
    /// The checked header, returned by the next unit of work.
    Publish(DtsInspectionViews),
}

/// Where an inspection stands. The record is held only until the result is
/// returned.
enum State {
    Inspecting { raw: Bytes, phase: Phase },
    Complete,
    Failed(Error),
}

/// Reads the core header fields of one record the caller owns, bit by bit, and
/// reports each field with its source bits and every constraint the header
/// breaks, where [`probe_core_header`](crate::dts::probe_core_header) refuses
/// the header at the first one. It inspects at most 120 bits. The result keeps
/// every byte of the record as it is, including any after the header.
///
/// One bit costs one unit of work, and so do an absent `HCRC`, the validation
/// and the result. A step without work does nothing; one unit always makes
/// progress. The inspector allocates and copies nothing, so a full pool never
/// delays the result. An error, cancellation included, is returned again by
/// every later step.
pub struct DtsHeaderInspector {
    identity: DtsSourceIdentity,
    fields: [FieldObservation; FIELD_COUNT],
    current: usize,
    position: u16,
    field_start: u16,
    source_bits: u16,
    record_end: SourceLocation,
    issues: DtsInspectionIssues,
    state: State,
}

impl std::fmt::Debug for DtsHeaderInspector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failure = match self.state {
            State::Failed(error) => Some(error),
            State::Inspecting { .. } | State::Complete => None,
        };
        f.debug_struct("DtsHeaderInspector")
            .field("identity", &self.identity)
            .field("position", &self.position)
            .field("source_bits", &self.source_bits)
            .field("issues", &self.issues)
            .field("failure", &failure)
            .finish_non_exhaustive()
    }
}

impl DtsHeaderInspector {
    /// An inspector of `source`, a record that begins where a frame's sync word is
    /// expected and that `identity` locates. `source` must be charged to `pool`. Bytes
    /// after the header are not read. Fails with `ForeignPool` when `source` is charged
    /// to another pool; `Invalid` when `maximum_record_bytes` is zero, `Limit` when the
    /// record is longer than that; `Overflow` when the record ends beyond offset
    /// `u64::MAX`.
    pub fn new(
        pool: &Budget,
        identity: DtsSourceIdentity,
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
            issues: DtsInspectionIssues(0),
            state: State::Inspecting {
                raw: source,
                phase: Phase::Read,
            },
        })
    }

    /// Advances the inspection. `Ready` occurs once, then `End` at the record's end:
    /// the end of the owned record, not an observed end of the transport. An owned
    /// record never requests input. Fails with `Cancelled` when `cancel` is set. A
    /// header that breaks the specification is not an error: it is reported in the
    /// result.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsInspection>> {
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result {
            self.state = State::Failed(error);
        }
        result
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsInspection>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let (raw, phase) = match &mut self.state {
                State::Inspecting { raw, phase } => (raw, phase),
                State::Complete => return Ok(Step::End(self.record_end)),
                &mut State::Failed(error) => return Err(error),
            };
            match phase {
                Phase::Read => {
                    if self.current == FIELD_COUNT {
                        *phase = Phase::Validate;
                        continue;
                    }
                    let field = DtsCoreField::ALL[self.current];
                    if field == DtsCoreField::Hcrc
                        && value(&self.fields, DtsCoreField::CrcPresent) == Some(0)
                    {
                        if !work.try_charge(1) {
                            return Ok(Step::Yield);
                        }
                        self.fields[self.current] = FieldObservation::NotInSyntax;
                        self.current += 1;
                        continue;
                    }
                    if self.position == self.source_bits {
                        *phase = Phase::Validate;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.position += 1;
                    let width = field.width();
                    let observed = self.position - self.field_start;
                    if observed < u16::from(width) && self.position < self.source_bits {
                        continue;
                    }
                    // A budget yield preserves the cursor. Extract the prefix once,
                    // when the field is complete or the source ends inside it.
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
                            *phase = Phase::Validate;
                        }
                    }
                }
                Phase::Validate => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    // Conditional absence is known from CPF source bits even if
                    // an earlier partial field stopped the incremental walk.
                    if value(&self.fields, DtsCoreField::CrcPresent) == Some(0) {
                        self.fields[DtsCoreField::Hcrc as usize] = FieldObservation::NotInSyntax;
                    }
                    let (issues, views) = validate_header(|field| value(&self.fields, field))?;
                    self.issues = issues;
                    *phase = Phase::Publish(views);
                }
                Phase::Publish(_) => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let required_bits = required_header_bits(
                        value(&self.fields, DtsCoreField::Sync),
                        value(&self.fields, DtsCoreField::CrcPresent),
                    );
                    let State::Inspecting {
                        raw,
                        phase: Phase::Publish(views),
                    } = std::mem::replace(&mut self.state, State::Complete)
                    else {
                        unreachable!("the inspection is checked")
                    };
                    return Ok(Step::Ready(DtsInspection {
                        raw,
                        identity: self.identity,
                        fields: self.fields,
                        issues: self.issues,
                        views,
                        required_bits,
                    }));
                }
            }
        }
    }
}

/// The value observed for `field`, when all its bits were read.
fn value(fields: &[FieldObservation; FIELD_COUNT], field: DtsCoreField) -> Option<u32> {
    fields[field as usize].value()
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

fn bitrate(code: u32) -> Option<u32> {
    usize::try_from(code)
        .ok()
        .and_then(|index| BITRATES.get(index).copied())
}
