//! The word packings of a DTS stream (ETSI TS 102 114 §3.4, §5.3) and the payload
//! of a record read in one of them. Every source bit is kept; the zero bits that pad
//! the payload to whole bytes are marked as not coming from the source.
use crate::dts::core_frame::CORE_SYNC;
use crate::dts::reader::limit;
use crate::dts::{DtsPackedBitLocation, DtsSourceIdentity};
use crate::retryable;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceLocation, Step, WorkBudget};

/// How the bits of a DTS stream are stored in its bytes (ETSI TS 102 114 §3.4/5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsPacking {
    /// The bitstream as specified: bytes in order, most significant bit first.
    CanonicalBe,
    /// 16-bit words stored low byte first: the canonical bytes swapped in pairs.
    Words16Le,
    /// 14 payload bits in the low bits of each 16-bit word, high byte first.
    Words14Be,
    /// 14 payload bits in the low bits of each 16-bit word, low byte first.
    Words14Le,
}
impl DtsPacking {
    /// Size of one storage unit in bytes: 1 for canonical bytes, 2 for words.
    pub const fn source_unit_bytes(self) -> usize {
        if matches!(self, Self::CanonicalBe) {
            1
        } else {
            2
        }
    }
    /// Payload bits one storage unit carries: 8 for a canonical byte, 16 or 14
    /// for a word.
    pub const fn word_payload_bits(self) -> usize {
        match self {
            Self::CanonicalBe => 8,
            Self::Words16Le => 16,
            Self::Words14Be | Self::Words14Le => 14,
        }
    }
    /// Number of payload bits in `source_bytes` bytes stored this way. Fails with
    /// `Truncated` when `source_bytes` is not a whole number of storage units,
    /// `Overflow` when the count does not fit `usize`.
    pub fn payload_bits(self, source_bytes: usize) -> Result<usize> {
        if !source_bytes.is_multiple_of(self.source_unit_bytes()) {
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
    /// The packing in which `prefix` begins with the core sync word
    /// `0x7FFE8001`. The probe allocates nothing and reads at most four bytes,
    /// six for the 14-bit packings. The two unused high bits of a 14-bit word are
    /// not examined and stay in the source.
    ///
    /// Fails with `Truncated` when `prefix` has fewer than four bytes, or matches the
    /// start of a 14-bit sync word and ends before its sixth byte; `Invalid` when it
    /// begins with the sync word in no packing.
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
            if bits == 32 && value == CORE_SYNC {
                return Ok(packing);
            }
            if bits < 32 && value == (CORE_SYNC >> (32 - bits)) {
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
        Ok(value == ((u64::from(CORE_SYNC) << 6) | 63))
    }
}

/// The bounds a [`DtsPackedPayloadJob`] enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsPackedLimits {
    /// Largest record accepted, in bytes.
    pub maximum_source_bytes: usize,
    /// Largest unpacked payload accepted, in bytes: its bits rounded up to
    /// whole bytes.
    pub maximum_storage_bytes: usize,
}

/// The payload bits of one record, unpacked into bytes in bitstream order, with
/// the record as it was stored. Every source bit is kept. When the payload is
/// not a whole number of bytes, the last storage byte ends in zero bits that are
/// not source bits; `valid_bits()` counts the bits that are.
#[derive(Clone, Debug)]
pub struct DtsPackedPayload {
    raw: Bytes,
    storage: Bytes,
    identity: DtsSourceIdentity,
    packing: DtsPacking,
    valid_bits: usize,
}
impl DtsPackedPayload {
    /// The record exactly as it was given, in its packing.
    pub fn source_bytes(&self) -> &Bytes {
        &self.raw
    }
    /// The payload in bitstream order, most significant bit first:
    /// `valid_bits()` rounded up to whole bytes. For
    /// [`DtsPacking::CanonicalBe`] these are the source bytes themselves,
    /// shared.
    pub fn storage_bytes(&self) -> &Bytes {
        &self.storage
    }
    /// Where the record came from, as the caller stated it.
    pub const fn identity(&self) -> DtsSourceIdentity {
        self.identity
    }
    /// The packing the record was read in.
    pub const fn packing(&self) -> DtsPacking {
        self.packing
    }
    /// Number of payload bits in `storage_bytes()`, counted from its first bit.
    pub const fn valid_bits(&self) -> usize {
        self.valid_bits
    }
    /// Number of zero bits after the payload in the last storage byte: 0, 2, 4
    /// or 6. They are not source bits.
    pub fn alignment_zero_bits(&self) -> u8 {
        ((8 - self.valid_bits % 8) % 8) as u8
    }
    /// Where payload bit `position`, counted from zero in bitstream order, lies in the
    /// source. Fails with `Invalid` when `position` is not below `valid_bits()`.
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
    /// Payload bit `position`, counted from zero in bitstream order: 0 or 1. Fails with
    /// `Invalid` when `position` is not below `valid_bits()`.
    pub fn bit(&self, position: usize) -> Result<u8> {
        if position >= self.valid_bits {
            return Err(Error::Invalid("DTS non-source alignment bit"));
        }
        Ok((self.storage.as_ref()[position / 8] >> (7 - position % 8)) & 1)
    }
    /// The two bits above the payload in source word `index`, counted from zero, for
    /// the 14-bit packings: 0 to 3. `None` for [`DtsPacking::Words16Le`], whose words
    /// are all payload. Fails with `Invalid` for [`DtsPacking::CanonicalBe`], which has
    /// no words, and when the record has no word `index`.
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
}

/// Where a job stands. The record and its storage are held only until the payload
/// is returned.
enum State {
    Unpacking {
        raw: Bytes,
        builder: BudgetVec<u8>,
        phase: Phase,
    },
    /// The payload is complete and returned by the next unit of work; a canonical
    /// record is its own storage.
    Frozen {
        raw: Bytes,
        storage: Bytes,
    },
    Complete,
    Failed(Error),
}
/// Unpacks one complete record the caller owns, stored in a known
/// [`DtsPacking`], into a [`DtsPackedPayload`], step by step. A canonical record
/// is returned as it is, sharing its bytes. A record of words is unpacked bit by
/// bit into storage allocated once, at its final size, before the first bit is
/// written.
///
/// Every payload bit and every storage byte costs one unit of work; allocating,
/// completing and returning the storage cost a few more. After `Backpressure`
/// or `Allocation` the job keeps its record and progress and may be stepped
/// again; any other error is returned again by every later step.
pub struct DtsPackedPayloadJob {
    identity: DtsSourceIdentity,
    packing: DtsPacking,
    valid_bits: usize,
    storage_bytes: usize,
    position: usize,
    partial: u8,
    partial_bits: u8,
    state: State,
    end: SourceLocation,
}
impl std::fmt::Debug for DtsPackedPayloadJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failure = match self.state {
            State::Failed(error) => Some(error),
            _ => None,
        };
        f.debug_struct("DtsPackedPayloadJob")
            .field("identity", &self.identity)
            .field("packing", &self.packing)
            .field("valid_bits", &self.valid_bits)
            .field("position", &self.position)
            .field("failure", &failure)
            .finish_non_exhaustive()
    }
}
impl DtsPackedPayloadJob {
    /// A job for `raw`, a record of whole storage units of `packing` that
    /// `identity` locates. `raw` must be charged to `pool`, and the unpacked
    /// storage is charged to it.
    ///
    /// Fails with `ForeignPool` when `raw` is charged to another pool; `Truncated` when
    /// a record of words has an odd number of bytes; `Limit` when the record or its
    /// unpacked storage exceeds `limits`, and, for a record of words, when the pool's
    /// limit cannot hold the record and the storage together; `Overflow` when the
    /// record ends beyond offset `u64::MAX`.
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
        let state = if matches!(packing, DtsPacking::CanonicalBe) {
            State::Frozen {
                storage: raw.clone(),
                raw,
            }
        } else {
            let minimum = raw
                .retained_storage_bytes()
                .checked_add(storage_bytes)
                .and_then(|v| v.checked_add(Bytes::owner_overhead_bytes()))
                .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
                .ok_or(Error::Overflow)?;
            limit(minimum, pool.limit())?;
            State::Unpacking {
                raw,
                builder: BudgetVec::new(pool),
                phase: Phase::Reserve,
            }
        };
        Ok(Self {
            identity,
            packing,
            valid_bits,
            storage_bytes,
            position: 0,
            partial: 0,
            partial_bits: 0,
            state,
            end,
        })
    }
    /// Advances the job by at most the work left in `work`. `Ready` carries the
    /// payload, once; every later step returns `End` with the location of the
    /// record's end in its source. An owned record never requests input.
    ///
    /// Fails with `Cancelled` when `cancel` is set; `Backpressure` or `Allocation` when
    /// the storage cannot be allocated: after these two, and only these, `step` may be
    /// called again.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsPackedPayload>> {
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
    ) -> Result<Step<DtsPackedPayload>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let (raw, builder, phase) = match &mut self.state {
                State::Unpacking {
                    raw,
                    builder,
                    phase,
                } => (raw, builder, phase),
                State::Frozen { .. } => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let State::Frozen { raw, storage } =
                        std::mem::replace(&mut self.state, State::Complete)
                    else {
                        unreachable!("the payload is frozen")
                    };
                    return Ok(Step::Ready(DtsPackedPayload {
                        raw,
                        storage,
                        identity: self.identity,
                        packing: self.packing,
                        valid_bits: self.valid_bits,
                    }));
                }
                State::Complete => return Ok(Step::End(self.end)),
                &mut State::Failed(error) => return Err(error),
            };
            if !work.try_charge(1) {
                return Ok(Step::Yield);
            }
            match phase {
                Phase::Reserve => {
                    builder.try_reserve_exact(self.storage_bytes)?;
                    *phase = Phase::Bits;
                }
                Phase::Bits => {
                    if self.position == self.valid_bits {
                        if self.partial_bits == 0 {
                            *phase = Phase::Freeze;
                        } else {
                            // Representation alignment only. valid_bits does not include these zeros.
                            self.partial <<= 8 - self.partial_bits;
                            *phase = Phase::Byte;
                        }
                        continue;
                    }
                    self.partial =
                        (self.partial << 1) | self.packing.bit(raw.as_ref(), self.position)?;
                    self.partial_bits += 1;
                    self.position += 1;
                    if self.partial_bits == 8 {
                        *phase = Phase::Byte;
                    }
                }
                Phase::Byte => {
                    if builder.len() == builder.capacity() {
                        return Err(Error::Invalid("packed capacity invariant"));
                    }
                    builder
                        .try_push(self.partial)
                        .map_err(|failure| failure.error)?;
                    self.partial = 0;
                    self.partial_bits = 0;
                    *phase = Phase::Bits;
                }
                Phase::Freeze => {
                    let State::Unpacking { raw, builder, .. } =
                        std::mem::replace(&mut self.state, State::Complete)
                    else {
                        unreachable!("the payload is unpacking")
                    };
                    match builder.freeze() {
                        Ok(storage) => self.state = State::Frozen { raw, storage },
                        Err(refused) => {
                            self.state = State::Unpacking {
                                raw,
                                builder: refused.value,
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
