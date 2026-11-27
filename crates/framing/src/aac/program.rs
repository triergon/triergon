use super::{AacAudioSpecificConfig, AacProgramConfig};
use triergon_bitstream::bits_at;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};

/// The first syntactic element of an ADTS raw data block when it carries an
/// AAC program configuration.
#[derive(Debug)]
#[repr(transparent)]
pub struct AacProgramElement(Bytes);

impl AacProgramElement {
    /// Encodes the configuration's program element, when present, in storage
    /// charged to `pool`. Propagates storage bounds and refuses truncated fields.
    #[cold]
    #[inline(never)]
    pub fn from_config(
        pool: &Budget,
        configuration: &AacAudioSpecificConfig,
    ) -> Result<Option<Self>> {
        configuration
            .program_config()
            .map(|program| raw_block_program(pool, configuration.source_bytes(), program).map(Self))
            .transpose()
    }

    /// Prepends the element to `payload` in the same budget. Returns `None`
    /// when the block already begins with a program element.
    pub fn prepend(&self, payload: &[u8]) -> Result<Option<Bytes>> {
        opening_block(self.0.budget(), &self.0, payload)
    }
}

/// `ID_PCE`, the identifier a program config element has among the syntactic
/// elements of a raw data block.
const ID_PCE: u8 = 5;

/// `program`, the program config element of the AudioSpecificConfig `record`, as the
/// first syntactic element of an ADTS raw data block, charged to `pool`.
///
/// With channel configuration 0 an ADTS header names no layout: a program config
/// element that opens the raw data block gives it (ISO/IEC 14496-3, 1.A.4.3), as
/// the one in the GASpecificConfig does for an AudioSpecificConfig (1.6.2.1, 4.4.1).
/// The element written is `ID_PCE`, the record's fields from `element_instance_tag`
/// through the last list entry, zero bits up to a byte boundary counted from the
/// start of the block (4.4.1.1), then `comment_field_bytes` and the comment.
#[cold]
#[inline(never)]
fn raw_block_program(pool: &Budget, record: &[u8], program: &AacProgramConfig) -> Result<Bytes> {
    let facts = program.facts();
    let comment = program.comment_bytes();
    let fields = comment
        .len()
        .checked_add(1)
        .and_then(|bytes| bytes.checked_mul(8))
        .and_then(|bits| bits.checked_add(usize::from(facts.alignment_bits)))
        .and_then(|tail| facts.source_bits.bit_count.checked_sub(tail))
        .ok_or(Error::Invalid("AAC program config element extent"))?;
    let head = fields.checked_add(3).ok_or(Error::Overflow)?.div_ceil(8);
    let mut element = BudgetVec::try_with_owner_capacity(pool, head + 1 + comment.len())?;
    let (mut word, mut held) = (u64::from(ID_PCE), 3);
    let mut position = facts.source_bits.start_bit;
    let end = position + fields;
    while position < end {
        let width = (end - position).min(24);
        let bits = bits_at(record, position, width as u32).ok_or(Error::Truncated)?;
        word = (word << width) | u64::from(bits);
        held += width;
        position += width;
        while held >= 8 {
            held -= 8;
            element.try_push((word >> held) as u8)?;
        }
        word &= (1 << held) - 1;
    }
    if held > 0 {
        element.try_push((word << (8 - held)) as u8)?;
    }
    element.try_push(comment.len() as u8)?;
    element.try_extend_with(comment.len(), |index| comment[index])?;
    Ok(element.freeze()?)
}

/// The raw data block `payload` behind the program config element `program`, in one
/// allocation charged to `pool`; `None` when the block already opens with a program
/// config element.
#[cold]
#[inline(never)]
fn opening_block(pool: &Budget, program: &[u8], payload: &[u8]) -> Result<Option<Bytes>> {
    if payload.first().is_some_and(|byte| byte >> 5 == ID_PCE) {
        return Ok(None);
    }
    let length = program
        .len()
        .checked_add(payload.len())
        .ok_or(Error::Overflow)?;
    let mut block = BudgetVec::try_with_owner_capacity(pool, length)?;
    block.try_extend_with(program.len(), |index| program[index])?;
    block.try_extend_with(payload.len(), |index| payload[index])?;
    Ok(Some(block.freeze()?))
}
