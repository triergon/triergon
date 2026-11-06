//! Private retained jobs behind the additive typed Matroska reader.
//! Intended as a child of matroska. All transport/storage/time use shared core.
pub(crate) mod blocking;
pub(crate) mod colour;
pub(crate) mod configuration;
pub(crate) mod copy_vec;
pub(crate) mod cues;
pub(crate) mod epoch;
pub(crate) mod hdr10_plus;
pub(crate) mod header;
#[cfg(test)]
pub(crate) mod info;
pub(crate) mod lace;
pub(crate) mod mapping;
pub(crate) mod native_additions;
pub(crate) mod packet;
pub(crate) mod range;
pub(crate) mod seek;
pub(crate) mod seek_validate;
pub(crate) mod side_data;
pub(crate) mod syntax;
pub(crate) mod tracks;
pub(crate) mod usage;
use crate::{Error, Result, incremental::ParseStep};
#[inline(always)]
pub(crate) fn pending<T, U>(step: ParseStep<T>) -> Result<ParseStep<U>> {
    Ok(match step {
        ParseStep::Yield => ParseStep::Yield,
        ParseStep::NeedInput(request) => ParseStep::NeedInput(request),
        ParseStep::Backpressure {
            requested,
            available,
        } => ParseStep::Backpressure {
            requested,
            available,
        },
        ParseStep::Finished => ParseStep::Finished,
        ParseStep::Event(_) => return Err(Error::Invalid("unexpected job event")),
    })
}
#[cfg(test)]
mod tests;
