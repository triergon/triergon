//! How libass's renderer draws ASS glyphs and combines them into images: the
//! transform of `\frx` `\fry` `\frz` `\fax` `\fay` and its quantisation
//! against the glyph's run, the scale a border is stroked at and the opaque
//! box of `\bord` style 3 ([`Run`], [`GlyphRequest`]), the composition of a
//! run's fill, border and shadow under `\blur`, `\be` and `\shad`
//! ([`Composite`], [`Effects`]), and the rectangle clipping of `\clip` and
//! `\iclip` with the `\kf` split into image parts ([`Clip`]).
//!
//! Reference: libass 0.17.5.

mod clip;
mod compose;
mod glyph;

pub use clip::{Clip, ClipFrame, ClipPart, Colour};
pub use compose::{Border, Composite, Effects, EffectsKey, Shadow};
pub use glyph::{
    BorderStroke, FrameScale, GlyphBitmaps, GlyphPlacement, GlyphRequest, GlyphSource,
    PlacedBitmap, Placement, Run,
};
