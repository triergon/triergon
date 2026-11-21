#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Hot paths keep the forms they were measured with; these rewrites change their code.
#![allow(
    clippy::checked_conversions,
    clippy::map_unwrap_or,
    clippy::redundant_closure_for_method_calls
)]

pub mod boxes;
pub mod compat;
pub mod config;
pub mod fragment;
pub mod index;
pub mod interleave;
pub mod movie;
pub mod open;
pub mod payload;
pub mod quicktime;
#[cfg(feature = "reference-trace")]
pub mod reference_trace;
pub mod sample_groups;
pub mod seek;
pub mod table;

mod fields;
mod reader;
mod sample;
mod step;

pub use reader::{Packet, Reader, ReaderEvent, ReaderLimits, Track, media_kind};
pub use sample::{Access, CompositionOffset, Sample, SourceTimes};
pub use step::{BuildStatus, CursorStatus, Next};

use triergon_core::{Error, Result};

/// Count limits on what one parse may walk. Memory is bounded separately, by the shared
/// [`Budget`](triergon_core::Budget); a count past its limit is [`Error::Limit`].
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Most children one [`BoxChildren`](boxes::BoxChildren) walk visits, and most boxes
    /// one movie walk visits; default 1,000,000.
    pub max_boxes: u32,
    /// Most rows one table box (`stts`, `stsc`, `stco`, `elst`, `trun`, `sidx`, ...)
    /// declares, and most `trun` boxes in one `traf`; default 1,000,000.
    pub max_records: u32,
    /// Most entries in one `stsd`, and in one `dref` during open; default 256.
    pub max_descriptions: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_boxes: 1_000_000,
            max_records: 1_000_000,
            max_descriptions: 256,
        }
    }
}

pub(crate) fn policy(count: u32, maximum: u32) -> Result<()> {
    if count > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(count).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}
