//! Independent bounded subtitle primitives. Experimental.
//!
//! DVB interlaced rasterization, selected method-0 display state and explicit
//! source-colour projections, plus bounded strict text source-ingestion profiles.
//! Full subtitle parity and presentation scheduling are out of scope.
#![forbid(unsafe_code)]

pub mod dvb;
pub mod text;

pub mod context;

pub mod dvd;
pub mod pgs;
