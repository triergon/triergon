//! The AudioSpecificConfig of ISO/IEC 14496-3 for AAC LC, alone or under SBR or PS,
//! with its GASpecificConfig and program config element. Nothing here decodes audio.
use crate::BuildStep;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_bitstream::bits_at;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceId, WorkBudget};

/// Where a configuration record, an ADTS frame or an access unit starts in its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacSource {
    /// The source the bytes come from.
    pub source: SourceId,
    /// Generation of the input the bytes were read from. A stream reader sets the
    /// [`InputToken::generation`](triergon_core::InputToken::generation) of its
    /// input; a record parser or writer copies the caller's value.
    pub generation: u64,
    /// Offset of the first byte from the start of the source, in bytes.
    pub offset: u64,
}

/// Bounds on one AudioSpecificConfig record. A record beyond one of them is refused
/// with [`Error::Limit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacConfigLimits {
    /// Most bytes the record may hold. Zero is refused as [`Error::Invalid`].
    pub maximum_record_bytes: usize,
    /// Most entries a program config element may declare, its six lists together.
    pub maximum_pce_elements: usize,
    /// Most bytes of `comment_field_data` a program config element may carry.
    pub maximum_comment_bytes: usize,
}

/// A run of bits in a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacBitRange {
    /// Offset of the first bit, counted from the most significant bit of the record's
    /// first byte.
    pub start_bit: usize,
    /// Length in bits; zero for an empty run.
    pub bit_count: usize,
}

/// An audio object type as `GetAudioObjectType()` codes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacObjectType {
    /// The object type: `audioObjectType`, or 32 + `audioObjectTypeExt` when escaped.
    /// 2 is AAC LC, 5 SBR and 29 PS.
    pub code: u8,
    /// Whether it is coded as the escape value 31 followed by `audioObjectTypeExt`.
    pub escaped: bool,
}

/// A sampling frequency as an AudioSpecificConfig codes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacFrequency {
    /// `samplingFrequencyIndex` or `extensionSamplingFrequencyIndex`: 0 to 12, or 15
    /// when the frequency follows as a 24-bit value.
    pub index: u8,
    /// The 24-bit frequency in Hz that follows index 15, never zero; `None` for an
    /// indexed frequency.
    pub explicit_hz: Option<u32>,
    /// The frequency in Hz: [`indexed_frequency`] of `index`, or `explicit_hz`.
    pub hz: u32,
}

/// How an AudioSpecificConfig signals SBR or PS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacExtensionSignal {
    /// The record carries no signal for the tool.
    Unsignalled,
    /// The first audio object type names the tool: 5 for SBR, 29 for SBR with PS. The
    /// flag is the presence signalled; the parser only produces `true`.
    Explicit(bool),
    /// A sync extension after the GASpecificConfig carries the flag given here:
    /// `sbrPresentFlag` after `syncExtensionType` `0x2b7`, `psPresentFlag` after
    /// `0x548`.
    Sync(bool),
}

impl AacExtensionSignal {
    /// Whether the record signals the tool as present.
    pub const fn is_present(self) -> bool {
        matches!(self, Self::Explicit(true) | Self::Sync(true))
    }
}

/// The GASpecificConfig fields of an AAC LC configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacGaSpecific {
    /// `frameLengthFlag`: frames of 960 samples instead of 1024.
    pub frame_length_flag: bool,
    /// `coreCoderDelay` in samples (14 bits) when `dependsOnCoreCoder` is set; `None`
    /// when it is clear.
    pub core_coder_delay: Option<u16>,
    /// `extensionFlag`.
    pub extension_flag: bool,
    /// `extensionFlag3`, coded only when `extensionFlag` is set; `None` when absent.
    /// The parser refuses a record that sets it.
    pub extension_flag3: Option<bool>,
}

/// The list of a program config element that an entry belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacPceElementKind {
    /// A front channel element.
    Front,
    /// A side channel element.
    Side,
    /// A back channel element.
    Back,
    /// A low-frequency effects channel element.
    Lfe,
    /// An associated data element.
    AssociatedData,
    /// A coupling channel element (`valid_cc_element`).
    Coupling,
}

/// One entry of a list in a program config element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacPceElement {
    /// The list the entry is in.
    pub kind: AacPceElementKind,
    /// The entry's `*_element_tag_select`, 0 to 15: the `element_instance_tag` of the
    /// syntactic element it names.
    pub tag_select: u8,
    /// `*_element_is_cpe` of a front, side or back entry: a channel pair rather than
    /// a single channel. `None` for the other lists, which have no such bit.
    pub channel_pair: Option<bool>,
    /// `cc_element_is_ind_sw` of a coupling entry; `None` for the other lists.
    pub independent_switch: Option<bool>,
    /// The entry's bits in the record: 4 for an LFE or associated data entry, 5 for
    /// the others.
    pub source_bits: AacBitRange,
}

/// The fields of a `program_config_element()` outside its entry lists and comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacPceFacts {
    /// `element_instance_tag`, 0 to 15.
    pub element_instance_tag: u8,
    /// `object_type`, 0 to 3: the audio object type minus one, so 1 for AAC LC.
    pub profile_code: u8,
    /// `sampling_frequency_index`, 0 to 15, as coded; [`indexed_frequency`] gives
    /// its frequency.
    pub frequency_index: u8,
    /// Entries declared for each list.
    /// Front, side, back, LFE, associated data, coupling, in wire order.
    pub counts: [u8; 6],
    /// `mono_mixdown_element_number`, 0 to 15; `None` when `mono_mixdown_present` is
    /// clear.
    pub mono_mixdown: Option<u8>,
    /// `stereo_mixdown_element_number`, 0 to 15; `None` when
    /// `stereo_mixdown_present` is clear.
    pub stereo_mixdown: Option<u8>,
    /// `matrix_mixdown_idx` (0 to 3) and `pseudo_surround_enable`; `None` when
    /// `matrix_mixdown_idx_present` is clear.
    pub matrix_mixdown: Option<(u8, bool)>,
    /// Value of the bits that pad to a byte boundary of the record before
    /// `comment_field_bytes`.
    pub alignment_value: u8,
    /// Number of those padding bits, 0 to 7.
    pub alignment_bits: u8,
    /// Channels the front, side, back and LFE entries declare: two for a channel
    /// pair, one for any other entry. Associated data and coupling entries add none.
    pub channel_count: u16,
    /// The whole element's bits in the record, through the last comment byte.
    pub source_bits: AacBitRange,
}

/// A parsed `program_config_element()`: the channel layout of a configuration whose
/// `channelConfiguration` is 0.
#[derive(Debug)]
pub struct AacProgramConfig {
    facts: AacPceFacts,
    elements: BudgetVec<AacPceElement>,
    comment: Bytes,
}

impl AacProgramConfig {
    /// The element's fields outside its entry lists and comment.
    pub const fn facts(&self) -> AacPceFacts {
        self.facts
    }
    /// The entries in wire order: front, side, back, LFE, associated data, coupling.
    pub fn elements(&self) -> &[AacPceElement] {
        self.elements.as_slice()
    }
    /// `comment_field_data`: 0 to 255 bytes, sharing the record's allocation.
    pub fn comment_bytes(&self) -> &Bytes {
        &self.comment
    }
}

/// The fields of an AudioSpecificConfig as coded, and what follows from them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAscFacts {
    /// The first `audioObjectType`: 2 (AAC LC), 5 (SBR) or 29 (PS).
    pub outer_object_type: AacObjectType,
    /// The object type of the core coder, always 2 (AAC LC): the first
    /// `audioObjectType`, or the one after the extension frequency when the first is
    /// 5 or 29.
    pub core_object_type: AacObjectType,
    /// `samplingFrequencyIndex` and `samplingFrequency`: the core coder's frequency.
    pub core_frequency: AacFrequency,
    /// `channelConfiguration`: 1 to 7, 11, 12 or 14, or 0 when a program config
    /// element gives the layout.
    pub channel_configuration: u8,
    /// `extensionSamplingFrequencyIndex` and `extensionSamplingFrequency`: the output
    /// frequency with SBR. `None` when the record codes none.
    pub extension_frequency: Option<AacFrequency>,
    /// How the record signals SBR.
    pub sbr: AacExtensionSignal,
    /// How the record signals PS.
    pub ps: AacExtensionSignal,
    /// The GASpecificConfig.
    pub ga: AacGaSpecific,
    /// Channels of the core coder: [`fixed_channel_count`] of the channel
    /// configuration, or the program config element's `channel_count`, which may be
    /// zero.
    pub core_channels: u16,
    /// Bit offset in the record just past the last field read.
    pub understood_end_bit: usize,
    /// Bits of the record the parser was given: eight per byte, or the caller's bit
    /// length.
    pub source_bit_length: usize,
}

impl AacAscFacts {
    /// Samples per channel in one frame of the core coder: 960 when `frameLengthFlag`
    /// is set, otherwise 1024.
    pub const fn core_samples_per_frame(self) -> u16 {
        if self.ga.frame_length_flag { 960 } else { 1024 }
    }
    /// The output sampling frequency in Hz that the record's fields give: the
    /// extension frequency when one is coded, otherwise the core frequency. It is
    /// not a clock for packets.
    pub fn signalled_render_rate_hz(self) -> u32 {
        self.extension_frequency
            .map_or(self.core_frequency.hz, |f| f.hz)
    }
    /// The output channels the record signals: 2 for a mono core with PS signalled
    /// present, otherwise the core's channels.
    /// SBR alone does not change a coded mono configuration into stereo.
    pub const fn signalled_render_channels(self) -> u16 {
        if self.ps.is_present() && self.core_channels == 1 {
            2
        } else {
            self.core_channels
        }
    }
}

/// A parsed AudioSpecificConfig with the record it was read from.
///
/// The record's bytes are shared, not copied, and stay readable whole, including
/// those the parser did not read.
#[derive(Debug)]
pub struct AacAudioSpecificConfig {
    raw: Bytes,
    source: AacSource,
    facts: AacAscFacts,
    pce: Option<AacProgramConfig>,
}

impl AacAudioSpecificConfig {
    pub(crate) fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.pce
            .as_ref()
            .map_or(0, |p| p.elements.capacity())
            .checked_mul(size_of::<AacPceElement>())
            .ok_or(Error::Overflow)
    }
    /// Every byte of the record as the parser was given it.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// Where the record starts in its source.
    pub const fn source(&self) -> AacSource {
        self.source
    }
    /// The record's fields.
    pub const fn facts(&self) -> AacAscFacts {
        self.facts
    }
    /// The program config element of a record whose `channelConfiguration` is 0;
    /// `None` for any other channel configuration.
    pub fn program_config(&self) -> Option<&AacProgramConfig> {
        self.pce.as_ref()
    }
    /// The bits from the last field read to the end of the parsed extent. They are
    /// not interpreted; the run is empty when the fields fill the extent.
    pub fn opaque_suffix_bits(&self) -> AacBitRange {
        AacBitRange {
            start_bit: self.facts.understood_end_bit,
            bit_count: self.facts.source_bit_length - self.facts.understood_end_bit,
        }
    }
    /// Bits0/1: PCE profile/frequency disagreement; bit2: zero PCE channels.
    pub fn syntax_issues(&self) -> u32 {
        self.pce.as_ref().map_or(0, |p| {
            let f = p.facts;
            u32::from(f.profile_code != 1)
                | (u32::from(
                    indexed_frequency(f.frequency_index) != Some(self.facts.core_frequency.hz),
                ) << 1)
                | (u32::from(f.channel_count == 0) << 2)
        })
    }
}

/// Channel count for the fixed MPEG-4 AAC configurations of up to eight channels
/// (ISO/IEC 14496-3:2009/Amd.4, Table 1.19).
pub const fn fixed_channel_count(configuration: u8) -> Option<u8> {
    match configuration {
        1..=6 => Some(configuration),
        11 => Some(7),
        7 | 12 | 14 => Some(8),
        _ => None,
    }
}

/// Sampling frequency in Hz of a sampling frequency index from 0 (96000) to 12
/// (7350); `None` for the reserved indices 13 and 14, the escape 15 and anything
/// above.
pub fn indexed_frequency(index: u8) -> Option<u32> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    RATES.get(usize::from(index)).copied()
}

#[derive(Clone, Copy)]
struct Bits {
    position: usize,
    end: usize,
}

impl Bits {
    fn read(&mut self, raw: &[u8], count: usize) -> Result<u32> {
        if count > 24 {
            return Err(Error::Invalid("AAC scalar bit width"));
        }
        let end = self.position.checked_add(count).ok_or(Error::Overflow)?;
        if end > self.end {
            return Err(Error::Truncated);
        }
        let value = bits_at(raw, self.position, count as u32).ok_or(Error::Truncated)?;
        self.position = end;
        Ok(value)
    }
    fn object_type(&mut self, raw: &[u8]) -> Result<AacObjectType> {
        let code = self.read(raw, 5)? as u8;
        if code == 31 {
            Ok(AacObjectType {
                code: 32 + self.read(raw, 6)? as u8,
                escaped: true,
            })
        } else {
            Ok(AacObjectType {
                code,
                escaped: false,
            })
        }
    }
    fn remaining(self) -> usize {
        self.end - self.position
    }
}

#[derive(Clone, Copy)]
enum FrequencyTarget {
    Core,
    ExplicitExtension,
    SyncExtension,
}

enum Phase {
    Object,
    Frequency(FrequencyTarget),
    ExplicitFrequency(FrequencyTarget),
    Channel,
    InnerObject,
    Ga,
    PceIdentity,
    PceCounts,
    PceMixdown,
    PceReserve,
    PceElements,
    PceComment,
    GaExtension,
    SyncProbe,
    SyncType,
    SyncPresent,
    PsProbe,
    PsPresent,
    Publish,
    Complete,
}

struct PceBuild {
    facts: AacPceFacts,
    elements: BudgetVec<AacPceElement>,
    group: usize,
    in_group: u8,
    comment: Option<Bytes>,
}

/// Parses one AudioSpecificConfig record that the caller holds complete, charged to
/// the parser's pool.
///
/// It accepts AAC LC (object type 2), alone or under SBR (5) or PS (29): the
/// GASpecificConfig, a program config element for channel configuration 0, and the
/// `0x2b7` and `0x548` sync extensions. It reads at most 24 bits per work unit. The
/// output keeps the fields as coded and the bits after them uninterpreted, and
/// derives no decoder timing.
pub struct AacAscParser {
    pool: Budget,
    raw: Option<Bytes>,
    source: AacSource,
    limits: AacConfigLimits,
    bits: Bits,
    phase: Phase,
    outer: Option<AacObjectType>,
    core: Option<AacObjectType>,
    frequency: Option<AacFrequency>,
    extension: Option<AacFrequency>,
    channel: u8,
    sbr: AacExtensionSignal,
    ps: AacExtensionSignal,
    ga: AacGaSpecific,
    pce: Option<PceBuild>,
    failure: Option<Error>,
}

impl std::fmt::Debug for AacAscParser {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AacAscParser")
            .field("source", &self.source)
            .field("limits", &self.limits)
            .field("bit_position", &self.bits.position)
            .field("bit_length", &self.bits.end)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl AacAscParser {
    pub(crate) fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.pce
            .as_ref()
            .map_or(0, |p| p.elements.capacity())
            .checked_mul(size_of::<AacPceElement>())
            .ok_or(Error::Overflow)
    }
    /// A parser over every bit of `raw`, the record that starts at `source`.
    ///
    /// # Errors
    ///
    /// When `raw` is empty, longer than `limits.maximum_record_bytes` or charged to
    /// another pool than `pool`, when that limit is zero, or when the record's end
    /// offset does not fit `u64`.
    pub fn new(
        pool: &Budget,
        source: AacSource,
        raw: Bytes,
        limits: AacConfigLimits,
    ) -> Result<Self> {
        let bits = raw.len().checked_mul(8).ok_or(Error::Overflow)?;
        Self::with_bit_length(pool, source, raw, bits, limits)
    }

    /// A parser over the first `bits` bits of `raw`. The extent is taken as the
    /// caller gives it: it is neither derived from the record nor checked against
    /// its content. The output still holds every byte of `raw`.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), and when `bits` exceeds the bits in `raw`.
    pub fn with_bit_length(
        pool: &Budget,
        source: AacSource,
        raw: Bytes,
        bits: usize,
        limits: AacConfigLimits,
    ) -> Result<Self> {
        crate::record_input::check_record(pool, &raw, 1, limits.maximum_record_bytes)?;
        if bits > raw.len().checked_mul(8).ok_or(Error::Overflow)? {
            return Err(Error::Invalid("AAC bit extent exceeds physical record"));
        }
        source
            .offset
            .checked_add(u64::try_from(raw.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok(Self {
            pool: pool.clone(),
            raw: Some(raw),
            source,
            limits,
            bits: Bits {
                position: 0,
                end: bits,
            },
            phase: Phase::Object,
            outer: None,
            core: None,
            frequency: None,
            extension: None,
            channel: 0,
            sbr: AacExtensionSignal::Unsignalled,
            ps: AacExtensionSignal::Unsignalled,
            ga: AacGaSpecific {
                frame_length_flag: false,
                core_coder_delay: None,
                extension_flag: false,
                extension_flag3: None,
            },
            pce: None,
            failure: None,
        })
    }

    /// Parses for as long as `work` lasts, one unit per group of fields.
    ///
    /// [`BuildStep::Yield`] asks for another call with more work.
    /// [`BuildStep::Ready`] carries the configuration, once; later calls return
    /// [`BuildStep::Complete`].
    ///
    /// # Errors
    ///
    /// When the extent ends inside a field; when an object type is not AAC LC, alone
    /// or under SBR or PS, the channel configuration is not 0, 1 to 7, 11, 12 or 14,
    /// `extensionFlag3` is set or a `0x2b7` sync extension names another object type
    /// than SBR; when a sampling frequency index is 13 or 14 or an explicit frequency
    /// is zero; when a program config element exceeds the limits or what the pool
    /// can ever hold; when `cancel` is set. The parser then returns the same error
    /// from every later call. [`Error::Backpressure`] and [`Error::Allocation`] for
    /// the entries of a program config element leave it as it was, to be stepped
    /// again.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAudioSpecificConfig>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.failure = Some(error);
            self.raw = None;
            self.pce = None;
        }
        result
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAudioSpecificConfig>> {
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
                .ok_or(Error::Invalid("missing ASC source"))?;
            match self.phase {
                Phase::Object => {
                    let object = self.bits.object_type(raw.as_ref())?;
                    if !matches!(object.code, 2 | 5 | 29) {
                        return Err(Error::Unsupported("AAC outer audio object type"));
                    }
                    self.outer = Some(object);
                    if object.code == 2 {
                        self.core = Some(object);
                    } else {
                        self.sbr = AacExtensionSignal::Explicit(true);
                        if object.code == 29 {
                            self.ps = AacExtensionSignal::Explicit(true);
                        }
                    }
                    self.phase = Phase::Frequency(FrequencyTarget::Core);
                }
                Phase::Frequency(target) => {
                    let index = self.bits.read(raw.as_ref(), 4)? as u8;
                    if index == 15 {
                        self.phase = Phase::ExplicitFrequency(target);
                    } else {
                        let hz = indexed_frequency(index)
                            .ok_or(Error::Invalid("AAC reserved sampling index"))?;
                        self.set_frequency(
                            target,
                            AacFrequency {
                                index,
                                explicit_hz: None,
                                hz,
                            },
                        );
                    }
                }
                Phase::ExplicitFrequency(target) => {
                    let hz = self.bits.read(raw.as_ref(), 24)?;
                    if hz == 0 {
                        return Err(Error::Invalid("zero AAC explicit frequency"));
                    }
                    self.set_frequency(
                        target,
                        AacFrequency {
                            index: 15,
                            explicit_hz: Some(hz),
                            hz,
                        },
                    );
                }
                Phase::Channel => {
                    self.channel = self.bits.read(raw.as_ref(), 4)? as u8;
                    if self.channel != 0 && fixed_channel_count(self.channel).is_none() {
                        return Err(Error::Unsupported("AAC channel configuration"));
                    }
                    self.phase =
                        if self.outer.ok_or(Error::Invalid("missing ASC object"))?.code == 2 {
                            Phase::Ga
                        } else {
                            Phase::Frequency(FrequencyTarget::ExplicitExtension)
                        };
                }
                Phase::InnerObject => {
                    let object = self.bits.object_type(raw.as_ref())?;
                    if object.code != 2 {
                        return Err(Error::Unsupported("AAC extension core audio object type"));
                    }
                    self.core = Some(object);
                    self.phase = Phase::Ga;
                }
                Phase::Ga => {
                    self.ga.frame_length_flag = self.bits.read(raw.as_ref(), 1)? != 0;
                    let depends = self.bits.read(raw.as_ref(), 1)? != 0;
                    self.ga.core_coder_delay = if depends {
                        Some(self.bits.read(raw.as_ref(), 14)? as u16)
                    } else {
                        None
                    };
                    self.ga.extension_flag = self.bits.read(raw.as_ref(), 1)? != 0;
                    self.phase = if self.channel == 0 {
                        Phase::PceIdentity
                    } else {
                        Phase::GaExtension
                    };
                }
                Phase::PceIdentity => {
                    let start = self.bits.position;
                    let tag = self.bits.read(raw.as_ref(), 4)? as u8;
                    let profile = self.bits.read(raw.as_ref(), 2)? as u8;
                    let frequency = self.bits.read(raw.as_ref(), 4)? as u8;
                    self.pce = Some(PceBuild {
                        facts: AacPceFacts {
                            element_instance_tag: tag,
                            profile_code: profile,
                            frequency_index: frequency,
                            counts: [0; 6],
                            mono_mixdown: None,
                            stereo_mixdown: None,
                            matrix_mixdown: None,
                            alignment_value: 0,
                            alignment_bits: 0,
                            channel_count: 0,
                            source_bits: AacBitRange {
                                start_bit: start,
                                bit_count: 0,
                            },
                        },
                        elements: BudgetVec::new(&self.pool),
                        group: 0,
                        in_group: 0,
                        comment: None,
                    });
                    self.phase = Phase::PceCounts;
                }
                Phase::PceCounts => {
                    let p = self
                        .pce
                        .as_mut()
                        .ok_or(Error::Invalid("missing PCE state"))?;
                    for (count, width) in p.facts.counts.iter_mut().zip([4, 4, 4, 2, 3, 4]) {
                        *count = self.bits.read(raw.as_ref(), width)? as u8;
                    }
                    let total: usize = p.facts.counts.iter().map(|n| usize::from(*n)).sum();
                    check_limit(total, self.limits.maximum_pce_elements)?;
                    let bytes = total
                        .checked_mul(size_of::<AacPceElement>())
                        .ok_or(Error::Overflow)?;
                    let minimum = Budget::control_storage_bytes()
                        .checked_add(raw.retained_storage_bytes())
                        .and_then(|n| n.checked_add(bytes))
                        .ok_or(Error::Overflow)?;
                    check_limit(minimum, self.pool.limit())?;
                    self.phase = Phase::PceMixdown;
                }
                Phase::PceMixdown => {
                    let p = self
                        .pce
                        .as_mut()
                        .ok_or(Error::Invalid("missing PCE state"))?;
                    p.facts.mono_mixdown = if self.bits.read(raw.as_ref(), 1)? != 0 {
                        Some(self.bits.read(raw.as_ref(), 4)? as u8)
                    } else {
                        None
                    };
                    p.facts.stereo_mixdown = if self.bits.read(raw.as_ref(), 1)? != 0 {
                        Some(self.bits.read(raw.as_ref(), 4)? as u8)
                    } else {
                        None
                    };
                    p.facts.matrix_mixdown = if self.bits.read(raw.as_ref(), 1)? != 0 {
                        Some((
                            self.bits.read(raw.as_ref(), 2)? as u8,
                            self.bits.read(raw.as_ref(), 1)? != 0,
                        ))
                    } else {
                        None
                    };
                    self.phase = Phase::PceReserve;
                }
                Phase::PceReserve => {
                    let p = self
                        .pce
                        .as_mut()
                        .ok_or(Error::Invalid("missing PCE state"))?;
                    let count = p.facts.counts.iter().map(|n| usize::from(*n)).sum();
                    p.elements.try_reserve_exact(count)?;
                    self.phase = Phase::PceElements;
                }
                Phase::PceElements => {
                    let p = self
                        .pce
                        .as_mut()
                        .ok_or(Error::Invalid("missing PCE state"))?;
                    // At most six zero-count groups are skipped. No unbounded
                    // scan, source read or allocation hides behind this unit.
                    while p.group < 6 && p.in_group == p.facts.counts[p.group] {
                        p.group += 1;
                        p.in_group = 0;
                    }
                    if p.group == 6 {
                        self.phase = Phase::PceComment;
                        continue;
                    }
                    let start = self.bits.position;
                    let flag = if p.group < 3 || p.group == 5 {
                        Some(self.bits.read(raw.as_ref(), 1)? != 0)
                    } else {
                        None
                    };
                    let tag = self.bits.read(raw.as_ref(), 4)? as u8;
                    let kind = [
                        AacPceElementKind::Front,
                        AacPceElementKind::Side,
                        AacPceElementKind::Back,
                        AacPceElementKind::Lfe,
                        AacPceElementKind::AssociatedData,
                        AacPceElementKind::Coupling,
                    ][p.group];
                    p.elements
                        .try_push(AacPceElement {
                            kind,
                            tag_select: tag,
                            channel_pair: if p.group < 3 { flag } else { None },
                            independent_switch: if p.group == 5 { flag } else { None },
                            source_bits: AacBitRange {
                                start_bit: start,
                                bit_count: self.bits.position - start,
                            },
                        })
                        .map_err(|e| e.error)?;
                    let channels = if p.group < 3 {
                        if flag == Some(true) { 2 } else { 1 }
                    } else {
                        u16::from(p.group == 3)
                    };
                    p.facts.channel_count = p
                        .facts
                        .channel_count
                        .checked_add(channels)
                        .ok_or(Error::Overflow)?;
                    p.in_group += 1;
                }
                Phase::PceComment => {
                    let p = self
                        .pce
                        .as_mut()
                        .ok_or(Error::Invalid("missing PCE state"))?;
                    let alignment = (8 - self.bits.position % 8) % 8;
                    p.facts.alignment_value = self.bits.read(raw.as_ref(), alignment)? as u8;
                    p.facts.alignment_bits = alignment as u8;
                    let count = self.bits.read(raw.as_ref(), 8)? as usize;
                    check_limit(count, self.limits.maximum_comment_bytes)?;
                    let end = self
                        .bits
                        .position
                        .checked_add(count.checked_mul(8).ok_or(Error::Overflow)?)
                        .ok_or(Error::Overflow)?;
                    if end > self.bits.end {
                        return Err(Error::Truncated);
                    }
                    p.comment = Some(raw.slice(self.bits.position / 8..end / 8)?);
                    self.bits.position = end;
                    p.facts.source_bits.bit_count = end - p.facts.source_bits.start_bit;
                    self.phase = Phase::GaExtension;
                }
                Phase::GaExtension => {
                    if self.ga.extension_flag {
                        let flag = self.bits.read(raw.as_ref(), 1)? != 0;
                        self.ga.extension_flag3 = Some(flag);
                        if flag {
                            return Err(Error::Unsupported("AAC GASpecific version3 extension"));
                        }
                    }
                    self.phase =
                        if self.outer.ok_or(Error::Invalid("missing ASC object"))?.code == 2 {
                            Phase::SyncProbe
                        } else {
                            Phase::Publish
                        };
                }
                Phase::SyncProbe => {
                    let mut probe = self.bits;
                    if probe.remaining() >= 11 && probe.read(raw.as_ref(), 11)? == 0x2b7 {
                        self.bits = probe;
                        self.phase = Phase::SyncType;
                    } else {
                        self.phase = Phase::Publish;
                    }
                }
                Phase::SyncType => {
                    if self.bits.object_type(raw.as_ref())?.code != 5 {
                        return Err(Error::Unsupported("AAC sync extension object type"));
                    }
                    self.phase = Phase::SyncPresent;
                }
                Phase::SyncPresent => {
                    let present = self.bits.read(raw.as_ref(), 1)? != 0;
                    self.sbr = AacExtensionSignal::Sync(present);
                    self.phase = if present {
                        Phase::Frequency(FrequencyTarget::SyncExtension)
                    } else {
                        Phase::Publish
                    };
                }
                Phase::PsProbe => {
                    let mut probe = self.bits;
                    if probe.remaining() >= 11 && probe.read(raw.as_ref(), 11)? == 0x548 {
                        self.bits = probe;
                        self.phase = Phase::PsPresent;
                    } else {
                        self.phase = Phase::Publish;
                    }
                }
                Phase::PsPresent => {
                    self.ps = AacExtensionSignal::Sync(self.bits.read(raw.as_ref(), 1)? != 0);
                    self.phase = Phase::Publish;
                }
                Phase::Publish => {
                    let core_channels = match self.channel {
                        0 => {
                            self.pce
                                .as_ref()
                                .ok_or(Error::Invalid("missing PCE state"))?
                                .facts
                                .channel_count
                        }
                        channel => u16::from(
                            fixed_channel_count(channel)
                                .ok_or(Error::Unsupported("AAC channel configuration"))?,
                        ),
                    };
                    let facts = AacAscFacts {
                        outer_object_type: self
                            .outer
                            .ok_or(Error::Invalid("missing ASC object"))?,
                        core_object_type: self
                            .core
                            .ok_or(Error::Invalid("missing ASC core object"))?,
                        core_frequency: self
                            .frequency
                            .ok_or(Error::Invalid("missing ASC frequency"))?,
                        channel_configuration: self.channel,
                        extension_frequency: self.extension,
                        sbr: self.sbr,
                        ps: self.ps,
                        ga: self.ga,
                        core_channels,
                        understood_end_bit: self.bits.position,
                        source_bit_length: self.bits.end,
                    };
                    let pce = if let Some(p) = self.pce.take() {
                        Some(AacProgramConfig {
                            facts: p.facts,
                            elements: p.elements,
                            comment: p
                                .comment
                                .ok_or(Error::Invalid("missing PCE comment extent"))?,
                        })
                    } else {
                        None
                    };
                    let output = AacAudioSpecificConfig {
                        raw: self
                            .raw
                            .take()
                            .ok_or(Error::Invalid("missing ASC source"))?,
                        source: self.source,
                        facts,
                        pce,
                    };
                    self.phase = Phase::Complete;
                    return Ok(BuildStep::Ready(output));
                }
                Phase::Complete => return Ok(BuildStep::Complete),
            }
        }
    }

    fn set_frequency(&mut self, target: FrequencyTarget, value: AacFrequency) {
        match target {
            FrequencyTarget::Core => {
                self.frequency = Some(value);
                self.phase = Phase::Channel;
            }
            FrequencyTarget::ExplicitExtension => {
                self.extension = Some(value);
                self.phase = Phase::InnerObject;
            }
            FrequencyTarget::SyncExtension => {
                self.extension = Some(value);
                self.phase = Phase::PsProbe;
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
