//! H.265 / HEVC Annex-B video decoder (ITU-T H.265). Pure safe Rust. Not yet qualified.
//!
//! Selected feature set: Main, Main 10 and Main Still Picture profiles (4:2:0, 8/10-bit).
//! Range extension, screen content coding and multi-layer tools are refused with
//! [`DecodeError::Unsupported`] naming the tool; NAL units with nuh_layer_id > 0 are skipped.
#![forbid(unsafe_code)]
#![cfg_attr(feature = "simd", feature(portable_simd))]

mod bits;
mod cabac;
mod ctu;
mod deblock;
mod decoder;
mod error;
#[cfg(feature = "simd")]
mod filter_simd;
mod inter;
mod intra;
#[cfg(feature = "simd")]
mod mc_simd;
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
