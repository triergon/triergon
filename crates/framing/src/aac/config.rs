//! The AudioSpecificConfig of ISO/IEC 14496-3 for AAC LC, alone or under SBR or PS,
//! with its GASpecificConfig and program config element. Nothing here decodes audio.
use crate::BuildStep;
use crate::retryable;
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
    /// Whether the AAC-LC GASpecificConfig carries `extensionFlag3`.
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
    /// Entries declared for each list: front, side, back, LFE, associated data, coupling.
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
    /// The core coder's frame length, delay and extension flags.
    pub ga: AacGaSpecific,
    /// Channels of the core coder: [`fixed_channel_count`] of the channel
    /// configuration, or the program config element's `channel_count`, which may be
    /// zero.
    pub core_channels: u16,
    /// Bit offset in the record just past the last field read.
    pub understood_end_bit: usize,
    source_bit_length: usize,
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

/// A parsed AudioSpecificConfig with the record it was read from, whose bytes are
/// shared and stay readable whole, including those the parser did not read.
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
    // Every field goes through here; a call per field costs more than the read.
    #[inline(always)]
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
    /// A sampling frequency index, or the 24-bit frequency after the escape index 15
    /// when `explicit`. `None` when that frequency follows.
    #[inline]
    fn frequency(&mut self, raw: &[u8], explicit: bool) -> Result<Option<AacFrequency>> {
        if explicit {
            let hz = self.read(raw, 24)?;
            if hz == 0 {
                return Err(Error::Invalid("zero AAC explicit frequency"));
            }
            return Ok(Some(AacFrequency {
                index: 15,
                explicit_hz: Some(hz),
                hz,
            }));
        }
        let index = self.read(raw, 4)? as u8;
        if index == 15 {
            return Ok(None);
        }
        let hz = indexed_frequency(index).ok_or(Error::Invalid("AAC reserved sampling index"))?;
        Ok(Some(AacFrequency {
            index,
            explicit_hz: None,
            hz,
        }))
    }
}

/// The fields before the core object type that the rest of the record depends on.
#[derive(Clone, Copy)]
struct Opening {
    outer: AacObjectType,
    frequency: AacFrequency,
    channel: u8,
}

/// The fields before GASpecificConfig that its syntax and the output depend on.
#[derive(Clone, Copy)]
struct Head {
    opening: Opening,
    core: AacObjectType,
}

/// The next field to read, with the earlier ones it depends on.
// This and the field enums inside it have byte tags: decoding a niche tag on every
// round of the walk costs more than the bytes it saves.
#[repr(u8)]
enum Phase {
    /// AudioSpecificConfig before GASpecificConfig.
    Config(ConfigField),
    /// GASpecificConfig and the sync extensions after it, with the program config
    /// element once it is read.
    Body(Head, BodyField, Option<AacProgramConfig>),
    /// The program config element of channel configuration 0 after its identity.
    Pce(Head, PceBuild, PceField),
}

/// AudioSpecificConfig before GASpecificConfig (ISO/IEC 14496-3, 1.6.2.1).
#[derive(Clone, Copy)]
#[repr(u8)]
enum ConfigField {
    /// `audioObjectType`.
    Object,
    /// `samplingFrequencyIndex`, or `samplingFrequency` when `explicit`.
    Frequency {
        outer: AacObjectType,
        explicit: bool,
    },
    /// `channelConfiguration`.
    Channel {
        outer: AacObjectType,
        frequency: AacFrequency,
    },
    /// `extensionSamplingFrequencyIndex` of a first object type of 5 or 29, or
    /// `extensionSamplingFrequency` when `explicit`.
    ExtensionFrequency { opening: Opening, explicit: bool },
    /// The core `audioObjectType` after it.
    CoreObject(Opening),
}

/// GASpecificConfig (ISO/IEC 14496-3, 4.4.1) and the sync extensions after it.
#[derive(Clone, Copy)]
#[repr(u8)]
enum BodyField {
    /// `frameLengthFlag` through `extensionFlag`.
    Ga,
    /// The identity fields that open the program config element of channel
    /// configuration 0.
    PceIdentity,
    /// `extensionFlag3`, after any program config element.
    GaExtension,
    /// The sync extensions after the GASpecificConfig of a first object type of 2.
    Sync(SyncField),
    /// Every field is read.
    Publish,
}

/// `program_config_element()` (ISO/IEC 14496-3, 4.4.1.1) after its identity.
#[derive(Clone, Copy)]
enum PceField {
    Counts,
    Mixdown,
    Reserve,
    Elements,
    Comment,
}

/// The sync extensions after the GASpecificConfig of a first object type of 2.
#[derive(Clone, Copy)]
#[repr(u8)]
enum SyncField {
    /// `syncExtensionType` `0x2b7`.
    Sbr,
    /// `extensionAudioObjectType`.
    SbrObject,
    /// `sbrPresentFlag`.
    SbrPresent,
    /// `extensionSamplingFrequencyIndex`, or `extensionSamplingFrequency` when
    /// `explicit`.
    Frequency { explicit: bool },
    /// `syncExtensionType` `0x548`.
    Ps,
    /// `psPresentFlag`.
    PsPresent,
}

struct PceBuild {
    facts: AacPceFacts,
    elements: BudgetVec<AacPceElement>,
    group: usize,
    in_group: u8,
}

/// A parse in progress: the record and what has been read from it.
struct Walk {
    raw: Bytes,
    phase: Phase,
    fields: Fields,
}

/// The fields read so far that the output takes as they are.
struct Fields {
    extension: Option<AacFrequency>,
    sbr: AacExtensionSignal,
    ps: AacExtensionSignal,
    ga: AacGaSpecific,
}

/// Where a parse stands. The walk is held only while the record is read.
enum State {
    Reading(Walk),
    Complete,
    Failed(Error),
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
    source: AacSource,
    limits: AacConfigLimits,
    bits: Bits,
    state: State,
}

impl std::fmt::Debug for AacAscParser {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failure = match self.state {
            State::Failed(error) => Some(error),
            State::Reading(_) | State::Complete => None,
        };
        formatter
            .debug_struct("AacAscParser")
            .field("source", &self.source)
            .field("limits", &self.limits)
            .field("bit_position", &self.bits.position)
            .field("bit_length", &self.bits.end)
            .field("failure", &failure)
            .finish_non_exhaustive()
    }
}

impl AacAscParser {
    pub(crate) fn descriptor_storage_bytes(&self) -> Result<usize> {
        let elements = match &self.state {
            State::Reading(Walk {
                phase: Phase::Pce(_, build, _),
                ..
            }) => build.elements.capacity(),
            State::Reading(Walk {
                phase: Phase::Body(_, _, Some(program)),
                ..
            }) => program.elements.capacity(),
            State::Reading(_) | State::Complete | State::Failed(_) => 0,
        };
        elements
            .checked_mul(size_of::<AacPceElement>())
            .ok_or(Error::Overflow)
    }
    /// A parser over every bit of `raw`, the record that starts at `source`. Fails when
    /// `raw` is empty, longer than `limits.maximum_record_bytes` or charged to another
    /// pool than `pool`, when that limit is zero, or when the record's end offset does
    /// not fit `u64`.
    pub fn new(
        pool: &Budget,
        source: AacSource,
        raw: Bytes,
        limits: AacConfigLimits,
    ) -> Result<Self> {
        let bits = raw.len().checked_mul(8).ok_or(Error::Overflow)?;
        Self::with_bit_length(pool, source, raw, bits, limits)
    }

    /// A parser over the first `bits` bits of `raw`. The extent is taken as the caller
    /// gives it: it is neither derived from the record nor checked against its content.
    /// The output still holds every byte of `raw`. Fails as [`new`](Self::new) does,
    /// and when `bits` exceeds the bits in `raw`.
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
            source,
            limits,
            bits: Bits {
                position: 0,
                end: bits,
            },
            state: State::Reading(Walk {
                raw,
                phase: Phase::Config(ConfigField::Object),
                fields: Fields {
                    extension: None,
                    sbr: AacExtensionSignal::Unsignalled,
                    ps: AacExtensionSignal::Unsignalled,
                    ga: AacGaSpecific {
                        frame_length_flag: false,
                        core_coder_delay: None,
                        extension_flag: false,
                        extension_flag3: None,
                    },
                },
            }),
        })
    }

    /// Parses for as long as `work` lasts, one unit per group of fields.
    ///
    /// Fails when the extent ends inside a field; when an object type is not AAC LC,
    /// alone or under SBR or PS, the channel configuration is not 0, 1 to 7, 11, 12 or
    /// 14, `extensionFlag3` is set or a `0x2b7` sync extension names another object
    /// type than SBR; when a sampling frequency index is 13 or 14 or an explicit
    /// frequency is zero; when a program config element exceeds the limits or what the
    /// pool can ever hold; when `cancel` is set. The parser then returns the same error
    /// from every later call. [`Error::Backpressure`] and [`Error::Allocation`] for the
    /// entries of a program config element leave it as it was, to be stepped again.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAudioSpecificConfig>> {
        let walk = match &mut self.state {
            State::Reading(walk) => walk,
            State::Complete => return Ok(BuildStep::Complete),
            State::Failed(error) => return Err(*error),
        };
        let result = walk.read(
            &mut self.bits,
            &self.pool,
            &self.limits,
            self.source,
            work,
            cancel,
        );
        match result {
            Ok(BuildStep::Ready(_)) => self.state = State::Complete,
            Ok(BuildStep::Yield | BuildStep::Complete) => {}
            Err(error) if !retryable(error) => self.state = State::Failed(error),
            Err(_) => {}
        }
        result
    }
}

impl Walk {
    /// Reads one group of fields per unit of `work` until the record is read, then
    /// publishes the configuration.
    fn read(
        &mut self,
        bits: &mut Bits,
        pool: &Budget,
        limits: &AacConfigLimits,
        source: AacSource,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAudioSpecificConfig>> {
        let raw = self.raw.as_ref();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            match &mut self.phase {
                Phase::Config(field) => {
                    if let Some(head) = self.fields.audio_specific_config(bits, raw, field)? {
                        self.phase = Phase::Body(head, BodyField::Ga, None);
                    }
                }
                Phase::Body(head, field, program) => match field {
                    BodyField::Ga => {
                        self.fields.ga_specific_config(bits, raw)?;
                        if head.opening.channel == 0 {
                            *field = BodyField::PceIdentity;
                        } else {
                            *field = BodyField::GaExtension;
                        }
                    }
                    BodyField::PceIdentity => {
                        let build = PceBuild::identity(bits, raw, pool)?;
                        self.phase = Phase::Pce(*head, build, PceField::Counts);
                    }
                    BodyField::GaExtension => {
                        self.fields.ga_extension(bits, raw)?;
                        if head.opening.outer.code == 2 {
                            *field = BodyField::Sync(SyncField::Sbr);
                        } else {
                            *field = BodyField::Publish;
                        }
                    }
                    BodyField::Sync(sync) => match self.fields.sync_extension(bits, raw, *sync)? {
                        Some(next) => *sync = next,
                        None => *field = BodyField::Publish,
                    },
                    BodyField::Publish => {
                        return self
                            .fields
                            .publish(&self.raw, head, program.take(), source, *bits);
                    }
                },
                Phase::Pce(head, build, field) => {
                    if let Some(program) = build.read(field, bits, &self.raw, pool, limits)? {
                        self.phase = Phase::Body(*head, BodyField::GaExtension, Some(program));
                    }
                }
            }
        }
    }
}

impl Fields {
    /// One field of AudioSpecificConfig before GASpecificConfig; the fields
    /// GASpecificConfig depends on once the last of them is read.
    fn audio_specific_config(
        &mut self,
        bits: &mut Bits,
        raw: &[u8],
        field: &mut ConfigField,
    ) -> Result<Option<Head>> {
        match *field {
            ConfigField::Object => {
                let outer = bits.object_type(raw)?;
                if !matches!(outer.code, 2 | 5 | 29) {
                    return Err(Error::Unsupported("AAC outer audio object type"));
                }
                if outer.code != 2 {
                    self.sbr = AacExtensionSignal::Explicit(true);
                    if outer.code == 29 {
                        self.ps = AacExtensionSignal::Explicit(true);
                    }
                }
                *field = ConfigField::Frequency {
                    outer,
                    explicit: false,
                };
            }
            ConfigField::Frequency { outer, explicit } => {
                *field = match bits.frequency(raw, explicit)? {
                    Some(frequency) => ConfigField::Channel { outer, frequency },
                    None => ConfigField::Frequency {
                        outer,
                        explicit: true,
                    },
                };
            }
            ConfigField::Channel { outer, frequency } => {
                let channel = bits.read(raw, 4)? as u8;
                if channel != 0 && fixed_channel_count(channel).is_none() {
                    return Err(Error::Unsupported("AAC channel configuration"));
                }
                let opening = Opening {
                    outer,
                    frequency,
                    channel,
                };
                if outer.code == 2 {
                    return Ok(Some(Head {
                        opening,
                        core: outer,
                    }));
                }
                *field = ConfigField::ExtensionFrequency {
                    opening,
                    explicit: false,
                };
            }
            ConfigField::ExtensionFrequency { opening, explicit } => {
                *field = match bits.frequency(raw, explicit)? {
                    Some(frequency) => {
                        self.extension = Some(frequency);
                        ConfigField::CoreObject(opening)
                    }
                    None => ConfigField::ExtensionFrequency {
                        opening,
                        explicit: true,
                    },
                };
            }
            ConfigField::CoreObject(opening) => {
                let core = bits.object_type(raw)?;
                if core.code != 2 {
                    return Err(Error::Unsupported("AAC extension core audio object type"));
                }
                return Ok(Some(Head { opening, core }));
            }
        }
        Ok(None)
    }

    /// GASpecificConfig from `frameLengthFlag` through `extensionFlag`.
    fn ga_specific_config(&mut self, bits: &mut Bits, raw: &[u8]) -> Result<()> {
        self.ga.frame_length_flag = bits.read(raw, 1)? != 0;
        let depends = bits.read(raw, 1)? != 0;
        self.ga.core_coder_delay = if depends {
            Some(bits.read(raw, 14)? as u16)
        } else {
            None
        };
        self.ga.extension_flag = bits.read(raw, 1)? != 0;
        Ok(())
    }

    /// `extensionFlag3` when `extensionFlag` is set.
    fn ga_extension(&mut self, bits: &mut Bits, raw: &[u8]) -> Result<()> {
        if self.ga.extension_flag {
            let flag = bits.read(raw, 1)? != 0;
            self.ga.extension_flag3 = Some(flag);
            if flag {
                return Err(Error::Unsupported("AAC GASpecific version3 extension"));
            }
        }
        Ok(())
    }

    /// One field of the SBR and PS sync extensions: the next one, or `None` when the
    /// record has none, or no more.
    fn sync_extension(
        &mut self,
        bits: &mut Bits,
        raw: &[u8],
        field: SyncField,
    ) -> Result<Option<SyncField>> {
        Ok(match field {
            SyncField::Sbr => {
                let mut probe = *bits;
                if probe.remaining() >= 11 && probe.read(raw, 11)? == 0x2b7 {
                    *bits = probe;
                    Some(SyncField::SbrObject)
                } else {
                    None
                }
            }
            SyncField::SbrObject => {
                if bits.object_type(raw)?.code != 5 {
                    return Err(Error::Unsupported("AAC sync extension object type"));
                }
                Some(SyncField::SbrPresent)
            }
            SyncField::SbrPresent => {
                let present = bits.read(raw, 1)? != 0;
                self.sbr = AacExtensionSignal::Sync(present);
                present.then_some(SyncField::Frequency { explicit: false })
            }
            SyncField::Frequency { explicit } => match bits.frequency(raw, explicit)? {
                Some(frequency) => {
                    self.extension = Some(frequency);
                    Some(SyncField::Ps)
                }
                None => Some(SyncField::Frequency { explicit: true }),
            },
            SyncField::Ps => {
                let mut probe = *bits;
                if probe.remaining() >= 11 && probe.read(raw, 11)? == 0x548 {
                    *bits = probe;
                    Some(SyncField::PsPresent)
                } else {
                    None
                }
            }
            SyncField::PsPresent => {
                self.ps = AacExtensionSignal::Sync(bits.read(raw, 1)? != 0);
                None
            }
        })
    }
    /// The configuration of a walk that read the whole record, sharing its bytes, as
    /// the step that returns it.
    fn publish(
        &self,
        raw: &Bytes,
        head: &Head,
        program: Option<AacProgramConfig>,
        source: AacSource,
        bits: Bits,
    ) -> Result<BuildStep<AacAudioSpecificConfig>> {
        let channel = head.opening.channel;
        let core_channels = match &program {
            Some(program) => program.facts.channel_count,
            None => u16::from(
                fixed_channel_count(channel)
                    .ok_or(Error::Unsupported("AAC channel configuration"))?,
            ),
        };
        let facts = AacAscFacts {
            outer_object_type: head.opening.outer,
            core_object_type: head.core,
            core_frequency: head.opening.frequency,
            channel_configuration: channel,
            extension_frequency: self.extension,
            sbr: self.sbr,
            ps: self.ps,
            ga: self.ga,
            core_channels,
            understood_end_bit: bits.position,
            source_bit_length: bits.end,
        };
        Ok(BuildStep::Ready(AacAudioSpecificConfig {
            raw: raw.clone(),
            source,
            facts,
            pce: program,
        }))
    }
}

impl PceBuild {
    /// The identity fields that open a program config element.
    fn identity(bits: &mut Bits, raw: &[u8], pool: &Budget) -> Result<Self> {
        let start = bits.position;
        let tag = bits.read(raw, 4)? as u8;
        let profile = bits.read(raw, 2)? as u8;
        let frequency = bits.read(raw, 4)? as u8;
        Ok(Self {
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
            elements: BudgetVec::new(pool),
            group: 0,
            in_group: 0,
        })
    }

    /// One group of fields after the identity, one list entry per unit; the element
    /// once its comment is read.
    fn read(
        &mut self,
        field: &mut PceField,
        bits: &mut Bits,
        raw: &Bytes,
        pool: &Budget,
        limits: &AacConfigLimits,
    ) -> Result<Option<AacProgramConfig>> {
        match field {
            PceField::Counts => {
                for (count, width) in self.facts.counts.iter_mut().zip([4, 4, 4, 2, 3, 4]) {
                    *count = bits.read(raw.as_ref(), width)? as u8;
                }
                let total: usize = self.facts.counts.iter().map(|n| usize::from(*n)).sum();
                check_limit(total, limits.maximum_pce_elements)?;
                let bytes = total
                    .checked_mul(size_of::<AacPceElement>())
                    .ok_or(Error::Overflow)?;
                let minimum = Budget::control_storage_bytes()
                    .checked_add(raw.retained_storage_bytes())
                    .and_then(|n| n.checked_add(bytes))
                    .ok_or(Error::Overflow)?;
                check_limit(minimum, pool.limit())?;
                *field = PceField::Mixdown;
            }
            PceField::Mixdown => {
                self.facts.mono_mixdown = if bits.read(raw.as_ref(), 1)? != 0 {
                    Some(bits.read(raw.as_ref(), 4)? as u8)
                } else {
                    None
                };
                self.facts.stereo_mixdown = if bits.read(raw.as_ref(), 1)? != 0 {
                    Some(bits.read(raw.as_ref(), 4)? as u8)
                } else {
                    None
                };
                self.facts.matrix_mixdown = if bits.read(raw.as_ref(), 1)? != 0 {
                    Some((
                        bits.read(raw.as_ref(), 2)? as u8,
                        bits.read(raw.as_ref(), 1)? != 0,
                    ))
                } else {
                    None
                };
                *field = PceField::Reserve;
            }
            PceField::Reserve => {
                let count = self.facts.counts.iter().map(|n| usize::from(*n)).sum();
                self.elements.try_reserve_exact(count)?;
                *field = PceField::Elements;
            }
            PceField::Elements => {
                // At most six zero-count groups are skipped. No unbounded scan, source
                // read or allocation hides behind this unit.
                while self.group < 6 && self.in_group == self.facts.counts[self.group] {
                    self.group += 1;
                    self.in_group = 0;
                }
                if self.group == 6 {
                    *field = PceField::Comment;
                    return Ok(None);
                }
                let start = bits.position;
                let flag = if self.group < 3 || self.group == 5 {
                    Some(bits.read(raw.as_ref(), 1)? != 0)
                } else {
                    None
                };
                let tag = bits.read(raw.as_ref(), 4)? as u8;
                let kind = [
                    AacPceElementKind::Front,
                    AacPceElementKind::Side,
                    AacPceElementKind::Back,
                    AacPceElementKind::Lfe,
                    AacPceElementKind::AssociatedData,
                    AacPceElementKind::Coupling,
                ][self.group];
                self.elements
                    .try_push(AacPceElement {
                        kind,
                        tag_select: tag,
                        channel_pair: if self.group < 3 { flag } else { None },
                        independent_switch: if self.group == 5 { flag } else { None },
                        source_bits: AacBitRange {
                            start_bit: start,
                            bit_count: bits.position - start,
                        },
                    })
                    .map_err(|e| e.error)?;
                let channels = if self.group < 3 {
                    if flag == Some(true) { 2 } else { 1 }
                } else {
                    u16::from(self.group == 3)
                };
                self.facts.channel_count = self
                    .facts
                    .channel_count
                    .checked_add(channels)
                    .ok_or(Error::Overflow)?;
                self.in_group += 1;
            }
            PceField::Comment => {
                let alignment = (8 - bits.position % 8) % 8;
                self.facts.alignment_value = bits.read(raw.as_ref(), alignment)? as u8;
                self.facts.alignment_bits = alignment as u8;
                let count = bits.read(raw.as_ref(), 8)? as usize;
                check_limit(count, limits.maximum_comment_bytes)?;
                let end = bits
                    .position
                    .checked_add(count.checked_mul(8).ok_or(Error::Overflow)?)
                    .ok_or(Error::Overflow)?;
                if end > bits.end {
                    return Err(Error::Truncated);
                }
                let comment = raw.slice(bits.position / 8..end / 8)?;
                bits.position = end;
                self.facts.source_bits.bit_count = end - self.facts.source_bits.start_bit;
                return Ok(Some(AacProgramConfig {
                    facts: self.facts,
                    elements: std::mem::replace(&mut self.elements, BudgetVec::new(pool)),
                    comment,
                }));
            }
        }
        Ok(None)
    }
}

fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}
