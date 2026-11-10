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
mod face;
mod fallback;
mod language;
mod metrics;
mod normalize;
mod ot;
mod plan;
mod props;
mod rotations;
mod shape;
mod shapers;
mod tag;
pub mod unicode;
mod work;

pub use buffer::{Buffer, Glyph, Position};
pub use face::Face;
pub use language::Language;
pub use metrics::{Extents, Metrics, OtMetrics, Scale, SizedMetrics};
pub use plan::{Feature, Plan};
pub use rotations::{FaceRotations, GlyphRotations, rotates};
pub use shape::shape;
pub use tag::{Direction, Script, Tag};
pub use work::Limits;
