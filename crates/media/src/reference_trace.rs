//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! These report values in the form the harness compares with FFmpeg's output. A
//! consumer has no use for them outside a comparison, and they are absent from
//! default builds. With the feature enabled,
//! [`Colour::trace_fields`](crate::colour::Colour::trace_fields) is also available:
//! the colour properties, content light level and mastering display metadata
//! FFmpeg's public API reports for a track, encoded as the colour trace's 32
//! integers.
//!
//! Reference: the `matroska` demuxer at FFmpeg git revision `d32b387f` on the 9.0
//! branch.
