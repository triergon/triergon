//! H.265 / HEVC Annex-B video decoder (ITU-T H.265). Pure safe Rust.
//!
//! Selected feature set: Main, Main 10 and Main Still Picture profiles (4:2:0, 8/10-bit).
//! Range extension, screen content coding and multi-layer tools are refused with
//! [`DecodeError::Unsupported`] naming the tool; NAL units with nuh_layer_id > 0 are skipped.
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::expect_used
)]
// Pending: `missing_docs`, `unreachable_pub`, `clippy::unwrap_used`.
#![cfg_attr(feature = "simd", feature(portable_simd))]
// Pending: these rewrites change the generated code of measured hot paths and are
// adopted with an A/B timing run.
#![allow(
    clippy::bool_to_int_with_if,
    clippy::inconsistent_struct_constructor,
    clippy::manual_let_else,
    clippy::map_unwrap_or,
    clippy::match_wildcard_for_single_variants,
    clippy::nonminimal_bool,
    clippy::single_match_else
)]
mod bits;
mod cabac;
mod ctu;
mod deblock;
mod decoder;
mod error;
mod inter;
mod intra;
mod kernels;
pub mod nal;
mod picture;
mod ps;
mod sao;
mod slice;
mod state;
mod tables;
mod transform;

pub use decoder::{ChromaFormat, ColourDescription, Decoder, Picture, Plane, Row};
pub use error::DecodeError;
pub use nal::AccessUnitSplitter;
