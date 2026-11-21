//! The payload of 14-bit DTS words, the low 14 bits of each 16-bit word (ETSI
//! TS 102 114 V1.2.1 §5.3), in a byte order the caller names. No length is derived
//! from `FSIZE`.
use crate::dts::DtsSourceIdentity;
use crate::dts::reader::limit;
use crate::retryable;
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
    /// The two bits above the payload in source word `word`, counted from zero: 0 to 3.
    /// Fails with `Invalid` when the record has no word `word`.
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
    /// Where payload bit `bit`, counted from zero in bitstream order, lies in the
    /// packed source. Fails with `Invalid` when `bit` is not below `payload_bits()`.
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
}

/// Where an unpacking stands. The record and its output are held only until the
/// payload is returned.
enum State {
    Unpacking {
        source: Bytes,
        output: BudgetVec<u8>,
        phase: Phase,
    },
    /// The payload is complete and returned by the next unit of work.
    Frozen {
        source: Bytes,
        canonical: Bytes,
    },
    Complete,
    Failed(Error),
}

/// Unpacks one complete record of 14-bit DTS words that the caller owns
/// (ETSI TS 102 114 V1.2.1 §5.3) into canonical bytes, step by step, in either
/// byte order. The unpacker does not look for a sync word or a frame boundary.
///
/// The output is allocated once, at its final size, before the first bit is
/// written. Every payload bit and every output byte costs one unit of work;
/// allocating, completing and returning the output cost a few more. After
/// `Backpressure` or `Allocation` the unpacker keeps its record and progress
/// and may be stepped again; any other error is returned again by every later
/// step.
pub struct Dts14BitUnpacker {
    identity: DtsSourceIdentity,
    order: DtsWordOrder,
    record_end: SourceLocation,
    payload_bits: usize,
    output_bytes: usize,
    position: usize,
    partial_bits: u8,
    partial: u8,
    state: State,
}

impl std::fmt::Debug for Dts14BitUnpacker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failure = match self.state {
            State::Failed(error) => Some(error),
            _ => None,
        };
        f.debug_struct("Dts14BitUnpacker")
            .field("identity", &self.identity)
            .field("order", &self.order)
            .field("payload_bits", &self.payload_bits)
            .field("position", &self.position)
            .field("failure", &failure)
            .finish_non_exhaustive()
    }
}

impl Dts14BitUnpacker {
    /// An unpacker of `source`, a record of whole 16-bit words in byte order
    /// `order` that `identity` locates. `source` must be charged to `pool`, and
    /// the output is charged to it.
    ///
    /// Fails with `ForeignPool` when `source` is charged to another pool; `Truncated`
    /// for an odd number of bytes; `Limit` when the record or its output exceeds
    /// `limits`, and when the pool's limit cannot hold the record and the output
    /// together; `Overflow` when the record ends beyond offset `u64::MAX`.
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
            state: State::Unpacking {
                source,
                output: BudgetVec::new(pool),
                phase: Phase::Reserve,
            },
        })
    }

    /// Advances the unpacking by at most the work left in `work`. `Ready`
    /// carries the payload, once; every later step returns `End` with the
    /// location of the record's end in its source. An owned record never
    /// requests input.
    ///
    /// Fails with `Cancelled` when `cancel` is set; `Backpressure` or `Allocation` when
    /// the output cannot be allocated: after these two, and only these, `step` may be
    /// called again.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Dts14BitPayload>> {
        if let State::Failed(error) = self.state {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !retryable(error)
        {
            self.state = State::Failed(error);
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
            let (source, output, phase) = match &mut self.state {
                State::Unpacking {
                    source,
                    output,
                    phase,
                } => (source, output, phase),
                State::Frozen { .. } => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let State::Frozen { source, canonical } =
                        std::mem::replace(&mut self.state, State::Complete)
                    else {
                        unreachable!("the payload is frozen")
                    };
                    return Ok(Step::Ready(Dts14BitPayload {
                        source,
                        canonical,
                        identity: self.identity,
                        order: self.order,
                        payload_bits: self.payload_bits,
                        residual_bits: self.partial_bits,
                        residual_value: self.partial,
                    }));
                }
                State::Complete => return Ok(Step::End(self.record_end)),
                &mut State::Failed(error) => return Err(error),
            };
            if !work.try_charge(1) {
                return Ok(Step::Yield);
            }
            match phase {
                Phase::Reserve => {
                    output.try_reserve_exact(self.output_bytes)?;
                    *phase = Phase::Bits;
                }
                Phase::Bits => {
                    if self.position == self.payload_bits {
                        *phase = Phase::Freeze;
                        continue;
                    }
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
                        *phase = Phase::Byte;
                    }
                }
                Phase::Byte => {
                    // Enforce the pre-reserved construction invariant before the
                    // generic push can try growing/moving a materialized prefix.
                    if output.len() == output.capacity() {
                        return Err(Error::Invalid("DTS packed output capacity"));
                    }
                    output.try_push(self.partial).map_err(|error| error.error)?;
                    self.partial = 0;
                    self.partial_bits = 0;
                    *phase = Phase::Bits;
                }
                Phase::Freeze => {
                    if output.len() != self.output_bytes {
                        return Err(Error::Invalid("DTS packed output length"));
                    }
                    let State::Unpacking { source, output, .. } =
                        std::mem::replace(&mut self.state, State::Complete)
                    else {
                        unreachable!("the payload is unpacking")
                    };
                    match output.freeze() {
                        Ok(canonical) => self.state = State::Frozen { source, canonical },
                        Err(refused) => {
                            self.state = State::Unpacking {
                                source,
                                output: refused.value,
                                phase: Phase::Freeze,
                            };
                            return Err(refused.error);
                        }
                    }
                }
            }
        }
    }
}
