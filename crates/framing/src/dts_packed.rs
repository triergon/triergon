//! Original word-payload storage from ETSI TS102114 §3.4/5.3.
//! Every source bit survives; byte-alignment zeros are labelled non-source bits.
use crate::{DtsPackedBitLocation, DtsSourceIdentity};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceLocation, Step, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsPacking {
    CanonicalBe,
    Words16Le,
    Words14Be,
    Words14Le,
}
impl DtsPacking {
    pub const fn source_unit_bytes(self) -> usize {
        if matches!(self, Self::CanonicalBe) {
            1
        } else {
            2
        }
    }
    pub const fn word_payload_bits(self) -> usize {
        match self {
            Self::CanonicalBe => 8,
            Self::Words16Le => 16,
            Self::Words14Be | Self::Words14Le => 14,
        }
    }
    pub fn payload_bits(self, source_bytes: usize) -> Result<usize> {
        if source_bytes % self.source_unit_bytes() != 0 {
            return Err(Error::Truncated);
        }
        (source_bytes / self.source_unit_bytes())
            .checked_mul(self.word_payload_bits())
            .ok_or(Error::Overflow)
    }
    pub(crate) fn bit(self, source: &[u8], position: usize) -> Result<u8> {
        if matches!(self, Self::CanonicalBe) {
            let byte = *source.get(position / 8).ok_or(Error::Truncated)?;
            return Ok((byte >> (7 - position % 8)) & 1);
        }
        let width = self.word_payload_bits();
        let at = (position / width).checked_mul(2).ok_or(Error::Overflow)?;
        let pair = source
            .get(at..at.checked_add(2).ok_or(Error::Overflow)?)
            .ok_or(Error::Truncated)?;
        let value = match self {
            Self::Words14Be => u16::from_be_bytes([pair[0], pair[1]]),
            _ => u16::from_le_bytes([pair[0], pair[1]]),
        };
        Ok(((value >> (width - 1 - position % width)) & 1) as u8)
    }
    /// Allocation-free fixed probe. High unused word bits remain in source.
    pub fn detect(prefix: &[u8]) -> Result<Self> {
        if prefix.len() < 4 {
            return Err(Error::Truncated);
        }
        for packing in [
            Self::CanonicalBe,
            Self::Words16Le,
            Self::Words14Be,
            Self::Words14Le,
        ] {
            let need = if matches!(packing, Self::Words14Be | Self::Words14Le) {
                6
            } else {
                4
            };
            let available = prefix.len().min(need);
            let complete = available / packing.source_unit_bytes() * packing.source_unit_bytes();
            let bits = packing.payload_bits(complete)?.min(32);
            let mut value = 0_u32;
            for bit in 0..bits {
                value = (value << 1) | u32::from(packing.bit(prefix, bit)?);
            }
            if bits == 32 && value == 0x7ffe8001 {
                return Ok(packing);
            }
            if bits < 32 && value == (0x7ffe8001_u32 >> (32 - bits)) {
                return Err(Error::Truncated);
            }
        }
        Err(Error::Invalid("DTS source sync packing"))
    }
    pub(crate) fn normal_sync(self, prefix: &[u8]) -> Result<bool> {
        if self.payload_bits(prefix.len())? < 38 {
            return Ok(false);
        }
        let mut value = 0_u64;
        for bit in 0..38 {
            value = (value << 1) | u64::from(self.bit(prefix, bit)?);
        }
        Ok(value == ((0x7ffe8001_u64 << 6) | 63))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsPackedLimits {
    pub maximum_source_bytes: usize,
    pub maximum_storage_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct DtsPackedPayload {
    raw: Bytes,
    storage: Bytes,
    identity: DtsSourceIdentity,
    packing: DtsPacking,
    valid_bits: usize,
}
impl DtsPackedPayload {
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    pub fn storage_bytes(&self) -> &Bytes {
        &self.storage
    }
    pub const fn identity(&self) -> DtsSourceIdentity {
        self.identity
    }
    pub const fn packing(&self) -> DtsPacking {
        self.packing
    }
    pub const fn valid_bits(&self) -> usize {
        self.valid_bits
    }
    pub fn alignment_zero_bits(&self) -> u8 {
        ((8 - self.valid_bits % 8) % 8) as u8
    }
    pub fn source_bit(&self, position: usize) -> Result<DtsPackedBitLocation> {
        if position >= self.valid_bits {
            return Err(Error::Invalid("DTS non-source alignment bit"));
        }
        let (index, msb0) = if matches!(self.packing, DtsPacking::CanonicalBe) {
            (position / 8, (position % 8) as u8)
        } else {
            let width = self.packing.word_payload_bits();
            let word_bit = width - 1 - position % width;
            let byte = if matches!(self.packing, DtsPacking::Words14Be) {
                1 - word_bit / 8
            } else {
                word_bit / 8
            };
            (
                (position / width)
                    .checked_mul(2)
                    .and_then(|at| at.checked_add(byte))
                    .ok_or(Error::Overflow)?,
                (7 - word_bit % 8) as u8,
            )
        };
        Ok(DtsPackedBitLocation {
            source: self.identity.source,
            generation: self.identity.generation,
            offset: self
                .identity
                .offset
                .checked_add(u64::try_from(index).map_err(|_| Error::Overflow)?)
                .ok_or(Error::Overflow)?,
            msb0_bit: msb0,
        })
    }
    pub fn bit(&self, position: usize) -> Result<u8> {
        if position >= self.valid_bits {
            return Err(Error::Invalid("DTS non-source alignment bit"));
        }
        Ok((self.storage.as_ref()[position / 8] >> (7 - position % 8)) & 1)
    }
    pub fn unused_word_high_bits(&self, index: usize) -> Result<Option<u8>> {
        if matches!(self.packing, DtsPacking::CanonicalBe) {
            return Err(Error::Invalid("canonical byte stream has no storage words"));
        }
        let at = index.checked_mul(2).ok_or(Error::Overflow)?;
        let pair = self
            .raw
            .as_ref()
            .get(at..at.checked_add(2).ok_or(Error::Overflow)?)
            .ok_or(Error::Invalid("DTS source word index"))?;
        if matches!(self.packing, DtsPacking::Words16Le) {
            return Ok(None);
        }
        let word = if matches!(self.packing, DtsPacking::Words14Be) {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        };
        Ok(Some((word >> 14) as u8))
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Reserve,
    Bits,
    Byte,
    Freeze,
    Publish,
    Complete,
}
pub struct DtsPackedPayloadJob {
    pool: Budget,
    raw: Option<Bytes>,
    builder: Option<BudgetVec<u8>>,
    frozen: Option<Bytes>,
    identity: DtsSourceIdentity,
    packing: DtsPacking,
    valid_bits: usize,
    storage_bytes: usize,
    position: usize,
    partial: u8,
    partial_bits: u8,
    phase: Phase,
    failure: Option<Error>,
    end: SourceLocation,
}
impl DtsPackedPayloadJob {
    pub fn new(
        pool: &Budget,
        identity: DtsSourceIdentity,
        raw: Bytes,
        packing: DtsPacking,
        limits: DtsPackedLimits,
    ) -> Result<Self> {
        if !raw.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        limit(raw.len(), limits.maximum_source_bytes)?;
        let valid_bits = packing.payload_bits(raw.len())?;
        let storage_bytes = valid_bits.checked_add(7).ok_or(Error::Overflow)? / 8;
        limit(storage_bytes, limits.maximum_storage_bytes)?;
        let end = SourceLocation {
            source: identity.source,
            offset: identity
                .offset
                .checked_add(u64::try_from(raw.len()).map_err(|_| Error::Overflow)?)
                .ok_or(Error::Overflow)?,
        };
        let phase = if matches!(packing, DtsPacking::CanonicalBe) {
            Phase::Publish
        } else {
            Phase::Reserve
        };
        if !matches!(packing, DtsPacking::CanonicalBe) {
            let minimum = raw
                .retained_storage_bytes()
                .checked_add(storage_bytes)
                .and_then(|v| v.checked_add(Bytes::owner_overhead_bytes()))
                .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
                .ok_or(Error::Overflow)?;
            limit(minimum, pool.limit())?;
        }
        Ok(Self {
            pool: pool.clone(),
            raw: Some(raw),
            builder: None,
            frozen: None,
            identity,
            packing,
            valid_bits,
            storage_bytes,
            position: 0,
            partial: 0,
            partial_bits: 0,
            phase,
            failure: None,
            end,
        })
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsPackedPayload>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result {
            if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                self.failure = Some(error);
                self.raw = None;
                self.builder = None;
                self.frozen = None;
            }
        }
        result
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsPackedPayload>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.phase, Phase::Complete) {
                return Ok(Step::End(self.end));
            }
            if !work.try_charge(1) {
                return Ok(Step::Yield);
            }
            match self.phase {
                Phase::Reserve => {
                    self.builder
                        .get_or_insert_with(|| BudgetVec::new(&self.pool))
                        .try_reserve_exact(self.storage_bytes)?;
                    self.phase = Phase::Bits;
                }
                Phase::Bits => {
                    if self.position == self.valid_bits {
                        if self.partial_bits == 0 {
                            self.phase = Phase::Freeze;
                        } else {
                            // Representation alignment only. valid_bits does not include these zeros.
                            self.partial <<= 8 - self.partial_bits;
                            self.phase = Phase::Byte;
                        }
                        continue;
                    }
                    let raw = self
                        .raw
                        .as_ref()
                        .ok_or(Error::Invalid("missing packed source"))?;
                    self.partial =
                        (self.partial << 1) | self.packing.bit(raw.as_ref(), self.position)?;
                    self.partial_bits += 1;
                    self.position += 1;
                    if self.partial_bits == 8 {
                        self.phase = Phase::Byte;
                    }
                }
                Phase::Byte => {
                    let builder = self
                        .builder
                        .as_mut()
                        .ok_or(Error::Invalid("missing packed builder"))?;
                    if builder.len() == builder.capacity() {
                        return Err(Error::Invalid("packed capacity invariant"));
                    }
                    builder
                        .try_push(self.partial)
                        .map_err(|failure| failure.error)?;
                    self.partial = 0;
                    self.partial_bits = 0;
                    self.phase = Phase::Bits;
                }
                Phase::Freeze => {
                    let builder = self
                        .builder
                        .take()
                        .ok_or(Error::Invalid("missing packed freeze"))?;
                    match builder.freeze() {
                        Ok(bytes) => {
                            self.frozen = Some(bytes);
                            self.phase = Phase::Publish;
                        }
                        Err(failure) => {
                            self.builder = Some(failure.buffer);
                            return Err(failure.error);
                        }
                    }
                }
                Phase::Publish => {
                    let raw = self
                        .raw
                        .take()
                        .ok_or(Error::Invalid("missing packed publication"))?;
                    let storage = if matches!(self.packing, DtsPacking::CanonicalBe) {
                        raw.clone()
                    } else {
                        self.frozen
                            .take()
                            .ok_or(Error::Invalid("missing packed storage"))?
                    };
                    self.phase = Phase::Complete;
                    return Ok(Step::Ready(DtsPackedPayload {
                        raw,
                        storage,
                        identity: self.identity,
                        packing: self.packing,
                        valid_bits: self.valid_bits,
                    }));
                }
                Phase::Complete => return Ok(Step::End(self.end)),
            }
        }
    }
}
pub(crate) fn limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}
