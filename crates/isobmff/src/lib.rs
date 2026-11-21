#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used
)]
// Pending: `missing_docs`, `clippy::expect_used`.
// Pending: these rewrites change the generated code of measured hot paths and are
// adopted with an A/B timing run.
#![allow(
    clippy::assigning_clones,
    clippy::checked_conversions,
    clippy::map_unwrap_or,
    clippy::redundant_closure_for_method_calls,
    clippy::ref_option,
    clippy::unnested_or_patterns
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

pub use reader::{Packet, Reader, ReaderEvent, ReaderLimits, Track, media_kind};
pub use sample::{Access, CompositionOffset, Sample, SampleStep, SourceTimes};

use triergon_core::{Error, Result};

/// Format-policy counts; live capacity is exclusively owned/accounted by core.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_boxes: u32,
    pub max_records: u32,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildStatus {
    Yield,
    Ready,
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
