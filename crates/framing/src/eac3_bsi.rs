//! Selected complete E-AC-3 BSI source facts, ATSC A/52:2018 Annex E Table E1.2.
//! This opt-in inspector does not validate encoded audio, CRCs or stream groups.
use crate::{Ac3Family, Ac3Frame, Ac3FrameDetails, BuildStep};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

/// Frame-relative MSB-first bit range. Physical alignment is never discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eac3BitRange {
    start: usize,
    bits: usize,
}

impl Eac3BitRange {
    pub const fn start_bit(self) -> usize {
        self.start
    }
    pub const fn bit_count(self) -> usize {
        self.bits
    }
}

/// An owned view: frame source/generation and the whole backing charge survive
/// the BSI inspector, frame reader and other output handles being dropped.
#[derive(Clone, Debug)]
pub struct Eac3BitSpan {
    frame: Ac3Frame,
    range: Eac3BitRange,
}

impl Eac3BitSpan {
    pub fn frame(&self) -> &Ac3Frame {
        &self.frame
    }
    pub const fn range(&self) -> Eac3BitRange {
        self.range
    }
    pub fn bit(&self, index: usize) -> Result<bool> {
        if index >= self.range.bits {
            return Err(Error::Invalid("E-AC-3 bit span index"));
        }
        let position = self.range.start.checked_add(index).ok_or(Error::Overflow)?;
        Ok(self.frame.bytes()[position / 8] & (1 << (7 - position % 8)) != 0)
    }
    /// Source bytes covering the span. The first/last bytes can also contain
    /// adjacent fields; range() retains the exact bit boundary.
    pub fn covering_bytes(&self) -> Result<Bytes> {
        let end = self
            .range
            .start
            .checked_add(self.range.bits)
            .ok_or(Error::Overflow)?;
        self.frame
            .bytes()
            .slice(self.range.start / 8..end.checked_add(7).ok_or(Error::Overflow)? / 8)
    }
}

/// Raw mandatory tail fields. Auxiliary length, when physically present, is
/// retained without interpreting the opaque auxiliary/audio payload or its CRC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eac3Trailer {
    pub auxiliary_data_present: bool,
    pub auxiliary_length_code: Option<u16>,
    pub encoder_info: bool,
    pub crc2_word: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eac3BsiLimits {
    pub maximum_frame_bytes: usize,
    /// BSI bits after the sixteen-bit syncword, including the fixed BSI prefix.
    pub maximum_bsi_bits: usize,
}

/// Raw coded values and exact group spans. None is physically absent in the
/// selected context; an explicitly coded zero remains Some(0)/Some(false).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eac3BsiFacts {
    pub end_bit: usize,
    pub prefix_end_bit: usize,
    pub mixing_metadata: Option<Eac3BitRange>,
    pub mixing_definition: Option<u8>,
    pub informational_metadata: Option<Eac3BitRange>,
    pub bitstream_mode: Option<u8>,
    pub copyright: Option<bool>,
    pub original: Option<bool>,
    pub surround_mode: Option<u8>,
    pub headphone_mode: Option<u8>,
    pub surround_ex_mode: Option<u8>,
    /// Packed mixlevel(5), roomtyp(2), adconvtyp(1), without normalization.
    pub production: Option<u8>,
    pub second_production: Option<u8>,
    pub source_fscod: Option<bool>,
    pub converter_sync: Option<bool>,
    /// None for the six-block Type-2 inferred block ID; distinct from Some(true).
    pub block_id: Option<bool>,
    pub effective_block_id: Option<bool>,
    pub converted_frame_size_code: Option<u8>,
    /// Payload bits only, excluding addbsie and addbsil. May be byte-unaligned.
    pub additional_bsi: Option<Eac3BitRange>,
    pub trailer: Eac3Trailer,
}

#[derive(Clone, Debug)]
pub struct Eac3Bsi {
    frame: Ac3Frame,
    facts: Eac3BsiFacts,
}

impl Eac3Bsi {
    pub fn frame(&self) -> &Ac3Frame {
        &self.frame
    }
    pub const fn facts(&self) -> Eac3BsiFacts {
        self.facts
    }
    pub fn bit_span(&self, range: Eac3BitRange) -> Result<Eac3BitSpan> {
        let end = range.start.checked_add(range.bits).ok_or(Error::Overflow)?;
        if range.start < 16 || end > self.facts.end_bit {
            return Err(Error::Invalid("E-AC-3 BSI span outside inspected fields"));
        }
        Ok(Eac3BitSpan {
            frame: self.frame.clone(),
            range,
        })
    }
    /// Weighted locations, not merely set bits. ATSC Table E2.5 has six pair
    /// locations; its bit zero is the source word's most significant bit.
    pub fn channel_map_channels(&self) -> Option<u8> {
        let Ac3FrameDetails::Eac3 {
            channel_map: Some(map),
            ..
        } = self.frame.facts().header.details
        else {
            return None;
        };
        let weights = [1_u8, 1, 1, 1, 1, 2, 2, 1, 1, 2, 2, 2, 1, 2, 1, 1];
        let mut count = 0;
        for (position, weight) in weights.into_iter().enumerate() {
            if map & (1 << (15 - position)) != 0 {
                count += weight;
            }
        }
        Some(count)
    }
    /// Local coded-count consistency only. No independent/dependent group,
    /// presentation layout or decoder-readiness conclusion follows from true.
    pub fn channel_map_matches_coded_count(&self) -> Option<bool> {
        let header = self.frame.facts().header;
        Some(
            self.channel_map_channels()?
                == header.acmod_full_bandwidth_channels()? + u8::from(header.lfe_on),
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    MixPresent,
    MixDownmix,
    MixCenter,
    MixSurround,
    MixLfeFlag,
    MixLfeValue,
    ProgrammeFlag,
    ProgrammeValue,
    Programme2Flag,
    Programme2Value,
    ExternalFlag,
    ExternalValue,
    MixDefinition,
    MixControl,
    PanFlag,
    PanValue,
    Pan2Flag,
    Pan2Value,
    FrameMixFlag,
    BlockFlag,
    BlockValue,
    MixDone,
    InfoPresent,
    InfoService,
    InfoCopyright,
    InfoOriginal,
    InfoSurround,
    InfoHeadphone,
    InfoSurroundEx,
    ProductionFlag,
    ProductionValue,
    Production2Flag,
    Production2Value,
    SourceRate,
    InfoDone,
    ConverterSync,
    ConvertedBlock,
    ConvertedSize,
    AdditionalFlag,
    AdditionalLength,
    AdditionalBytes,
    Finish,
}

/// Inspects a complete same-pool E-AC-3 frame from Ac3FrameReader. The reader
/// owns chunk/EOF/transport and reserve/freeze handling; this second stage owns
/// BSI bit work and cancellation. It allocates no output storage or descriptors.
/// Mixing definitions 0/1/2 are selected; flexible definition 3 is unsupported.
pub struct Eac3BsiParser {
    frame: Option<Ac3Frame>,
    limits: Eac3BsiLimits,
    stream_type: u8,
    mode: u8,
    lfe: bool,
    fscod: u8,
    blocks: u8,
    phase: Phase,
    position: usize,
    before_tail: usize,
    partial_seen: u8,
    partial_width: u8,
    partial_value: u16,
    mix_start: usize,
    info_start: usize,
    block: u8,
    additional_remaining: u8,
    facts: Eac3BsiFacts,
    complete: bool,
    failure: Option<Error>,
}

impl Eac3BsiParser {
    pub fn new(pool: &Budget, frame: Ac3Frame, limits: Eac3BsiLimits) -> Result<Self> {
        if limits.maximum_frame_bytes == 0 || limits.maximum_bsi_bits == 0 {
            return Err(Error::Invalid("zero E-AC-3 BSI policy"));
        }
        if !frame.bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let length = frame.bytes().len();
        if length > limits.maximum_frame_bytes {
            return Err(Error::Limit {
                requested: length,
                maximum: limits.maximum_frame_bytes,
            });
        }
        let header = frame.facts().header;
        if header.family != Ac3Family::Eac3 {
            return Err(Error::Unsupported("E-AC-3 BSI requires E-AC-3 frame"));
        }
        let Ac3FrameDetails::Eac3 {
            stream_type, fscod, ..
        } = header.details
        else {
            return Err(Error::Invalid("E-AC-3 frame/header family disagreement"));
        };
        if stream_type == 3 {
            return Err(Error::Unsupported("reserved E-AC-3 stream type BSI"));
        }
        if length != header.frame_bytes || length < 6 {
            return Err(Error::Invalid("E-AC-3 complete frame envelope"));
        }
        // Fixed reads of at most four already-owned bytes; no payload scan.
        let bytes = frame.bytes();
        let auxiliary = bytes[length - 3] & 2 != 0;
        let trailer = Eac3Trailer {
            auxiliary_data_present: auxiliary,
            auxiliary_length_code: if auxiliary {
                Some((u16::from(bytes[length - 4]) << 6) | (u16::from(bytes[length - 3]) >> 2))
            } else {
                None
            },
            encoder_info: bytes[length - 3] & 1 != 0,
            crc2_word: frame.crc2_word(),
        };
        let before_tail = length
            .checked_mul(8)
            .and_then(|value| value.checked_sub(if auxiliary { 32 } else { 18 }))
            .ok_or(Error::Overflow)?;
        let position = usize::from(header.probe_bits);
        if position > before_tail {
            return Err(Error::Invalid(
                "E-AC-3 prefix overlaps auxiliary/error tail",
            ));
        }
        let prefix_bits = position.checked_sub(16).ok_or(Error::Overflow)?;
        if prefix_bits > limits.maximum_bsi_bits {
            return Err(Error::Limit {
                requested: prefix_bits,
                maximum: limits.maximum_bsi_bits,
            });
        }
        let facts = Eac3BsiFacts {
            end_bit: 0,
            prefix_end_bit: position,
            mixing_metadata: None,
            mixing_definition: None,
            informational_metadata: None,
            bitstream_mode: None,
            copyright: None,
            original: None,
            surround_mode: None,
            headphone_mode: None,
            surround_ex_mode: None,
            production: None,
            second_production: None,
            source_fscod: None,
            converter_sync: None,
            block_id: None,
            effective_block_id: None,
            converted_frame_size_code: None,
            additional_bsi: None,
            trailer,
        };
        Ok(Self {
            frame: Some(frame),
            limits,
            stream_type,
            mode: header.acmod,
            lfe: header.lfe_on,
            fscod,
            blocks: header.blocks,
            phase: Phase::MixPresent,
            position,
            before_tail,
            partial_seen: 0,
            partial_width: 0,
            partial_value: 0,
            mix_start: 0,
            info_start: 0,
            block: 0,
            additional_remaining: 0,
            facts,
            complete: false,
            failure: None,
        })
    }

    pub fn position_bits(&self) -> usize {
        self.position
    }

    fn read(
        &mut self,
        width: u8,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<u16>> {
        if width == 0 || width > 16 {
            return Err(Error::Invalid("E-AC-3 BSI field width"));
        }
        if self.partial_width == 0 {
            self.partial_width = width;
        }
        if self.partial_width != width {
            return Err(Error::Invalid("E-AC-3 partial field state"));
        }
        while self.partial_seen < width {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(None);
            }
            if self.position >= self.before_tail {
                return Err(Error::Invalid("E-AC-3 BSI overlaps auxiliary/error tail"));
            }
            let used = self
                .position
                .checked_sub(16)
                .and_then(|value| value.checked_add(1))
                .ok_or(Error::Overflow)?;
            if used > self.limits.maximum_bsi_bits {
                return Err(Error::Limit {
                    requested: used,
                    maximum: self.limits.maximum_bsi_bits,
                });
            }
            let frame = self
                .frame
                .as_ref()
                .ok_or(Error::Invalid("missing E-AC-3 frame"))?;
            let value = (frame.bytes()[self.position / 8] >> (7 - self.position % 8)) & 1;
            self.partial_value = (self.partial_value << 1) | u16::from(value);
            self.partial_seen += 1;
            self.position = self.position.checked_add(1).ok_or(Error::Overflow)?;
        }
        let result = self.partial_value;
        self.partial_seen = 0;
        self.partial_width = 0;
        self.partial_value = 0;
        Ok(Some(result))
    }

    fn after_mix_channels(&self) -> Phase {
        if self.stream_type == 0 {
            Phase::ProgrammeFlag
        } else {
            Phase::MixDone
        }
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Eac3Bsi>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.step_inner(work, cancel);
        if let Err(error) = result {
            self.failure = Some(error);
            self.frame = None;
        }
        result
    }

    fn step_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Eac3Bsi>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.complete {
            return Ok(BuildStep::Complete);
        }
        // A field can span arbitrarily many calls, including one-bit grants.
        macro_rules! field {
            ($width:expr) => {
                match self.read($width, work, cancel)? {
                    Some(value) => value,
                    None => return Ok(BuildStep::Yield),
                }
            };
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::MixPresent => {
                    if field!(1) != 0 {
                        self.mix_start = self.position - 1;
                        self.phase = Phase::MixDownmix;
                    } else {
                        self.phase = Phase::InfoPresent;
                    }
                }
                Phase::MixDownmix => {
                    if self.mode > 2 {
                        field!(2);
                    }
                    self.phase = Phase::MixCenter;
                }
                Phase::MixCenter => {
                    if matches!(self.mode, 3 | 5 | 7) {
                        field!(6);
                    }
                    self.phase = Phase::MixSurround;
                }
                Phase::MixSurround => {
                    if self.mode >= 4 {
                        field!(6);
                    }
                    self.phase = Phase::MixLfeFlag;
                }
                Phase::MixLfeFlag => {
                    if self.lfe && field!(1) != 0 {
                        self.phase = Phase::MixLfeValue;
                    } else {
                        self.phase = self.after_mix_channels();
                    }
                }
                Phase::MixLfeValue => {
                    field!(5);
                    self.phase = self.after_mix_channels();
                }
                Phase::ProgrammeFlag => {
                    self.phase = if field!(1) != 0 {
                        Phase::ProgrammeValue
                    } else {
                        Phase::Programme2Flag
                    };
                }
                Phase::ProgrammeValue => {
                    field!(6);
                    self.phase = Phase::Programme2Flag;
                }
                Phase::Programme2Flag => {
                    if self.mode == 0 && field!(1) != 0 {
                        self.phase = Phase::Programme2Value;
                    } else {
                        self.phase = Phase::ExternalFlag;
                    }
                }
                Phase::Programme2Value => {
                    field!(6);
                    self.phase = Phase::ExternalFlag;
                }
                Phase::ExternalFlag => {
                    self.phase = if field!(1) != 0 {
                        Phase::ExternalValue
                    } else {
                        Phase::MixDefinition
                    };
                }
                Phase::ExternalValue => {
                    field!(6);
                    self.phase = Phase::MixDefinition;
                }
                Phase::MixDefinition => {
                    let definition = u8::try_from(field!(2)).map_err(|_| Error::Overflow)?;
                    self.facts.mixing_definition = Some(definition);
                    self.phase = match definition {
                        0 => Phase::PanFlag,
                        1 | 2 => Phase::MixControl,
                        _ => return Err(Error::Unsupported("E-AC-3 flexible mixing definition 3")),
                    };
                }
                Phase::MixControl => {
                    field!(if self.facts.mixing_definition == Some(1) {
                        5
                    } else {
                        12
                    });
                    self.phase = Phase::PanFlag;
                }
                Phase::PanFlag => {
                    if self.mode < 2 && field!(1) != 0 {
                        self.phase = Phase::PanValue;
                    } else {
                        self.phase = Phase::Pan2Flag;
                    }
                }
                Phase::PanValue => {
                    field!(14);
                    self.phase = Phase::Pan2Flag;
                }
                Phase::Pan2Flag => {
                    if self.mode == 0 && field!(1) != 0 {
                        self.phase = Phase::Pan2Value;
                    } else {
                        self.phase = Phase::FrameMixFlag;
                    }
                }
                Phase::Pan2Value => {
                    field!(14);
                    self.phase = Phase::FrameMixFlag;
                }
                Phase::FrameMixFlag => {
                    if field!(1) != 0 {
                        self.block = 0;
                        self.phase = if self.blocks == 1 {
                            Phase::BlockValue
                        } else {
                            Phase::BlockFlag
                        };
                    } else {
                        self.phase = Phase::MixDone;
                    }
                }
                Phase::BlockFlag => {
                    if self.block == self.blocks {
                        self.phase = Phase::MixDone;
                    } else if field!(1) != 0 {
                        self.phase = Phase::BlockValue;
                    } else {
                        self.block += 1;
                    }
                }
                Phase::BlockValue => {
                    field!(5);
                    self.block += 1;
                    self.phase = if self.block == self.blocks {
                        Phase::MixDone
                    } else {
                        Phase::BlockFlag
                    };
                }
                Phase::MixDone => {
                    self.facts.mixing_metadata = Some(Eac3BitRange {
                        start: self.mix_start,
                        bits: self.position - self.mix_start,
                    });
                    self.phase = Phase::InfoPresent;
                }
                Phase::InfoPresent => {
                    if field!(1) != 0 {
                        self.info_start = self.position - 1;
                        self.phase = Phase::InfoService;
                    } else {
                        self.phase = Phase::ConverterSync;
                    }
                }
                Phase::InfoService => {
                    self.facts.bitstream_mode =
                        Some(u8::try_from(field!(3)).map_err(|_| Error::Overflow)?);
                    self.phase = Phase::InfoCopyright;
                }
                Phase::InfoCopyright => {
                    self.facts.copyright = Some(field!(1) != 0);
                    self.phase = Phase::InfoOriginal;
                }
                Phase::InfoOriginal => {
                    self.facts.original = Some(field!(1) != 0);
                    self.phase = Phase::InfoSurround;
                }
                Phase::InfoSurround => {
                    if self.mode == 2 {
                        self.facts.surround_mode =
                            Some(u8::try_from(field!(2)).map_err(|_| Error::Overflow)?);
                    }
                    self.phase = Phase::InfoHeadphone;
                }
                Phase::InfoHeadphone => {
                    if self.mode == 2 {
                        self.facts.headphone_mode =
                            Some(u8::try_from(field!(2)).map_err(|_| Error::Overflow)?);
                    }
                    self.phase = Phase::InfoSurroundEx;
                }
                Phase::InfoSurroundEx => {
                    if self.mode >= 6 {
                        self.facts.surround_ex_mode =
                            Some(u8::try_from(field!(2)).map_err(|_| Error::Overflow)?);
                    }
                    self.phase = Phase::ProductionFlag;
                }
                Phase::ProductionFlag => {
                    self.phase = if field!(1) != 0 {
                        Phase::ProductionValue
                    } else {
                        Phase::Production2Flag
                    };
                }
                Phase::ProductionValue => {
                    self.facts.production =
                        Some(u8::try_from(field!(8)).map_err(|_| Error::Overflow)?);
                    self.phase = Phase::Production2Flag;
                }
                Phase::Production2Flag => {
                    if self.mode == 0 && field!(1) != 0 {
                        self.phase = Phase::Production2Value;
                    } else {
                        self.phase = Phase::SourceRate;
                    }
                }
                Phase::Production2Value => {
                    self.facts.second_production =
                        Some(u8::try_from(field!(8)).map_err(|_| Error::Overflow)?);
                    self.phase = Phase::SourceRate;
                }
                Phase::SourceRate => {
                    if self.fscod != 3 {
                        self.facts.source_fscod = Some(field!(1) != 0);
                    }
                    self.phase = Phase::InfoDone;
                }
                Phase::InfoDone => {
                    self.facts.informational_metadata = Some(Eac3BitRange {
                        start: self.info_start,
                        bits: self.position - self.info_start,
                    });
                    self.phase = Phase::ConverterSync;
                }
                Phase::ConverterSync => {
                    if self.stream_type == 0 && self.blocks != 6 {
                        self.facts.converter_sync = Some(field!(1) != 0);
                    }
                    self.phase = Phase::ConvertedBlock;
                }
                Phase::ConvertedBlock => {
                    if self.stream_type != 2 {
                        self.phase = Phase::AdditionalFlag;
                    } else if self.blocks == 6 {
                        self.facts.effective_block_id = Some(true);
                        self.phase = Phase::ConvertedSize;
                    } else {
                        let block_id = field!(1) != 0;
                        self.facts.block_id = Some(block_id);
                        self.facts.effective_block_id = Some(block_id);
                        self.phase = if block_id {
                            Phase::ConvertedSize
                        } else {
                            Phase::AdditionalFlag
                        };
                    }
                }
                Phase::ConvertedSize => {
                    self.facts.converted_frame_size_code =
                        Some(u8::try_from(field!(6)).map_err(|_| Error::Overflow)?);
                    self.phase = Phase::AdditionalFlag;
                }
                Phase::AdditionalFlag => {
                    self.phase = if field!(1) != 0 {
                        Phase::AdditionalLength
                    } else {
                        Phase::Finish
                    };
                }
                Phase::AdditionalLength => {
                    self.additional_remaining = u8::try_from(field!(6))
                        .map_err(|_| Error::Overflow)?
                        .checked_add(1)
                        .ok_or(Error::Overflow)?;
                    self.facts.additional_bsi = Some(Eac3BitRange {
                        start: self.position,
                        bits: usize::from(self.additional_remaining) * 8,
                    });
                    self.phase = Phase::AdditionalBytes;
                }
                Phase::AdditionalBytes => {
                    if self.additional_remaining == 0 {
                        self.phase = Phase::Finish;
                    } else {
                        field!(8);
                        self.additional_remaining -= 1;
                    }
                }
                Phase::Finish => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    self.facts.end_bit = self.position;
                    let frame = self
                        .frame
                        .take()
                        .ok_or(Error::Invalid("missing E-AC-3 BSI output frame"))?;
                    self.complete = true;
                    return Ok(BuildStep::Ready(Eac3Bsi {
                        frame,
                        facts: self.facts,
                    }));
                }
            }
        }
    }
}
