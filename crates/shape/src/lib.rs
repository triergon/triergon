#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used
)]
// Pending: `clippy::expect_used`.

pub mod bidi;
mod buffer;
mod dump;
mod face;
mod fallback;
#[allow(
    missing_docs,
    reason = "pending: the fields of Scale and Extents, Metrics::h_advance and Metrics::extents"
)]
mod metrics;
mod normalize;
mod ot;
mod plan;
mod props;
mod shape;
mod shapers;
mod tag;
mod unicode;
mod work;

pub use buffer::{Buffer, Glyph, Position};
#[doc(hidden)]
pub use dump::{ReferenceProperties, reference_properties};
pub use face::Face;
pub use metrics::{Extents, Metrics, OtMetrics, Scale};
pub use plan::{Feature, Plan};
pub use shape::shape;
pub use tag::{Direction, Language, Script, Tag};
pub use work::Limits;
