//! Development-capture session: RGBA in, MPEG-1-in-TS bytes out.
//!
//! A thin composition of the BT.601 converter (`triergon-pixel`), the MPEG-1
//! video encoder (`triergon-codec-mpeg1`) and the single-program TS writer
//! (`triergon_mpegts::mux`). The frozen contract lives in
//! `research/capture/contract.json`.
//!
//! Experimental; this crate is not qualified. A passing slice is not O62.
#![forbid(unsafe_code)]

mod error;
mod session;

pub use error::{CaptureError, Result};
pub use session::{FrameReport, Session, SessionConfig, Sink};
pub use triergon_codec_mpeg1 as mpeg1;
pub use triergon_mpegts::mux;
pub use triergon_pixel as pixel;
