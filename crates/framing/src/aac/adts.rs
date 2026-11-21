//! ADTS: the header, complete AAC LC frames of one raw data block, and the writer
//! that wraps a raw access unit. Payload and CRC are kept as bytes, neither decoded
//! nor checked. No timing is derived.
use crate::retryable;
use crate::{
    BuildStep,
    aac::{AacAscFacts, AacSource, indexed_frequency},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

mod carrier;
mod stream;
pub use carrier::{
    AacAdtsCarrier, AacAdtsCarrierBoundary, AacAdtsCarrierLimits, AacAdtsCarrierReader,
};
pub use stream::{AacAdtsStreamLimits, AacAdtsStreamReader};

fn validate_selected_header(h: AacAdtsHeader) -> Result<()> {
    if h.syncword() != 0xfff || h.layer() != 0 {
        return Err(Error::Invalid("ADTS sync/layer"));
    }
    if h.coded_frequency_hz().is_none() {
        return Err(Error::Invalid("ADTS sampling index"));
    }
    if h.profile_code() != 1 {
        return Err(Error::Unsupported("ADTS non-LC profile"));
    }
    if h.raw_blocks_minus_one() != 0 {
        return Err(Error::Unsupported("ADTS multiple raw-data-block layout"));
    }
    Ok(())
}

/// The seven bytes of `adts_fixed_header()` and `adts_variable_header()`. Nothing is
/// validated: every field reads as coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAdtsHeader {
    prefix: [u8; 7],
}

impl AacAdtsHeader {
    /// The seven header bytes.
    pub const fn raw_prefix(self) -> [u8; 7] {
        self.prefix
    }
    /// `syncword`, 12 bits; `0xfff` in a valid header.
    pub const fn syncword(self) -> u16 {
        ((self.prefix[0] as u16) << 4) | ((self.prefix[1] >> 4) as u16)
    }
    /// `ID`: set for MPEG-2 AAC, clear for MPEG-4.
    pub const fn mpeg2_id(self) -> bool {
        self.prefix[1] & 8 != 0
    }
    /// `layer`, 2 bits; 0 in a valid header.
    pub const fn layer(self) -> u8 {
        (self.prefix[1] >> 1) & 3
    }
    /// `protection_absent`: set when no CRC follows the header.
    pub const fn protection_absent(self) -> bool {
        self.prefix[1] & 1 != 0
    }
    /// `profile_ObjectType`, 2 bits: the audio object type minus one, so 1 for
    /// AAC LC.
    pub const fn profile_code(self) -> u8 {
        self.prefix[2] >> 6
    }
    /// `sampling_frequency_index`, 4 bits.
    pub const fn frequency_index(self) -> u8 {
        (self.prefix[2] >> 2) & 15
    }
    /// `private_bit`.
    pub const fn private_bit(self) -> bool {
        self.prefix[2] & 2 != 0
    }
    /// `channel_configuration`, 3 bits; 0 when the payload gives the layout in a
    /// program config element.
    pub const fn channel_configuration(self) -> u8 {
        ((self.prefix[2] & 1) << 2) | (self.prefix[3] >> 6)
    }
    /// `original_copy`.
    pub const fn original_copy(self) -> bool {
        self.prefix[3] & 32 != 0
    }
    /// `home`.
    pub const fn home(self) -> bool {
        self.prefix[3] & 16 != 0
    }
    /// `copyright_identification_bit`.
    pub const fn copyright_id_bit(self) -> bool {
        self.prefix[3] & 8 != 0
    }
    /// `copyright_identification_start`.
    pub const fn copyright_id_start(self) -> bool {
        self.prefix[3] & 4 != 0
    }
    /// `aac_frame_length`, 13 bits: bytes in the frame, header and CRC included.
    pub const fn frame_length(self) -> u16 {
        (((self.prefix[3] & 3) as u16) << 11)
            | ((self.prefix[4] as u16) << 3)
            | ((self.prefix[5] >> 5) as u16)
    }
    /// `adts_buffer_fullness`, 11 bits; `0x7ff` signals a variable bit rate.
    pub const fn buffer_fullness(self) -> u16 {
        (((self.prefix[5] & 31) as u16) << 6) | ((self.prefix[6] >> 2) as u16)
    }
    /// `number_of_raw_data_blocks_in_frame`, 2 bits: the raw data blocks in the
    /// frame minus one.
    pub const fn raw_blocks_minus_one(self) -> u8 {
        self.prefix[6] & 3
    }
    /// Bytes before the raw data block of a frame that holds one block: 7, or 9 when
    /// a CRC follows the header. `None` for a frame of several blocks: with a CRC
    /// such a frame has further error-check syntax, and no size is guessed for it.
    pub const fn single_block_header_bytes(self) -> Option<usize> {
        if self.raw_blocks_minus_one() != 0 {
            None
        } else if self.protection_absent() {
            Some(7)
        } else {
            Some(9)
        }
    }
    /// The sampling frequency in Hz that the index names; `None` for the indices 13
    /// to 15, which name none.
    pub fn coded_frequency_hz(self) -> Option<u32> {
        indexed_frequency(self.frequency_index())
    }
    /// Channels of the channel configuration: 1 to 6, and 8 for configuration 7.
    /// None for the PCE escape. SBR/PS/channel layout is not inferred.
    pub fn coded_channel_count(self) -> Option<u8> {
        match self.channel_configuration() {
            0 => None,
            1..=6 => Some(self.channel_configuration()),
            7 => Some(8),
            _ => None,
        }
    }
    /// Why the header is not that of an AAC LC frame of one raw data block with a
    /// payload, as a bit set; 0 when it is one. Bit 0: `syncword` is not `0xfff`.
    /// Bit 1: `layer` is not 0. Bit 2: the sampling frequency index names no
    /// frequency. Bit 3: the profile is not AAC LC. Bit 4: the frame holds several
    /// raw data blocks. Bit 5: `aac_frame_length` leaves no byte after the header
    /// and CRC of a single-block frame.
    pub fn selected_wire_issues(self) -> u32 {
        u32::from(self.syncword() != 0xfff)
            | (u32::from(self.layer() != 0) << 1)
            | (u32::from(self.coded_frequency_hz().is_none()) << 2)
            | (u32::from(self.profile_code() != 1) << 3)
            | (u32::from(self.raw_blocks_minus_one() != 0) << 4)
            | (u32::from(
                self.single_block_header_bytes()
                    .is_some_and(|n| usize::from(self.frame_length()) <= n),
            ) << 5)
    }
}

/// The ADTS header in the first seven bytes of `raw`, without validation: the fields of
/// a malformed header, or of one the frame readers refuse, stay readable. Fails when
/// `raw` holds fewer than seven bytes.
pub fn inspect_adts_header(raw: &[u8]) -> Result<AacAdtsHeader> {
    Ok(AacAdtsHeader {
        prefix: raw
            .get(..7)
            .ok_or(Error::Truncated)?
            .try_into()
            .map_err(|_| Error::Truncated)?,
    })
}

/// One complete ADTS frame of a single AAC LC raw data block, with the bytes it was
/// read from. The CRC and the payload are carried, not checked.
#[derive(Clone, Debug)]
pub struct AacAdtsFrame {
    raw: Bytes,
    payload: Bytes,
    source: AacSource,
    header: AacAdtsHeader,
    crc: Option<u16>,
}
impl AacAdtsFrame {
    fn from_envelope(raw: Bytes, source: AacSource, header: AacAdtsHeader) -> Result<Self> {
        let header_bytes = header
            .single_block_header_bytes()
            .ok_or(Error::Unsupported("ADTS block layout"))?;
        let crc = if header.protection_absent() {
            None
        } else {
            let bytes = raw.get(7..9).ok_or(Error::Truncated)?;
            Some(u16::from_be_bytes([bytes[0], bytes[1]]))
        };
        let payload = raw.slice(header_bytes..raw.len())?;
        Ok(Self {
            raw,
            payload,
            source,
            header,
            crc,
        })
    }

    /// The whole frame: header, CRC when present, and payload.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// The raw data block: the bytes after the header and CRC, sharing the frame's
    /// allocation. It may be empty.
    pub fn payload_bytes(&self) -> &Bytes {
        &self.payload
    }
    /// Where the frame's header starts in its source.
    pub const fn source(&self) -> AacSource {
        self.source
    }
    /// The frame's header.
    pub const fn header(&self) -> AacAdtsHeader {
        self.header
    }
    /// The 16-bit `crc_check` that follows the header, as coded and not verified;
    /// `None` when `protection_absent` is set.
    pub const fn crc_word(&self) -> Option<u16> {
        self.crc
    }
}

enum ParsePhase {
    Header,
    Extent,
    Publish,
    Complete,
}
/// Parses a buffer that holds exactly one ADTS frame of a single AAC LC raw data
/// block. The frame shares the buffer's bytes.
pub struct AacAdtsParser {
    raw: Option<Bytes>,
    source: AacSource,
    header: Option<AacAdtsHeader>,
    crc: Option<u16>,
    phase: ParsePhase,
    failure: Option<Error>,
}
impl std::fmt::Debug for AacAdtsParser {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AacAdtsParser")
            .field("source", &self.source)
            .field("header", &self.header)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}
impl AacAdtsParser {
    /// A parser of `raw`, the frame that starts at `source`. Fails when `raw` is empty,
    /// longer than `maximum_frame_bytes` or charged to another pool than `pool`, when
    /// that limit is zero, or when the frame's end offset does not fit `u64`.
    pub fn new(
        pool: &Budget,
        source: AacSource,
        raw: Bytes,
        maximum_frame_bytes: usize,
    ) -> Result<Self> {
        crate::record_input::check_record(pool, &raw, 1, maximum_frame_bytes)?;
        source
            .offset
            .checked_add(u64::try_from(raw.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok(Self {
            raw: Some(raw),
            source,
            header: None,
            crc: None,
            phase: ParsePhase::Header,
            failure: None,
        })
    }
    /// Parses for as long as `work` lasts; the frame costs three units.
    ///
    /// [`BuildStep::Yield`] asks for another call with more work.
    /// [`BuildStep::Ready`] carries the frame, once; later calls return
    /// [`BuildStep::Complete`].
    ///
    /// Fails when the buffer ends before the header, the CRC or the declared frame
    /// length; when the syncword is not `0xfff`, the layer is not 0, the sampling
    /// frequency index names no frequency, the frame length is shorter than the header
    /// and CRC, or bytes follow the frame; when the profile is not AAC LC or the frame
    /// holds several raw data blocks; when `cancel` is set. The parser then returns the
    /// same error from every later call.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAdtsFrame>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.advance(work, cancel);
        if let Err(e) = result {
            self.failure = Some(e);
            self.raw = None;
        }
        result
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAdtsFrame>> {
        loop {
            if matches!(self.phase, ParsePhase::Complete) {
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
                .ok_or(Error::Invalid("missing ADTS source"))?;
            match self.phase {
                ParsePhase::Header => {
                    let h = inspect_adts_header(raw.as_ref())?;
                    validate_selected_header(h)?;
                    self.header = Some(h);
                    self.phase = ParsePhase::Extent;
                }
                ParsePhase::Extent => {
                    let h = self.header.ok_or(Error::Invalid("missing ADTS header"))?;
                    let size = usize::from(h.frame_length());
                    let header_size = h
                        .single_block_header_bytes()
                        .ok_or(Error::Unsupported("ADTS block layout"))?;
                    if raw.len() < header_size {
                        return Err(Error::Truncated);
                    }
                    if size < header_size {
                        return Err(Error::Invalid("ADTS declared extent smaller than header"));
                    }
                    if size > raw.len() {
                        return Err(Error::Truncated);
                    }
                    if size < raw.len() {
                        return Err(Error::Invalid("trailing bytes after one ADTS frame"));
                    }
                    if !h.protection_absent() {
                        self.crc = Some(u16::from_be_bytes([raw[7], raw[8]]));
                    }
                    self.phase = ParsePhase::Publish;
                }
                ParsePhase::Publish => {
                    let header = self.header.ok_or(Error::Invalid("missing ADTS header"))?;
                    let header_size = header
                        .single_block_header_bytes()
                        .ok_or(Error::Unsupported("ADTS block layout"))?;
                    let payload = raw.slice(header_size..raw.len())?;
                    let output = AacAdtsFrame {
                        raw: self
                            .raw
                            .take()
                            .ok_or(Error::Invalid("missing ADTS source"))?,
                        payload,
                        source: self.source,
                        header,
                        crc: self.crc,
                    };
                    self.phase = ParsePhase::Complete;
                    return Ok(BuildStep::Ready(output));
                }
                ParsePhase::Complete => return Ok(BuildStep::Complete),
            }
        }
    }
}

/// The fields that [`AacAdtsWriter`] and
/// [`header_for_payload`](Self::header_for_payload) write into an ADTS header beside
/// the frame length. The others are fixed: layer 0, no CRC, profile AAC LC and one
/// raw data block in the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAdtsParameters {
    /// `ID`: set for MPEG-2 AAC, clear for MPEG-4.
    pub mpeg2_id: bool,
    /// `sampling_frequency_index`: 0 to 12, an index that names a frequency.
    pub frequency_index: u8,
    /// `channel_configuration`, 0 to 7.
    pub channel_configuration: u8,
    /// `adts_buffer_fullness`, at most `0x7ff`, which signals a variable bit rate.
    pub buffer_fullness: u16,
    /// `private_bit`.
    pub private_bit: bool,
    /// `original_copy`.
    pub original_copy: bool,
    /// `home`.
    pub home: bool,
    /// `copyright_identification_bit`.
    pub copyright_id_bit: bool,
    /// `copyright_identification_start`.
    pub copyright_id_start: bool,
}
impl AacAdtsParameters {
    /// The parameters for the AAC LC core of the configuration `f`: an MPEG-4 header
    /// with its sampling frequency index and channel configuration, buffer fullness
    /// `0x7ff` and the other bits clear.
    ///
    /// An ADTS header has no field for the SBR and PS signalling of the
    /// configuration, which is not carried over. Nothing checks that the access
    /// units carry SBR data, or the program config element that channel
    /// configuration 0 needs, in band.
    ///
    /// Fails when the core is not AAC LC, its frequency is coded as a 24-bit value, the
    /// channel configuration exceeds 7, `frameLengthFlag` is set or a `coreCoderDelay`
    /// is present.
    pub fn from_asc(f: AacAscFacts) -> Result<Self> {
        if f.core_object_type.code != 2 || f.core_frequency.index == 15 {
            return Err(Error::Unsupported(
                "ASC cannot be represented by selected indexed ADTS core",
            ));
        }
        if f.channel_configuration > 7 {
            return Err(Error::Unsupported(
                "ASC channel configuration does not fit ADTS",
            ));
        }
        if f.ga.frame_length_flag || f.ga.core_coder_delay.is_some() {
            return Err(Error::Unsupported(
                "ASC frame/core-dependency fields not advertised by ADTS header",
            ));
        }
        Ok(Self {
            mpeg2_id: false,
            frequency_index: f.core_frequency.index,
            channel_configuration: f.channel_configuration,
            buffer_fullness: 0x7ff,
            private_bit: false,
            original_copy: false,
            home: false,
            copyright_id_bit: false,
            copyright_id_start: false,
        })
    }
    /// Build a header for an access unit the consumer already owns. The payload stays
    /// in its original storage. Fails when `frequency_index`, `channel_configuration`
    /// or `buffer_fullness` is out of range, or the frame of `payload_bytes` and the
    /// seven header bytes exceeds the 8191 bytes `aac_frame_length` can declare.
    pub fn header_for_payload(self, payload_bytes: usize) -> Result<[u8; 7]> {
        self.header(payload_bytes.checked_add(7).ok_or(Error::Overflow)?)
    }

    fn header(self, size: usize) -> Result<[u8; 7]> {
        if indexed_frequency(self.frequency_index).is_none()
            || self.channel_configuration > 7
            || self.buffer_fullness > 0x7ff
        {
            return Err(Error::Invalid(
                "ADTS output field does not fit selected wire contract",
            ));
        }
        if size > 8191 {
            return Err(Error::Limit {
                requested: size,
                maximum: 8191,
            });
        }
        Ok([
            0xff,
            0xf1 | (u8::from(self.mpeg2_id) << 3),
            0x40 | (self.frequency_index << 2)
                | (u8::from(self.private_bit) << 1)
                | (self.channel_configuration >> 2),
            ((self.channel_configuration & 3) << 6)
                | (u8::from(self.original_copy) << 5)
                | (u8::from(self.home) << 4)
                | (u8::from(self.copyright_id_bit) << 3)
                | (u8::from(self.copyright_id_start) << 2)
                | ((size >> 11) as u8),
            (size >> 3) as u8,
            ((size as u8 & 7) << 5) | ((self.buffer_fullness >> 6) as u8),
            (self.buffer_fullness as u8 & 63) << 2,
        ])
    }
}

/// One access unit wrapped in an ADTS frame by [`AacAdtsWriter`].
#[derive(Clone, Debug)]
pub struct AacAdtsOutput {
    bytes: Bytes,
    source: AacSource,
    header: AacAdtsHeader,
}
impl AacAdtsOutput {
    /// The frame: the seven header bytes, then the payload, in one allocation.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// Provenance of the original compressed access-unit payload. The emitted
    /// seven-byte header was constructed and is not a physical source header.
    pub const fn payload_source(&self) -> AacSource {
        self.source
    }
    /// The header the frame starts with.
    pub const fn constructed_header(&self) -> AacAdtsHeader {
        self.header
    }
    /// The payload within [`bytes`](Self::bytes), sharing its allocation. Never fails
    /// for a writer's output, which always holds the seven header bytes.
    pub fn payload_bytes(&self) -> Result<Bytes> {
        self.bytes.slice(7..self.bytes.len())
    }
}

/// Wraps one raw AAC access unit in an ADTS frame: a seven-byte header, then a copy
/// of the payload, in a pool allocation of its own.
#[derive(Debug)]
pub struct AacAdtsWriter {
    pool: Budget,
    input: Option<Bytes>,
    source: AacSource,
    header: [u8; 7],
    total: usize,
    copied: usize,
    output: Option<BudgetVec<u8>>,
    reserved: bool,
    complete: bool,
    failure: Option<Error>,
}
impl AacAdtsWriter {
    /// A writer for `payload`, the access unit that starts at `source`, with a
    /// header of `params`.
    ///
    /// Fails when `payload` is charged to another pool than `pool`; when the frame,
    /// seven bytes longer than the payload, exceeds `maximum_output_bytes`, the 8191
    /// bytes `aac_frame_length` can declare, or what the pool can ever hold beside the
    /// payload; when a field of `params` is out of range; or when the payload's end
    /// offset does not fit `u64`.
    pub fn new(
        pool: &Budget,
        source: AacSource,
        payload: Bytes,
        params: AacAdtsParameters,
        maximum_output_bytes: usize,
    ) -> Result<Self> {
        if !payload.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        source
            .offset
            .checked_add(u64::try_from(payload.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let total = payload.len().checked_add(7).ok_or(Error::Overflow)?;
        if total > maximum_output_bytes {
            return Err(Error::Limit {
                requested: total,
                maximum: maximum_output_bytes,
            });
        }
        let header = params.header(total)?;
        let minimum = Budget::control_storage_bytes()
            .checked_add(payload.retained_storage_bytes())
            .and_then(|n| n.checked_add(total))
            .ok_or(Error::Overflow)?;
        if minimum > pool.limit() {
            return Err(Error::Limit {
                requested: minimum,
                maximum: pool.limit(),
            });
        }
        Ok(Self {
            pool: pool.clone(),
            input: Some(payload),
            source,
            header,
            total,
            copied: 0,
            output: None,
            reserved: false,
            complete: false,
            failure: None,
        })
    }
    /// Writes for as long as `work` lasts: one unit for the allocation, one per
    /// byte copied and one for the output. `cancel` is checked at least every 4096
    /// bytes.
    ///
    /// [`BuildStep::Yield`] asks for another call with more work.
    /// [`BuildStep::Ready`] carries the frame, once; later calls return
    /// [`BuildStep::Complete`].
    ///
    /// Fails when `cancel` is set: the writer then returns the same error from every
    /// later call. [`Error::Backpressure`] and [`Error::Allocation`] keep what is
    /// written, to be stepped again.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAdtsOutput>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.advance(work, cancel);
        if let Err(e) = result
            && !retryable(e)
        {
            self.failure = Some(e);
            self.input = None;
            self.output = None;
        }
        result
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAdtsOutput>> {
        if self.complete {
            return Ok(BuildStep::Complete);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !self.reserved {
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            if self.output.is_none() {
                self.output = Some(BudgetVec::new(&self.pool));
            }
            self.output
                .as_mut()
                .ok_or(Error::Invalid("missing ADTS output"))?
                .try_reserve_exact(self.total)?;
            self.reserved = true;
        }
        while self.copied < self.total {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let available = if self.copied < 7 {
                7 - self.copied
            } else {
                (self.total - self.copied).min(4096)
            };
            let count =
                usize::try_from(work.take(u64::try_from(available).map_err(|_| Error::Overflow)?))
                    .map_err(|_| Error::Overflow)?;
            if count == 0 {
                return Ok(BuildStep::Yield);
            }
            let output = self
                .output
                .as_mut()
                .ok_or(Error::Invalid("missing ADTS output"))?;
            if self.copied < 7 {
                output.try_extend_from_slice(&self.header[self.copied..self.copied + count])?;
            } else {
                let start = self.copied - 7;
                output.try_extend_from_slice(
                    &self
                        .input
                        .as_ref()
                        .ok_or(Error::Invalid("missing ADTS payload"))?[start..start + count],
                )?;
            }
            self.copied += count;
        }
        // Every source byte has been copied. Release our source owner before
        // adding the frozen output's owner header; retries retain real output.
        self.input = None;
        if !work.try_charge(1) {
            return Ok(BuildStep::Yield);
        }
        let buffer = self
            .output
            .take()
            .ok_or(Error::Invalid("missing ADTS output"))?;
        let bytes = match buffer.freeze() {
            Ok(b) => b,
            Err(e) => {
                self.output = Some(e.value);
                return Err(e.error);
            }
        };
        self.complete = true;
        Ok(BuildStep::Ready(AacAdtsOutput {
            bytes,
            source: self.source,
            header: AacAdtsHeader {
                prefix: self.header,
            },
        }))
    }
}
