//! DTS low14-bit word payloads from ETSI TS102114 V1.2.1 §5.3 prose.
//! Word order is explicit. Source words/high bits and final residual payload
//! bits are retained; no sync autodetection, FSIZE scaling or padding is guessed.
use crate::DtsSourceIdentity;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceLocation, Step, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsWordOrder {
    BigEndian,
    LittleEndian,
}

impl DtsWordOrder {
    fn word(self, pair: [u8; 2]) -> u16 {
        match self {
            Self::BigEndian => u16::from_be_bytes(pair),
            Self::LittleEndian => u16::from_le_bytes(pair),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsPackingLimits {
    pub maximum_source_bytes: usize,
    pub maximum_output_bytes: usize,
}

/// Exact physical source bit for a logical payload bit; offset is an original
/// byte offset and msb0_bit is0..7. This is not a canonical-byte source offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsPackedBitLocation {
    pub source: triergon_core::SourceId,
    pub generation: u64,
    pub offset: u64,
    pub msb0_bit: u8,
}

/// Complete word-record payload, not a complete DTS frame. Canonical storage
/// contains full bytes only. The0..6 residual bits are right-aligned separately;
/// no payload bit is discarded and no partial byte is filled with invented bits.
#[derive(Clone, Debug)]
pub struct Dts14BitPayload {
    source: Bytes,
    canonical: Bytes,
    identity: DtsSourceIdentity,
    order: DtsWordOrder,
    payload_bits: usize,
    residual_bits: u8,
    residual_value: u8,
}

impl Dts14BitPayload {
    pub fn source_bytes(&self) -> &Bytes {
        &self.source
    }
    pub fn canonical_bytes(&self) -> &Bytes {
        &self.canonical
    }
    pub const fn identity(&self) -> DtsSourceIdentity {
        self.identity
    }
    pub const fn word_order(&self) -> DtsWordOrder {
        self.order
    }
    pub const fn payload_bits(&self) -> usize {
        self.payload_bits
    }
    pub const fn residual_bits(&self) -> u8 {
        self.residual_bits
    }
    pub const fn residual_value(&self) -> u8 {
        self.residual_value
    }
    pub fn unused_high_bits(&self, word: usize) -> Result<u8> {
        let at = word.checked_mul(2).ok_or(Error::Overflow)?;
        let end = at.checked_add(2).ok_or(Error::Overflow)?;
        let pair = self
            .source
            .as_ref()
            .get(at..end)
            .ok_or(Error::Invalid("DTS packed word index"))?;
        u8::try_from(self.order.word([pair[0], pair[1]]) >> 14).map_err(|_| Error::Overflow)
    }
    pub fn payload_bit_source(&self, bit: usize) -> Result<DtsPackedBitLocation> {
        if bit >= self.payload_bits {
            return Err(Error::Invalid("DTS packed payload bit index"));
        }
        let word_bit = 13 - bit % 14;
        let byte = match self.order {
            DtsWordOrder::BigEndian => 1 - word_bit / 8,
            DtsWordOrder::LittleEndian => word_bit / 8,
        };
        let index = (bit / 14)
            .checked_mul(2)
            .and_then(|at| at.checked_add(byte))
            .ok_or(Error::Overflow)?;
        let offset = self
            .identity
            .offset
            .checked_add(u64::try_from(index).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok(DtsPackedBitLocation {
            source: self.identity.source,
            generation: self.identity.generation,
            offset,
            msb0_bit: u8::try_from(7 - word_bit % 8).map_err(|_| Error::Overflow)?,
        })
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

/// An owned complete word record, processed incrementally by work. This is a
/// low14-bit transport adapter, not sync/frame discovery. Both word orders use
/// the same primary bit rule. Odd final physical bytes are explicitly truncated.
/// The output reserves its full capacity while empty; payload bits are charged
/// individually, with separately charged publication/freeze actions. No step
/// copies a dynamic prefix and no allocation failure loses its source or state.
pub struct Dts14BitUnpacker {
    pool: Budget,
    source: Option<Bytes>,
    output: Option<BudgetVec<u8>>,
    frozen: Option<Bytes>,
    identity: DtsSourceIdentity,
    order: DtsWordOrder,
    record_end: SourceLocation,
    payload_bits: usize,
    output_bytes: usize,
    position: usize,
    partial_bits: u8,
    partial: u8,
    phase: Phase,
    failure: Option<Error>,
}

impl Dts14BitUnpacker {
    pub fn new(
        pool: &Budget,
        identity: DtsSourceIdentity,
        source: Bytes,
        order: DtsWordOrder,
        limits: DtsPackingLimits,
    ) -> Result<Self> {
        if !source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        check_limit(source.len(), limits.maximum_source_bytes)?;
        if !source.len().is_multiple_of(2) {
            return Err(Error::Truncated);
        }
        let payload_bits = (source.len() / 2).checked_mul(14).ok_or(Error::Overflow)?;
        let output_bytes = payload_bits / 8;
        check_limit(output_bytes, limits.maximum_output_bytes)?;
        let end = identity
            .offset
            .checked_add(u64::try_from(source.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        // The input owner cannot be released while the adapter owns it. Refuse
        // impossible same-pool coexistence rather than reporting endless pressure.
        let minimum = source
            .retained_storage_bytes()
            .checked_add(output_bytes)
            .and_then(|bytes| bytes.checked_add(Bytes::owner_overhead_bytes()))
            .and_then(|bytes| bytes.checked_add(Budget::control_storage_bytes()))
            .ok_or(Error::Overflow)?;
        check_limit(minimum, pool.limit())?;
        Ok(Self {
            pool: pool.clone(),
            source: Some(source),
            output: None,
            frozen: None,
            identity,
            order,
            record_end: SourceLocation {
                source: identity.source,
                offset: end,
            },
            payload_bits,
            output_bytes,
            position: 0,
            partial_bits: 0,
            partial: 0,
            phase: Phase::Reserve,
            failure: None,
        })
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Dts14BitPayload>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.failure = Some(error);
            self.source = None;
            self.output = None;
            self.frozen = None;
        }
        result
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Dts14BitPayload>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.phase, Phase::Complete) {
                return Ok(Step::End(self.record_end));
            }
            if !work.try_charge(1) {
                return Ok(Step::Yield);
            }
            match self.phase {
                Phase::Reserve => {
                    let buffer = self
                        .output
                        .get_or_insert_with(|| BudgetVec::new(&self.pool));
                    buffer.try_reserve_exact(self.output_bytes)?;
                    self.phase = Phase::Bits;
                }
                Phase::Bits => {
                    if self.position == self.payload_bits {
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    let source = self
                        .source
                        .as_ref()
                        .ok_or(Error::Invalid("missing DTS packed source"))?;
                    let at = (self.position / 14).checked_mul(2).ok_or(Error::Overflow)?;
                    let word = self
                        .order
                        .word([source.as_ref()[at], source.as_ref()[at + 1]]);
                    let bit = u8::try_from((word >> (13 - self.position % 14)) & 1)
                        .map_err(|_| Error::Overflow)?;
                    self.partial = (self.partial << 1) | bit;
                    self.partial_bits += 1;
                    self.position += 1;
                    if self.partial_bits == 8 {
                        self.phase = Phase::Byte;
                    }
                }
                Phase::Byte => {
                    let buffer = self
                        .output
                        .as_mut()
                        .ok_or(Error::Invalid("missing DTS packed output"))?;
                    // Enforce the pre-reserved construction invariant before the
                    // generic push can try growing/moving a materialized prefix.
                    if buffer.len() == buffer.capacity() {
                        return Err(Error::Invalid("DTS packed output capacity"));
                    }
                    buffer.try_push(self.partial).map_err(|error| error.error)?;
                    self.partial = 0;
                    self.partial_bits = 0;
                    self.phase = Phase::Bits;
                }
                Phase::Freeze => {
                    let buffer = self
                        .output
                        .take()
                        .ok_or(Error::Invalid("missing DTS packed freeze"))?;
                    if buffer.len() != self.output_bytes {
                        return Err(Error::Invalid("DTS packed output length"));
                    }
                    match buffer.freeze() {
                        Ok(bytes) => {
                            self.frozen = Some(bytes);
                            self.phase = Phase::Publish;
                        }
                        Err(error) => {
                            self.output = Some(error.buffer);
                            return Err(error.error);
                        }
                    }
                }
                Phase::Publish => {
                    let output = Dts14BitPayload {
                        source: self
                            .source
                            .take()
                            .ok_or(Error::Invalid("missing DTS packed publication source"))?,
                        canonical: self
                            .frozen
                            .take()
                            .ok_or(Error::Invalid("missing DTS packed publication bytes"))?,
                        identity: self.identity,
                        order: self.order,
                        payload_bits: self.payload_bits,
                        residual_bits: self.partial_bits,
                        residual_value: self.partial,
                    };
                    self.phase = Phase::Complete;
                    return Ok(Step::Ready(output));
                }
                Phase::Complete => return Ok(Step::End(self.record_end)),
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
