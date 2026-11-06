//! Independent bounded subtitle primitives. Experimental.
//!
//! DVB interlaced rasterization, selected method-0 display state and explicit
//! source-colour projections, plus bounded strict text source-ingestion profiles.
//! Full subtitle parity and presentation scheduling are out of scope.
#![forbid(unsafe_code)]
#![warn(clippy::print_stdout, clippy::print_stderr)]
// Pending: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`, `clippy::unwrap_used`, `clippy::expect_used`.

pub mod dvb;
pub mod text;

pub mod context;

pub mod dvd;
pub mod pgs;
