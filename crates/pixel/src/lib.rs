#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used
)]
// Pending: `missing_docs`, `clippy::expect_used`.

pub mod convert;
mod error;
pub mod letterbox;
mod picture;
mod view;

pub use convert::{Converter, Orientation, rgba_to_yuv420};
pub use error::{Error, Result};
pub use picture::{Geometry, Picture};
pub use view::{Colour, PictureView, Plane};
