//! Private retained jobs behind the additive typed Matroska reader.
//! Intended as a child of matroska. All transport/storage/time use shared core.
#[path = "blocking.rs"]
pub(crate) mod blocking;
#[path = "colour.rs"]
pub(crate) mod colour;
#[path = "configuration.rs"]
pub(crate) mod configuration;
#[path = "copy_vec.rs"]
pub(crate) mod copy_vec;
#[path = "cues.rs"]
pub(crate) mod cues;
#[path = "epoch.rs"]
pub(crate) mod epoch;
pub(crate) mod hdr10_plus;
#[path = "header.rs"]
pub(crate) mod header;
#[path = "info.rs"]
#[cfg(test)]
pub(crate) mod info;
#[path = "lace.rs"]
pub(crate) mod lace;
#[path = "mapping.rs"]
pub(crate) mod mapping;
pub(crate) mod native_additions;
#[path = "packet.rs"]
pub(crate) mod packet;
#[path = "range.rs"]
pub(crate) mod range;
#[path = "seek.rs"]
pub(crate) mod seek;
#[path = "seek_validate.rs"]
pub(crate) mod seek_validate;
#[path = "side_data.rs"]
pub(crate) mod side_data;
#[path = "syntax.rs"]
pub(crate) mod syntax;
#[path = "tracks.rs"]
pub(crate) mod tracks;
#[path = "usage.rs"]
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
#[path = "jobs_tests.rs"]
mod tests;
