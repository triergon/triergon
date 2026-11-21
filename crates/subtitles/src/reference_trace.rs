//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! These report values in the form FFmpeg's public API exposes them, so tests
//! can compare them with recorded reference output. They are not behaviour a
//! consumer selects outside a comparison, and they are absent from default
//! builds. Their reference revision is the one named on the matching
//! [`compat`](crate::compat) family.

pub mod carriage;
