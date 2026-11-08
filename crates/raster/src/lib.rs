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
mod clip;
mod compose;
mod fp;
mod geometry;
mod glyph;
mod limits;
mod outline;
mod rasterizer;
mod shape;
mod stroke;
mod transform;

pub use bitmap::{ALIGNMENT, Bitmap};
pub use blur::Blur;
pub use clip::{Clip, ClipFrame, ClipPart, ClipParts, Colour, PixelRect};
pub use compose::{Composite, Compositor, Effects, Filter, FilterFlags, GlyphRef};
pub use geometry::{OUTLINE_MAX, Rect, Vector};
pub use glyph::{
    Border, BorderRequest, FrameScale, GlyphBitmaps, GlyphRaster, GlyphRequest, Placement, Run,
};
pub use limits::{Limits, MAX_BITMAP_BYTES};
pub use outline::{
    Curve, Curves, GlyphSource, Outline, PointKind, Segment, SegmentKind, SourcePoint,
};
pub use rasterizer::Rasterizer;
pub use shape::Shape;
pub use stroke::Pen;
pub use transform::{Matrix, Projection};
