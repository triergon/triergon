//! The push reader over a random-access source, shared by the unit and integration
//! tests. Library names resolve at `crate::`, as in the other kits.
use crate::{Limits, PayloadOwnership, Reader};
use triergon_core::{Budget, SourceCapabilities, SourceId};

/// A push reader over a random-access source of `length` bytes (`None`: not known
/// until the source finishes) that asks for windows of at most `window` bytes.
pub(crate) fn source_reader(
    pool: &Budget,
    length: Option<u64>,
    limits: Limits,
    window: usize,
    ownership: PayloadOwnership,
) -> Reader {
    let capabilities = SourceCapabilities {
        length,
        random_access: true,
    };
    Reader::new_with_payload_ownership(
        pool,
        SourceId(28001),
        capabilities,
        limits,
        window,
        ownership,
    )
    .unwrap()
}
