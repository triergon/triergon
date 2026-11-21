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
#![cfg_attr(feature = "simd", feature(portable_simd))]
// Conditions read as the specification writes them: `if c { 1 } else { 0 }` for a
// context or list index and the negated conjunctions of its availability rules.
#![allow(clippy::bool_to_int_with_if, clippy::nonminimal_bool)]
// Sample kernels and per-block steps take their geometry and operands as plain
// arguments, not parameter structs: the shape the hot paths are measured in.
#![allow(clippy::too_many_arguments)]
mod bits;
mod cabac;
mod ctu;
mod deblock;
mod decoder;
mod error;
mod inter;
mod intra;
mod kernels;
mod nal;
mod output;
mod picture;
mod ps;
mod sao;
mod slice;
mod state;
mod tables;
mod transform;

pub use decoder::{Decoder, Limits};
pub use error::DecodeError;
pub use nal::AccessUnitSplitter;
pub use output::{ChromaFormat, ColourDescription, Picture, Plane, Row};
