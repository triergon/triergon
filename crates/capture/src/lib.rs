//! Development-capture session: RGBA in, MPEG-1-in-TS bytes out.
//!
//! A thin composition of the BT.601 converter (`triergon-pixel`), the MPEG-1
//! video encoder (`triergon-codec-mpeg1`) and the single-program TS writer
//! (`triergon_mpegts::mux`).
//!
//! Experimental.
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Pending: `missing_docs`.

mod error;
mod session;

pub use error::{CaptureError, Result};
pub use session::{FrameReport, Session, SessionConfig, Sink};
pub use triergon_codec_mpeg1 as mpeg1;
pub use triergon_mpegts::mux;
pub use triergon_pixel as pixel;
