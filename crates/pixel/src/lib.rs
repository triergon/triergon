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

mod convert;
mod error;
pub mod letterbox;
mod picture;
mod view;
mod ycbcr;

pub use convert::{Converter, Orientation, rgba_to_yuv420};
pub use error::{Error, Result};
pub use picture::{CodedPicture, MacroblockGeometry};
pub use view::{Colour, PictureView, Plane, SampleAspectRatio};
pub use ycbcr::{ChannelRounding, YcbcrMatrix};
