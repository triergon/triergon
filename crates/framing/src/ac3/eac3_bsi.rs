//! The complete bit stream information of an E-AC-3 frame (ATSC A/52:2018 Annex E,
//! Table E1.2) with the source bits of each part. Coded audio, CRCs and the
//! grouping of substreams are not checked.
use crate::{
    BuildStep,
    ac3::{Ac3Family, Ac3Frame, Ac3FrameDetails},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

/// Frame-relative MSB-first bit range. Physical alignment is never discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eac3BitRange {
    start: usize,
    bits: usize,
}

impl Eac3BitRange {
    /// The first bit of the range, counted from the first bit of the syncword.
    pub const fn start_bit(self) -> usize {
        self.start
    }
    /// The length of the range in bits.
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
    /// The frame the span lies in.
    pub fn frame(&self) -> &Ac3Frame {
        &self.frame
    }
    /// The span's bits within [`frame`](Self::frame).
    pub const fn range(&self) -> Eac3BitRange {
        self.range
    }
    /// Bit `index` of the span; 0 is its first bit. Fails when `index` is not below the
    /// span's bit count.
    pub fn bit(&self, index: usize) -> Result<bool> {
        if index >= self.range.bits {
            return Err(Error::Invalid("E-AC-3 bit span index"));
        }
        let position = self.range.start.checked_add(index).ok_or(Error::Overflow)?;
        Ok(self.frame.bytes()[position / 8] & (1 << (7 - position % 8)) != 0)
    }
    /// Source bytes covering the span. The first/last bytes can also contain adjacent
    /// fields; range() retains the exact bit boundary. Never fails: a span lies inside
    /// its frame.
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
    /// `auxdatae`: whether the frame carries auxiliary data.
    pub auxiliary_data_present: bool,
    /// `auxdatal`, 14 bits: the length of the auxiliary data in bits. `None` when
    /// `auxdatae` is 0.
    pub auxiliary_length_code: Option<u16>,
    /// `encinfo`, the bit before `crc2`.
    pub encoder_info: bool,
    /// `crc2`, the last 16 bits of the frame, as coded; it is not checked.
    pub crc2_word: u16,
}

/// Caller limits of an [`Eac3BsiParser`]. Each must be nonzero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eac3BsiLimits {
    /// The longest frame accepted, in bytes.
    pub maximum_frame_bytes: usize,
    /// BSI bits after the sixteen-bit syncword, including the fixed BSI prefix.
    pub maximum_bsi_bits: usize,
}

/// Raw coded values and exact group spans. None is physically absent in the
/// selected context; an explicitly coded zero remains Some(0)/Some(false).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eac3BsiFacts {
    /// Bit position just past the last field of the bit stream information,
    /// counted from the first bit of the syncword.
    pub end_bit: usize,
    /// Bit position where the fields of the frame's header end and the fields
    /// below begin: the header's [`probe_bits`](crate::ac3::Ac3FrameHeader::probe_bits).
    pub prefix_end_bit: usize,
    /// Range of the mixing metadata, from `mixmdate` through its last field.
    /// `None` when `mixmdate` is 0.
    pub mixing_metadata: Option<Eac3BitRange>,
    /// `mixdef`: 0, 1 or 2. `None` unless an independent frame (`strmtyp` 0)
    /// carries mixing metadata.
    pub mixing_definition: Option<u8>,
    /// Range of the informational metadata, from `infomdate` through its last
    /// field. `None` when `infomdate` is 0.
    pub informational_metadata: Option<Eac3BitRange>,
    /// `bsmod`, 3 bits; coded with the informational metadata.
    pub bitstream_mode: Option<u8>,
    /// `copyrightb`; coded with the informational metadata.
    pub copyright: Option<bool>,
    /// `origbs`; coded with the informational metadata.
    pub original: Option<bool>,
    /// `dsurmod`, 2 bits; coded with the informational metadata of a frame whose
    /// `acmod` is 2.
    pub surround_mode: Option<u8>,
    /// `dheadphonmod`, 2 bits; coded with the informational metadata of a frame
    /// whose `acmod` is 2.
    pub headphone_mode: Option<u8>,
    /// `dsurexmod`, 2 bits; coded with the informational metadata of a frame whose
    /// `acmod` is 6 or 7.
    pub surround_ex_mode: Option<u8>,
    /// Packed mixlevel(5), roomtyp(2), adconvtyp(1), without normalization.
    pub production: Option<u8>,
    /// `mixlevel2`, `roomtyp2` and `adconvtyp2` of a dual-mono frame (`acmod` 0),
    /// packed as `production` is.
    pub second_production: Option<u8>,
    /// `sourcefscod`; coded with the informational metadata unless `fscod` is 3.
    pub source_fscod: Option<bool>,
    /// `convsync`; coded in an independent frame (`strmtyp` 0) of fewer than six
    /// audio blocks.
    pub converter_sync: Option<bool>,
    /// None for the six-block Type-2 inferred block ID; distinct from Some(true).
    pub block_id: Option<bool>,
    /// `blkid` of a frame converted from AC-3 (`strmtyp` 2), whether coded or
    /// implied by six audio blocks, which give `Some(true)`. `None` for the other
    /// stream types.
    pub effective_block_id: Option<bool>,
    /// `frmsizecod`, 6 bits; coded when `effective_block_id` is `Some(true)`.
    pub converted_frame_size_code: Option<u8>,
    /// Payload bits only, excluding addbsie and addbsil. May be byte-unaligned.
    pub additional_bsi: Option<Eac3BitRange>,
    /// The fields that end the frame.
    pub trailer: Eac3Trailer,
}

/// An E-AC-3 frame with the bit stream information an [`Eac3BsiParser`] read
/// from it.
#[derive(Clone, Debug)]
pub struct Eac3Bsi {
    frame: Ac3Frame,
    facts: Eac3BsiFacts,
}

impl Eac3Bsi {
    /// The frame the information was read from.
    pub fn frame(&self) -> &Ac3Frame {
        &self.frame
    }
    /// The values and bit ranges read.
    pub const fn facts(&self) -> Eac3BsiFacts {
        self.facts
    }
    /// An owned view of `range`, which shares the frame's bytes. Fails when `range`
    /// starts inside the syncword or ends after [`Eac3BsiFacts::end_bit`].
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

/// The next field to read, by syntax group of the bit stream information (ATSC
/// A/52:2018, E.2.2.2).
#[derive(Clone, Copy)]
enum Phase {
    Mixing(MixField),
    Programme(ProgrammeField),
    Info(InfoField),
    Conversion(ConversionField),
    Additional(AdditionalField),
    Finish,
}

/// The mixing metadata every stream type carries: `mixmdate` and the mix levels.
#[derive(Clone, Copy)]
enum MixField {
    Present,
    Downmix,
    Center,
    Surround,
    LfeFlag,
    LfeValue,
    /// The end of the mixing metadata: its range is recorded.
    Done,
}

/// The mixing metadata of an independent stream (`strmtyp` 0): programme scale
/// factors, mixing definition, pan and frame mixing information.
#[derive(Clone, Copy)]
enum ProgrammeField {
    ScaleFlag,
    ScaleValue,
    Scale2Flag,
    Scale2Value,
    ExternalFlag,
    ExternalValue,
    Definition,
    Control,
    PanFlag,
    PanValue,
    Pan2Flag,
    Pan2Value,
    FrameMixFlag,
    BlockFlag,
    BlockValue,
}

/// The informational metadata: `infomdate` and the fields it enables.
#[derive(Clone, Copy)]
enum InfoField {
    Present,
    Service,
    Copyright,
    Original,
    Surround,
    Headphone,
    SurroundEx,
    ProductionFlag,
    ProductionValue,
    Production2Flag,
    Production2Value,
    SourceRate,
    /// The end of the informational metadata: its range is recorded.
    Done,
}

/// The fields of a frame converted from AC-3: `convsync`, `blkid`, `frmsizecod`.
#[derive(Clone, Copy)]
enum ConversionField {
    Sync,
    Block,
    Size,
}

/// `addbsie`, `addbsil` and the additional bit stream information.
#[derive(Clone, Copy)]
enum AdditionalField {
    Flag,
    Length,
    Bytes,
}

/// Where a parse stands. The frame is held only until it is returned.
enum State {
    Reading(Ac3Frame),
    Complete,
    Failed(Error),
}

/// What one step reads: the frame's bytes, with the caller's work and cancellation.
struct Input<'a> {
    bytes: &'a [u8],
    work: &'a mut WorkBudget,
    cancel: &'a AtomicBool,
}

/// The value of the next field of `$width` bits, or `Ok(None)` from the enclosing
/// function when the work runs out inside it: a field can span any number of calls.
macro_rules! field {
    ($walk:expr, $input:expr, $width:expr) => {
        match $walk.reader.read($input, $width)? {
            Some(value) => value,
            None => return Ok(None),
        }
    };
}

/// The bit position and the bits of the field read so far.
struct FieldReader {
    position: usize,
    before_tail: usize,
    maximum_bsi_bits: usize,
    partial_seen: u8,
    partial_width: u8,
    partial_value: u16,
}

/// What the walk over the fields keeps: the header fields that select the syntax,
/// the reader and the facts read.
struct Walk {
    stream_type: u8,
    mode: u8,
    lfe: bool,
    fscod: u8,
    blocks: u8,
    phase: Phase,
    reader: FieldReader,
    mix_start: usize,
    info_start: usize,
    block: u8,
    additional_remaining: u8,
    facts: Eac3BsiFacts,
}

/// Reads the bit stream information of a complete E-AC-3 frame from `Ac3FrameReader`,
/// charged to the same pool; the reader handles input and the frame's storage. It
/// allocates nothing. Mixing definitions 0, 1 and 2 are read; definition 3 is
/// [`Error::Unsupported`].
pub struct Eac3BsiParser {
    limits: Eac3BsiLimits,
    state: State,
    walk: Walk,
}

impl std::fmt::Debug for Eac3BsiParser {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failure = match self.state {
            State::Failed(error) => Some(error),
            State::Reading(_) | State::Complete => None,
        };
        formatter
            .debug_struct("Eac3BsiParser")
            .field("limits", &self.limits)
            .field("position_bits", &self.walk.reader.position)
            .field("complete", &matches!(self.state, State::Complete))
            .field("failure", &failure)
            .finish_non_exhaustive()
    }
}

impl Eac3BsiParser {
    /// A parser of `frame` that starts reading where the frame's header ends.
    ///
    /// Fails when a limit is zero; when `frame` is not charged to `pool`, is longer
    /// than `maximum_frame_bytes`, is an AC-3 frame, has the reserved `strmtyp` 3 or
    /// does not hold exactly the bytes its header declares; and when the header fields
    /// alone exceed `maximum_bsi_bits` or reach the auxiliary data and CRC fields that
    /// end the frame.
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
            limits,
            state: State::Reading(frame),
            walk: Walk {
                stream_type,
                mode: header.acmod,
                lfe: header.lfe_on,
                fscod,
                blocks: header.blocks,
                phase: Phase::Mixing(MixField::Present),
                reader: FieldReader {
                    position,
                    before_tail,
                    maximum_bsi_bits: limits.maximum_bsi_bits,
                    partial_seen: 0,
                    partial_width: 0,
                    partial_value: 0,
                },
                mix_start: 0,
                info_start: 0,
                block: 0,
                additional_remaining: 0,
                facts,
            },
        })
    }

    /// Bit position of the next field to read, counted from the first bit of the
    /// syncword.
    pub fn position_bits(&self) -> usize {
        self.walk.reader.position
    }

    /// Advances by at most the work left in `work`, where each bit read costs one unit,
    /// and returns the frame with its bit stream information once that is read. Fails
    /// when the fields reach the auxiliary data and CRC fields that end the frame or
    /// exceed `maximum_bsi_bits`, `mixdef` is 3, or `cancel` is set. Every later call
    /// returns the same error.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Eac3Bsi>> {
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        let result = self.read(work, cancel);
        if let Err(error) = result {
            self.state = State::Failed(error);
        }
        result
    }

    fn read(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStep<Eac3Bsi>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let State::Reading(frame) = &self.state else {
            return Ok(BuildStep::Complete);
        };
        let input = &mut Input {
            bytes: frame.bytes().as_ref(),
            work,
            cancel,
        };
        let walk = &mut self.walk;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let next = match walk.phase {
                Phase::Mixing(field) => walk.mixing(input, field)?,
                Phase::Programme(field) => walk.programme(input, field)?,
                Phase::Info(field) => walk.informational(input, field)?,
                Phase::Conversion(field) => walk.conversion(input, field)?,
                Phase::Additional(field) => walk.additional(input, field)?,
                Phase::Finish => {
                    if !input.work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    walk.facts.end_bit = walk.reader.position;
                    let bsi = Eac3Bsi {
                        frame: frame.clone(),
                        facts: walk.facts,
                    };
                    self.state = State::Complete;
                    return Ok(BuildStep::Ready(bsi));
                }
            };
            match next {
                Some(phase) => walk.phase = phase,
                None => return Ok(BuildStep::Yield),
            }
        }
    }
}

impl FieldReader {
    /// The next field of `width` bits once all of them are read, one unit of work per
    /// bit; `None` when the work runs out first.
    fn read(&mut self, input: &mut Input<'_>, width: u8) -> Result<Option<u16>> {
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
            if input.cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !input.work.try_charge(1) {
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
            if used > self.maximum_bsi_bits {
                return Err(Error::Limit {
                    requested: used,
                    maximum: self.maximum_bsi_bits,
                });
            }
            let value = (input.bytes[self.position / 8] >> (7 - self.position % 8)) & 1;
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
}

/// Each group reads one field per call and returns the phase after it; `None` when
/// the work ran out inside the field.
impl Walk {
    fn after_mix_channels(&self) -> Phase {
        if self.stream_type == 0 {
            Phase::Programme(ProgrammeField::ScaleFlag)
        } else {
            Phase::Mixing(MixField::Done)
        }
    }

    /// `mixmdate`, the downmix and LFE mix levels, and the end of the mixing metadata.
    fn mixing(&mut self, input: &mut Input<'_>, field: MixField) -> Result<Option<Phase>> {
        let next = match field {
            MixField::Present => {
                if field!(self, input, 1) != 0 {
                    self.mix_start = self.reader.position - 1;
                    Phase::Mixing(MixField::Downmix)
                } else {
                    Phase::Info(InfoField::Present)
                }
            }
            MixField::Downmix => {
                if self.mode > 2 {
                    field!(self, input, 2);
                }
                Phase::Mixing(MixField::Center)
            }
            MixField::Center => {
                if matches!(self.mode, 3 | 5 | 7) {
                    field!(self, input, 6);
                }
                Phase::Mixing(MixField::Surround)
            }
            MixField::Surround => {
                if self.mode >= 4 {
                    field!(self, input, 6);
                }
                Phase::Mixing(MixField::LfeFlag)
            }
            MixField::LfeFlag => {
                if self.lfe && field!(self, input, 1) != 0 {
                    Phase::Mixing(MixField::LfeValue)
                } else {
                    self.after_mix_channels()
                }
            }
            MixField::LfeValue => {
                field!(self, input, 5);
                self.after_mix_channels()
            }
            MixField::Done => {
                self.facts.mixing_metadata = Some(Eac3BitRange {
                    start: self.mix_start,
                    bits: self.reader.position - self.mix_start,
                });
                Phase::Info(InfoField::Present)
            }
        };
        Ok(Some(next))
    }

    /// The mixing metadata of an independent stream.
    fn programme(&mut self, input: &mut Input<'_>, field: ProgrammeField) -> Result<Option<Phase>> {
        let next = match field {
            ProgrammeField::ScaleFlag => {
                if field!(self, input, 1) != 0 {
                    ProgrammeField::ScaleValue
                } else {
                    ProgrammeField::Scale2Flag
                }
            }
            ProgrammeField::ScaleValue => {
                field!(self, input, 6);
                ProgrammeField::Scale2Flag
            }
            ProgrammeField::Scale2Flag => {
                if self.mode == 0 && field!(self, input, 1) != 0 {
                    ProgrammeField::Scale2Value
                } else {
                    ProgrammeField::ExternalFlag
                }
            }
            ProgrammeField::Scale2Value => {
                field!(self, input, 6);
                ProgrammeField::ExternalFlag
            }
            ProgrammeField::ExternalFlag => {
                if field!(self, input, 1) != 0 {
                    ProgrammeField::ExternalValue
                } else {
                    ProgrammeField::Definition
                }
            }
            ProgrammeField::ExternalValue => {
                field!(self, input, 6);
                ProgrammeField::Definition
            }
            ProgrammeField::Definition => {
                let definition =
                    u8::try_from(field!(self, input, 2)).map_err(|_| Error::Overflow)?;
                self.facts.mixing_definition = Some(definition);
                match definition {
                    0 => ProgrammeField::PanFlag,
                    1 | 2 => ProgrammeField::Control,
                    _ => return Err(Error::Unsupported("E-AC-3 flexible mixing definition 3")),
                }
            }
            ProgrammeField::Control => {
                field!(
                    self,
                    input,
                    if self.facts.mixing_definition == Some(1) {
                        5
                    } else {
                        12
                    }
                );
                ProgrammeField::PanFlag
            }
            ProgrammeField::PanFlag => {
                if self.mode < 2 && field!(self, input, 1) != 0 {
                    ProgrammeField::PanValue
                } else {
                    ProgrammeField::Pan2Flag
                }
            }
            ProgrammeField::PanValue => {
                field!(self, input, 14);
                ProgrammeField::Pan2Flag
            }
            ProgrammeField::Pan2Flag => {
                if self.mode == 0 && field!(self, input, 1) != 0 {
                    ProgrammeField::Pan2Value
                } else {
                    ProgrammeField::FrameMixFlag
                }
            }
            ProgrammeField::Pan2Value => {
                field!(self, input, 14);
                ProgrammeField::FrameMixFlag
            }
            ProgrammeField::FrameMixFlag => {
                if field!(self, input, 1) == 0 {
                    return Ok(Some(Phase::Mixing(MixField::Done)));
                }
                self.block = 0;
                if self.blocks == 1 {
                    ProgrammeField::BlockValue
                } else {
                    ProgrammeField::BlockFlag
                }
            }
            ProgrammeField::BlockFlag => {
                if self.block == self.blocks {
                    return Ok(Some(Phase::Mixing(MixField::Done)));
                }
                if field!(self, input, 1) != 0 {
                    ProgrammeField::BlockValue
                } else {
                    self.block += 1;
                    ProgrammeField::BlockFlag
                }
            }
            ProgrammeField::BlockValue => {
                field!(self, input, 5);
                self.block += 1;
                if self.block == self.blocks {
                    return Ok(Some(Phase::Mixing(MixField::Done)));
                }
                ProgrammeField::BlockFlag
            }
        };
        Ok(Some(Phase::Programme(next)))
    }

    /// `infomdate` and the informational metadata it enables.
    fn informational(&mut self, input: &mut Input<'_>, field: InfoField) -> Result<Option<Phase>> {
        let next = match field {
            InfoField::Present => {
                if field!(self, input, 1) == 0 {
                    return Ok(Some(Phase::Conversion(ConversionField::Sync)));
                }
                self.info_start = self.reader.position - 1;
                InfoField::Service
            }
            InfoField::Service => {
                self.facts.bitstream_mode =
                    Some(u8::try_from(field!(self, input, 3)).map_err(|_| Error::Overflow)?);
                InfoField::Copyright
            }
            InfoField::Copyright => {
                self.facts.copyright = Some(field!(self, input, 1) != 0);
                InfoField::Original
            }
            InfoField::Original => {
                self.facts.original = Some(field!(self, input, 1) != 0);
                InfoField::Surround
            }
            InfoField::Surround => {
                if self.mode == 2 {
                    self.facts.surround_mode =
                        Some(u8::try_from(field!(self, input, 2)).map_err(|_| Error::Overflow)?);
                }
                InfoField::Headphone
            }
            InfoField::Headphone => {
                if self.mode == 2 {
                    self.facts.headphone_mode =
                        Some(u8::try_from(field!(self, input, 2)).map_err(|_| Error::Overflow)?);
                }
                InfoField::SurroundEx
            }
            InfoField::SurroundEx => {
                if self.mode >= 6 {
                    self.facts.surround_ex_mode =
                        Some(u8::try_from(field!(self, input, 2)).map_err(|_| Error::Overflow)?);
                }
                InfoField::ProductionFlag
            }
            InfoField::ProductionFlag => {
                if field!(self, input, 1) != 0 {
                    InfoField::ProductionValue
                } else {
                    InfoField::Production2Flag
                }
            }
            InfoField::ProductionValue => {
                self.facts.production =
                    Some(u8::try_from(field!(self, input, 8)).map_err(|_| Error::Overflow)?);
                InfoField::Production2Flag
            }
            InfoField::Production2Flag => {
                if self.mode == 0 && field!(self, input, 1) != 0 {
                    InfoField::Production2Value
                } else {
                    InfoField::SourceRate
                }
            }
            InfoField::Production2Value => {
                self.facts.second_production =
                    Some(u8::try_from(field!(self, input, 8)).map_err(|_| Error::Overflow)?);
                InfoField::SourceRate
            }
            InfoField::SourceRate => {
                if self.fscod != 3 {
                    self.facts.source_fscod = Some(field!(self, input, 1) != 0);
                }
                InfoField::Done
            }
            InfoField::Done => {
                self.facts.informational_metadata = Some(Eac3BitRange {
                    start: self.info_start,
                    bits: self.reader.position - self.info_start,
                });
                return Ok(Some(Phase::Conversion(ConversionField::Sync)));
            }
        };
        Ok(Some(Phase::Info(next)))
    }

    /// `convsync`, and `blkid` and `frmsizecod` of a frame converted from AC-3.
    fn conversion(
        &mut self,
        input: &mut Input<'_>,
        field: ConversionField,
    ) -> Result<Option<Phase>> {
        let next = match field {
            ConversionField::Sync => {
                if self.stream_type == 0 && self.blocks != 6 {
                    self.facts.converter_sync = Some(field!(self, input, 1) != 0);
                }
                Phase::Conversion(ConversionField::Block)
            }
            ConversionField::Block => {
                if self.stream_type != 2 {
                    Phase::Additional(AdditionalField::Flag)
                } else if self.blocks == 6 {
                    self.facts.effective_block_id = Some(true);
                    Phase::Conversion(ConversionField::Size)
                } else {
                    let block_id = field!(self, input, 1) != 0;
                    self.facts.block_id = Some(block_id);
                    self.facts.effective_block_id = Some(block_id);
                    if block_id {
                        Phase::Conversion(ConversionField::Size)
                    } else {
                        Phase::Additional(AdditionalField::Flag)
                    }
                }
            }
            ConversionField::Size => {
                self.facts.converted_frame_size_code =
                    Some(u8::try_from(field!(self, input, 6)).map_err(|_| Error::Overflow)?);
                Phase::Additional(AdditionalField::Flag)
            }
        };
        Ok(Some(next))
    }

    /// `addbsie`, `addbsil` and the additional bit stream information, one byte per
    /// call.
    fn additional(
        &mut self,
        input: &mut Input<'_>,
        field: AdditionalField,
    ) -> Result<Option<Phase>> {
        let next = match field {
            AdditionalField::Flag => {
                if field!(self, input, 1) != 0 {
                    Phase::Additional(AdditionalField::Length)
                } else {
                    Phase::Finish
                }
            }
            AdditionalField::Length => {
                self.additional_remaining = u8::try_from(field!(self, input, 6))
                    .map_err(|_| Error::Overflow)?
                    .checked_add(1)
                    .ok_or(Error::Overflow)?;
                self.facts.additional_bsi = Some(Eac3BitRange {
                    start: self.reader.position,
                    bits: usize::from(self.additional_remaining) * 8,
                });
                Phase::Additional(AdditionalField::Bytes)
            }
            AdditionalField::Bytes => {
                if self.additional_remaining == 0 {
                    Phase::Finish
                } else {
                    field!(self, input, 8);
                    self.additional_remaining -= 1;
                    Phase::Additional(AdditionalField::Bytes)
                }
            }
        };
        Ok(Some(next))
    }
}
