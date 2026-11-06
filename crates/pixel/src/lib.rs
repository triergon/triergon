//! Pixel formats and conversions shared by the capture and playback sides.
//!
//! [`Picture`] is a planar YUV 4:2:0 picture at a macroblock-aligned coded
//! size. [`convert`] turns caller RGBA into it with ITU-R BT.601 limited-range
//! coefficients.
//!
//! [`PictureView`] borrows a decoder's output picture; [`letterbox`] fits one
//! into the 1920x1080 RGBA canvas of the desktop session exactly as the
//! reference scaler chain does.
//!
//! Experimental.
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
