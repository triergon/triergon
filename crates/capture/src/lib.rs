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

mod error;
mod session;

pub use error::{CaptureError, Result};
pub use session::{FrameReport, Session, SessionConfig, Sink};
pub use triergon_codec_mpeg1 as mpeg1;
pub use triergon_mpegts::mux;
pub use triergon_pixel as pixel;
