//! Development-capture pipeline: RGBA → YUV 4:2:0 conversion, MPEG-1 video
//! encoding and MPEG-TS muxing, independently authored from ITU-R BT.601,
//! ISO/IEC 11172-2 (via the freely available ITU-T H.262 syntax) and ITU-T
//! H.222.0. The frozen contract lives in `research/capture/contract.json`.
//!
//! Experimental; this crate is not qualified. A passing slice is not O62.
#![forbid(unsafe_code)]

pub mod convert;
mod error;
pub mod mpeg1;
mod picture;
pub mod session;
pub mod ts;

pub use convert::{Orientation, rgba_to_yuv420};
pub use error::{CaptureError, Result};
pub use picture::{Geometry, Picture};
pub use session::{Session, SessionConfig, Sink};
