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
// The negated float comparisons are libass's: they treat NaN as failing the test.
#![allow(clippy::neg_cmp_op_on_partial_ord)]

mod bitmap;
mod blur;
pub mod compat;
mod fp;
mod geometry;
mod limits;
mod outline;
mod raster;
mod rasterizer;
mod set;
mod shape;
mod stroke;
mod transform;

pub use bitmap::Bitmap;
pub use geometry::{OUTLINE_MAX, Rect, Vector, pixels_26_6};
pub use limits::{Limits, MAX_BITMAP_BYTES};
pub use outline::{Outline, Segment, SegmentKind};
pub use raster::Raster;
pub use shape::Shape;
pub use stroke::Pen;
pub use transform::{BitmapTransform, Matrix, Projection, Quantized};
