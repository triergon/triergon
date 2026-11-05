//! Selected complete single-block ADTS source/framing operations. Audio payload
//! and CRC are retained, not decoded or certified. No clock is inferred.
use crate::{AacAscFacts, AacSource, BuildStep, indexed_aac_frequency};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAdtsHeader {
    prefix: [u8; 7],
}

impl AacAdtsHeader {
    pub const fn raw_prefix(self) -> [u8; 7] {
        self.prefix
    }
    pub const fn syncword(self) -> u16 {
        ((self.prefix[0] as u16) << 4) | ((self.prefix[1] >> 4) as u16)
    }
    pub const fn mpeg2_id(self) -> bool {
        self.prefix[1] & 8 != 0
    }
    pub const fn layer(self) -> u8 {
        (self.prefix[1] >> 1) & 3
    }
    pub const fn protection_absent(self) -> bool {
        self.prefix[1] & 1 != 0
    }
    pub const fn profile_code(self) -> u8 {
        self.prefix[2] >> 6
    }
    pub const fn frequency_index(self) -> u8 {
        (self.prefix[2] >> 2) & 15
    }
    pub const fn private_bit(self) -> bool {
        self.prefix[2] & 2 != 0
    }
    pub const fn channel_configuration(self) -> u8 {
        ((self.prefix[2] & 1) << 2) | (self.prefix[3] >> 6)
    }
    pub const fn original_copy(self) -> bool {
        self.prefix[3] & 32 != 0
    }
    pub const fn home(self) -> bool {
        self.prefix[3] & 16 != 0
    }
    pub const fn copyright_id_bit(self) -> bool {
        self.prefix[3] & 8 != 0
    }
    pub const fn copyright_id_start(self) -> bool {
        self.prefix[3] & 4 != 0
    }
    pub const fn frame_length(self) -> u16 {
        (((self.prefix[3] & 3) as u16) << 11)
            | ((self.prefix[4] as u16) << 3)
            | ((self.prefix[5] >> 5) as u16)
    }
    pub const fn buffer_fullness(self) -> u16 {
        (((self.prefix[5] & 31) as u16) << 6) | ((self.prefix[6] >> 2) as u16)
    }
    pub const fn raw_blocks_minus_one(self) -> u8 {
        self.prefix[6] & 3
    }
    /// The selected single-block layout only. Protected multi-block frames
    /// have additional error-check syntax and never acquire a guessed extent.
    pub const fn single_block_header_bytes(self) -> Option<usize> {
        if self.raw_blocks_minus_one() != 0 {
            None
        } else if self.protection_absent() {
            Some(7)
        } else {
            Some(9)
        }
    }
    pub fn coded_frequency_hz(self) -> Option<u32> {
        indexed_aac_frequency(self.frequency_index())
    }
    /// None for the PCE escape. SBR/PS/channel layout is not inferred.
    pub fn coded_channel_count(self) -> Option<u8> {
        match self.channel_configuration() {
            0 => None,
            1..=6 => Some(self.channel_configuration()),
            7 => Some(8),
            _ => None,
        }
    }
    /// Bits0/1/2: sync/layer/frequency;3/4: unselected profile/block layout;
    /// bit5: no compressed payload in the declared selected extent.
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

/// Raw fixed fields remain inspectable for unselected/malformed modes.
pub fn inspect_aac_adts_header(raw: &[u8]) -> Result<AacAdtsHeader> {
    Ok(AacAdtsHeader {
        prefix: raw
            .get(..7)
            .ok_or(Error::Truncated)?
            .try_into()
            .map_err(|_| Error::Truncated)?,
    })
}

#[derive(Clone, Debug)]
pub struct AacAdtsFrame {
    raw: Bytes,
    payload: Bytes,
    source: AacSource,
    header: AacAdtsHeader,
    crc: Option<u16>,
}
impl AacAdtsFrame {
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub fn payload_bytes(&self) -> &Bytes {
        &self.payload
    }
    pub const fn source(&self) -> AacSource {
        self.source
    }
    pub const fn header(&self) -> AacAdtsHeader {
        self.header
    }
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
pub struct AacAdtsParser {
    raw: Option<Bytes>,
    source: AacSource,
    header: Option<AacAdtsHeader>,
    crc: Option<u16>,
    phase: ParsePhase,
    failure: Option<Error>,
}
impl AacAdtsParser {
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
                    let h = inspect_aac_adts_header(raw.as_ref())?;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAdtsParameters {
    pub mpeg2_id: bool,
    pub frequency_index: u8,
    pub channel_configuration: u8,
    pub buffer_fullness: u16,
    pub private_bit: bool,
    pub original_copy: bool,
    pub home: bool,
    pub copyright_id_bit: bool,
    pub copyright_id_start: bool,
}
impl AacAdtsParameters {
    /// An explicit AAC-LC core projection; this does not transmit ASC extension
    /// signalling or verify that an opaque access unit contains in-band SBR/PCE.
    pub fn from_asc(f: AacAscFacts) -> Result<Self> {
        if f.core_object_type.code != 2 || f.core_frequency.index == 15 {
            return Err(Error::Unsupported(
                "ASC cannot be represented by selected indexed ADTS core",
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
    /// Build a header for an access unit the consumer already owns.
    /// The payload stays in its original storage.
    pub fn header_for_payload(self, payload_bytes: usize) -> Result<[u8; 7]> {
        self.header(payload_bytes.checked_add(7).ok_or(Error::Overflow)?)
    }

    fn header(self, size: usize) -> Result<[u8; 7]> {
        if indexed_aac_frequency(self.frequency_index).is_none()
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

#[derive(Clone, Debug)]
pub struct AacAdtsOutput {
    bytes: Bytes,
    source: AacSource,
    header: AacAdtsHeader,
}
impl AacAdtsOutput {
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// Provenance of the original compressed access-unit payload. The emitted
    /// seven-byte header was constructed and is not a physical source header.
    pub const fn payload_source(&self) -> AacSource {
        self.source
    }
    pub const fn constructed_header(&self) -> AacAdtsHeader {
        self.header
    }
    pub fn payload_bytes(&self) -> Result<Bytes> {
        self.bytes.slice(7..self.bytes.len())
    }
}

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
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AacAdtsOutput>> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let result = self.advance(work, cancel);
        if let Err(e) = result {
            if !matches!(e, Error::Backpressure { .. } | Error::Allocation) {
                self.failure = Some(e);
                self.input = None;
                self.output = None;
            }
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
                self.output = Some(e.buffer);
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
