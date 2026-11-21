//! Reference-compatible policies.
//!
//! The types of the format modules follow the specifications and refuse what they
//! do not allow. Where FFmpeg accepts or frames an input differently and a
//! consumer needs FFmpeg's result, the policies here reproduce FFmpeg's behaviour
//! for that input. Each module is one policy family; its documentation names the
//! FFmpeg build it reproduces.
//!
//! Projections that exist only to compare traces with FFmpeg are not policies. They
//! are compiled only with the `reference-trace` feature.

pub mod nal;
