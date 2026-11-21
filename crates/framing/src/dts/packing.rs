//! DTS low14-bit word payloads from ETSI TS102114 V1.2.1 §5.3 prose.
//! Word order is explicit. Source words/high bits and final residual payload
//! bits are retained; no sync autodetection, FSIZE scaling or padding is guessed.
use crate::dts::DtsSourceIdentity;
use crate::dts::reader::limit;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceLocation, Step, WorkBudget};

/// Byte order of the 16-bit words that carry 14-bit DTS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsWordOrder {
    /// The first byte of a word is its high byte.
    BigEndian,
    /// The first byte of a word is its low byte.
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

/// The bounds a [`Dts14BitUnpacker`] enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsPackingLimits {
    /// Largest record accepted, in bytes.
    pub maximum_source_bytes: usize,
    /// Largest canonical output accepted, in bytes: 14 bits per source word,
    /// rounded down to whole bytes.
    pub maximum_output_bytes: usize,
}

/// The place in the packed source of one payload bit. `offset` is a byte offset
/// in the source as it was stored, not in the unpacked canonical bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsPackedBitLocation {
    /// The source the record was read from.
    pub source: triergon_core::SourceId,
    /// The generation of the input the record was read from.
    pub generation: u64,
    /// Offset in the source of the byte that holds the bit.
    pub offset: u64,
    /// Position of the bit in that byte: 0 for the most significant bit to 7 for
    /// the least.
    pub msb0_bit: u8,
}

/// The payload of one record of 14-bit words: the low 14 bits of every word,
/// joined most significant bit first. The record is not necessarily a complete
/// DTS frame. The canonical bytes are whole bytes only; the 0 to 6 payload bits
/// left over are kept apart, right-aligned, so no payload bit is discarded and
/// no partial byte is filled with invented bits.
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
    /// The packed record exactly as it was given.
    pub fn source_bytes(&self) -> &Bytes {
        &self.source
    }
    /// The whole bytes of the payload in bitstream order: `payload_bits() / 8`
    /// of them.
    pub fn canonical_bytes(&self) -> &Bytes {
        &self.canonical
    }
    /// Where the record came from, as the caller stated it.
    pub const fn identity(&self) -> DtsSourceIdentity {
        self.identity
    }
    /// The byte order the record's words were read in.
    pub const fn word_order(&self) -> DtsWordOrder {
        self.order
    }
    /// Number of payload bits: 14 for every source word.
    pub const fn payload_bits(&self) -> usize {
        self.payload_bits
    }
    /// Number of payload bits after the last whole byte: `payload_bits() % 8`,
    /// which is 0, 2, 4 or 6.
    pub const fn residual_bits(&self) -> u8 {
        self.residual_bits
    }
    /// The last `residual_bits()` payload bits, right-aligned; 0 when there are
    /// none.
    pub const fn residual_value(&self) -> u8 {
        self.residual_value
    }
    /// The two bits above the payload in source word `word`, counted from zero:
    /// 0 to 3.
    ///
    /// # Errors
    ///
    /// `Invalid` when the record has no word `word`.
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
    /// Where payload bit `bit`, counted from zero in bitstream order, lies in
    /// the packed source.
    ///
    /// # Errors
    ///
    /// `Invalid` when `bit` is not below `payload_bits()`.
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

/// Unpacks one complete record of 14-bit DTS words that the caller owns
/// (ETSI TS 102 114 V1.2.1 §5.3) into canonical bytes, step by step. Every
/// 16-bit word gives its low 14 bits, most significant first, in either byte
/// order. The unpacker does not look for a sync word or a frame boundary. A
/// record with an odd number of bytes is refused as truncated.
///
/// The output is allocated once, at its final size, before the first bit is
/// written. Every payload bit and every output byte costs one unit of work;
/// allocating, completing and returning the output cost a few more. After
/// `Backpressure` or `Allocation` the unpacker keeps its record and progress
/// and may be stepped again; any other error is returned again by every later
/// step.
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

impl std::fmt::Debug for Dts14BitUnpacker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dts14BitUnpacker")
            .field("identity", &self.identity)
            .field("order", &self.order)
            .field("payload_bits", &self.payload_bits)
            .field("position", &self.position)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl Dts14BitUnpacker {
    /// An unpacker of `source`, a record of whole 16-bit words in byte order
    /// `order` that `identity` locates. `source` must be charged to `pool`, and
    /// the output is charged to it.
    ///
    /// # Errors
    ///
    /// `ForeignPool` when `source` is charged to another pool. `Truncated` for
    /// an odd number of bytes. `Limit` when the record or its output exceeds
    /// `limits`, and when the pool's limit cannot hold the record and the output
    /// together. `Overflow` when the record ends beyond offset `u64::MAX`.
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
        limit(source.len(), limits.maximum_source_bytes)?;
        if !source.len().is_multiple_of(2) {
            return Err(Error::Truncated);
        }
        let payload_bits = (source.len() / 2).checked_mul(14).ok_or(Error::Overflow)?;
        let output_bytes = payload_bits / 8;
        limit(output_bytes, limits.maximum_output_bytes)?;
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
        limit(minimum, pool.limit())?;
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

    /// Advances the unpacking by at most the work left in `work`. `Ready`
    /// carries the payload, once; every later step returns `End` with the
    /// location of the record's end in its source. `Yield` means the work is
    /// spent. An owned record never requests input.
    ///
    /// # Errors
    ///
    /// `Cancelled` when `cancel` is set. `Backpressure` or `Allocation` when the
    /// output cannot be allocated: after these two, and only these, `step` may
    /// be called again.
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
                            self.output = Some(error.value);
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
