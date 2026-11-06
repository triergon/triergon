//! Pixel formats and conversions shared by the capture and playback sides.
//!
//! [`Picture`] is a planar YUV 4:2:0 picture at a macroblock-aligned coded
//! size. [`convert`] turns caller RGBA into it with ITU-R BT.601 limited-range
//! coefficients (the capture contract in `research/capture/contract.json`).
//!
//! [`PictureView`] borrows a decoder's output picture; [`letterbox`] fits one
//! into the 1920x1080 RGBA canvas of the desktop session exactly as the
//! reference scaler chain does (`research/d4/RGBA-CONTRACT.json`).
//!
//! Experimental; nothing here is qualified on its own.
#![forbid(unsafe_code)]

pub mod convert;
mod error;
pub mod letterbox;
mod picture;
mod view;

pub use convert::{Converter, Orientation, rgba_to_yuv420};
pub use error::{Error, Result};
pub use picture::{Geometry, Picture};
pub use view::{Colour, PictureView, Plane};
