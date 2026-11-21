//! One canonical decoder configuration per audio codec, whichever container carried
//! it: [`flac::canonical_configuration`](crate::flac::canonical_configuration),
//! [`opus::canonical_configuration`](crate::opus::canonical_configuration) and
//! [`vorbis::canonical_configuration`](crate::vorbis::canonical_configuration). Each
//! accepted form is recognised by its own leading bytes, never by guessing from the
//! container.

use triergon_core::{BudgetVec, Error, Result};

/// Replaces `out` with the concatenation of `parts`, charging their whole length
/// before any byte is copied.
pub(crate) fn replace(out: &mut BudgetVec<u8>, parts: &[&[u8]]) -> Result<()> {
    out.clear();
    let len = parts
        .iter()
        .try_fold(0usize, |len, part| len.checked_add(part.len()))
        .ok_or(Error::Overflow)?;
    out.try_reserve_exact(len)?;
    for part in parts {
        out.try_extend_from_slice(part)?;
    }
    Ok(())
}
