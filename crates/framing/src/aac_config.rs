//! Selected ASC/GASpecific/PCE source grammar. No encoded-audio decoder is implied.
use crate::BuildStep;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceId, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacSource {
    pub source: SourceId,
    pub generation: u64,
    pub offset: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacConfigLimits {
    pub maximum_record_bytes: usize,
    pub maximum_pce_elements: usize,
    pub maximum_comment_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacBitRange {
    pub start_bit: usize,
    pub bit_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacObjectType {
    pub code: u8,
    pub escaped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacFrequency {
    pub index: u8,
    pub explicit_hz: Option<u32>,
    pub hz: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacExtensionSignal {
    Unsignalled,
    Explicit(bool),
    Sync(bool),
}

impl AacExtensionSignal {
    pub const fn is_present(self) -> bool {
        matches!(self, Self::Explicit(true) | Self::Sync(true))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacGaSpecific {
    pub frame_length_flag: bool,
    pub core_coder_delay: Option<u16>,
    pub extension_flag: bool,
    pub extension_flag3: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacPceElementKind {
    Front,
    Side,
    Back,
    Lfe,
    AssociatedData,
    Coupling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacPceElement {
    pub kind: AacPceElementKind,
    pub tag_select: u8,
    pub channel_pair: Option<bool>,
    pub independent_switch: Option<bool>,
    pub source_bits: AacBitRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacPceFacts {
    pub element_instance_tag: u8,
    pub profile_code: u8,
    pub frequency_index: u8,
    /// Front, side, back, LFE, associated data, coupling, in wire order.
    pub counts: [u8; 6],
    pub mono_mixdown: Option<u8>,
    pub stereo_mixdown: Option<u8>,
    pub matrix_mixdown: Option<(u8, bool)>,
    pub alignment_value: u8,
    pub alignment_bits: u8,
    pub channel_count: u16,
    pub source_bits: AacBitRange,
}

pub struct AacProgramConfig {
    facts: AacPceFacts,
    elements: BudgetVec<AacPceElement>,
    comment: Bytes,
}

impl AacProgramConfig {
    pub const fn facts(&self) -> AacPceFacts {
        self.facts
    }
    pub fn elements(&self) -> &[AacPceElement] {
        self.elements.as_slice()
    }
    pub fn comment_bytes(&self) -> &Bytes {
        &self.comment
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAscFacts {
    pub outer_object_type: AacObjectType,
    pub core_object_type: AacObjectType,
    pub core_frequency: AacFrequency,
    pub channel_configuration: u8,
    pub extension_frequency: Option<AacFrequency>,
    pub sbr: AacExtensionSignal,
    pub ps: AacExtensionSignal,
    pub ga: AacGaSpecific,
    pub core_channels: u16,
    pub understood_end_bit: usize,
    pub source_bit_length: usize,
}

impl AacAscFacts {
    pub const fn core_samples_per_frame(self) -> u16 {
        if self.ga.frame_length_flag { 960 } else { 1024 }
    }
    /// A selected presentation view from explicit fields, never a packet clock.
    pub fn signalled_render_rate_hz(self) -> u32 {
        self.extension_frequency
            .map_or(self.core_frequency.hz, |f| f.hz)
    }
    /// SBR alone does not change a coded mono configuration into stereo.
    pub const fn signalled_render_channels(self) -> u16 {
        if self.ps.is_present() && self.core_channels == 1 {
            2
        } else {
            self.core_channels
        }
    }
}

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
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub const fn source(&self) -> AacSource {
        self.source
    }
    pub const fn facts(&self) -> AacAscFacts {
        self.facts
    }
    pub fn program_config(&self) -> Option<&AacProgramConfig> {
        self.pce.as_ref()
    }
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
                    indexed_aac_frequency(f.frequency_index) != Some(self.facts.core_frequency.hz),
                ) << 1)
                | (u32::from(f.channel_count == 0) << 2)
        })
    }
}

pub fn indexed_aac_frequency(index: u8) -> Option<u32> {
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
        let mut value = 0_u32;
        for at in self.position..end {
            value = (value << 1)
                | u32::from((raw.get(at / 8).ok_or(Error::Truncated)? >> (7 - at % 8)) & 1);
        }
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

/// One already-framed same-pool ASC. It reads at most24 bits per work unit,
/// retains coded fields and opaque suffixes, and never invents a decoder clock.
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

impl AacAscParser {
    pub(crate) fn descriptor_storage_bytes(&self) -> Result<usize> {
        self.pce
            .as_ref()
            .map_or(0, |p| p.elements.capacity())
            .checked_mul(size_of::<AacPceElement>())
            .ok_or(Error::Overflow)
    }
    pub fn new(
        pool: &Budget,
        source: AacSource,
        raw: Bytes,
        limits: AacConfigLimits,
    ) -> Result<Self> {
        let bits = raw.len().checked_mul(8).ok_or(Error::Overflow)?;
        Self::with_bit_length(pool, source, raw, bits, limits)
    }

    /// `bits` is an explicit caller-delimited extent, not an inferred boundary
    /// or a validation certificate. All physical bytes still survive output.
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
                        let hz = indexed_aac_frequency(index)
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
                    if self.channel > 7 {
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
                    } else if p.group == 3 {
                        1
                    } else {
                        0
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
                        1..=6 => u16::from(self.channel),
                        7 => 8,
                        _ => return Err(Error::Unsupported("AAC channel configuration")),
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
