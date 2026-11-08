//! Rectangle clipping of a placed bitmap into the image parts libass emits
//! (`render_glyph`), with the `\kf` karaoke split into a left and a right colour.

use crate::bitmap::Bitmap;
use crate::geometry::Vector;

/// A rectangle in whole frame pixels, `right` and `bottom` exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PixelRect {
    /// The first column inside.
    pub left: i32,
    /// The first row inside.
    pub top: i32,
    /// The first column past the right edge.
    pub right: i32,
    /// The first row past the bottom edge.
    pub bottom: i32,
}

/// A `\clip` (or, `inverse`, `\iclip`) rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Clip {
    /// The clip rectangle in frame pixels (libass's `clip_x0` to `clip_y1`).
    pub rect: PixelRect,
    /// The bitmap is visible outside `rect` rather than inside it.
    pub inverse: bool,
}

/// The frame a bitmap is clipped against: its size, which bounds a normal
/// clip, and the script's content area, which bounds an inverse one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClipFrame {
    /// The frame width in pixels; a normal clip is clamped to `0..=width`.
    pub width: i32,
    /// The frame height in pixels; a normal clip is clamped to `0..=height`.
    pub height: i32,
    /// The script's play area in frame pixels, which bounds the parts of an
    /// inverse clip.
    pub content: PixelRect,
}

/// Which colour of a karaoke split a part is drawn in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Colour {
    /// Left of the split.
    #[default]
    First,
    /// From the split on.
    Second,
}

/// A visible part of a bitmap: `width` × `height` pixels from (`source_x`,
/// `source_y`) of the bitmap, drawn at `position` in the frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClipPart {
    /// The frame position of the part's top-left pixel, in whole pixels.
    pub position: Vector,
    /// The bitmap column the part starts at.
    pub source_x: usize,
    /// The bitmap row the part starts at.
    pub source_y: usize,
    /// The part's width in pixels, never zero.
    pub width: usize,
    /// The part's height in pixels, never zero.
    pub height: usize,
    /// The karaoke colour the part is drawn in.
    pub colour: Colour,
}

/// Up to eight parts, in the order libass emits them.
#[derive(Clone, Copy, Debug, Default)]
struct ClipParts {
    parts: [ClipPart; 8],
    len: usize,
}

impl ClipParts {
    fn push(&mut self, part: ClipPart) {
        self.parts[self.len] = part;
        self.len += 1;
    }
}

/// A rectangle in bitmap pixels, computed in 64 bits so no input overflows.
#[derive(Clone, Copy, Debug, Default)]
struct Span {
    x0: i64,
    y0: i64,
    x1: i64,
    y1: i64,
}

impl Span {
    fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }
}

impl Clip {
    /// The parts of `bitmap`, its position offset by `origin` pixels, that this
    /// clip leaves visible in `frame`, each split at frame column `split` into a
    /// part in the first colour (left) and one in the second (right): at most
    /// eight, in the order libass emits them (`render_glyph`).
    pub fn parts(
        &self,
        bitmap: &Bitmap,
        origin: Vector,
        split: i32,
        frame: &ClipFrame,
    ) -> impl ExactSizeIterator<Item = ClipPart> + use<> {
        let dst_x = i64::from(origin.x) + i64::from(bitmap.position.x);
        let dst_y = i64::from(origin.y) + i64::from(bitmap.position.y);
        let split = i64::from(split) - dst_x;
        let size = (bitmap.width() as i64, bitmap.height() as i64);
        let mut spans = [Span::default(); 4];
        let count = if self.inverse {
            self.outside(dst_x, dst_y, size, frame, &mut spans)
        } else {
            self.inside(dst_x, dst_y, size, frame, &mut spans)
        };
        let mut parts = ClipParts::default();
        for span in spans[..count].iter().filter(|s| !s.is_empty()) {
            let part = |x0: i64, x1: i64, colour| ClipPart {
                position: Vector::new((dst_x + x0) as i32, (dst_y + span.y0) as i32),
                source_x: x0 as usize,
                source_y: span.y0 as usize,
                width: (x1 - x0) as usize,
                height: (span.y1 - span.y0) as usize,
                colour,
            };
            if split > span.x0 {
                parts.push(part(span.x0, split.min(span.x1), Colour::First));
            }
            if split < span.x1 {
                parts.push(part(split.max(span.x0), span.x1, Colour::Second));
            }
        }
        parts.parts.into_iter().take(parts.len)
    }

    /// A normal clip: the clip rectangle clamped to the frame (as `FFMINMAX`
    /// does, the upper bound last), within the bitmap.
    fn inside(
        &self,
        dst_x: i64,
        dst_y: i64,
        (w, h): (i64, i64),
        frame: &ClipFrame,
        spans: &mut [Span; 4],
    ) -> usize {
        let PixelRect {
            left,
            top,
            right,
            bottom,
        } = self.rect;
        let (fw, fh) = (i64::from(frame.width), i64::from(frame.height));
        let clamp = |v: i32, extent: i64| i64::from(v).max(0).min(extent);
        let (x0, y0) = (clamp(left, fw), clamp(top, fh));
        let (x1, y1) = (clamp(right, fw), clamp(bottom, fh));
        let span = Span {
            x0: (x0 - dst_x).max(0),
            y0: (y0 - dst_y).max(0),
            x1: w + (x1 - dst_x - w).min(0),
            y1: h + (y1 - dst_y - h).min(0),
        };
        if span.is_empty() {
            return 0;
        }
        spans[0] = span;
        1
    }

    /// An inverse clip: the four bands around the rectangle, each limited to
    /// the content area.
    fn outside(
        &self,
        dst_x: i64,
        dst_y: i64,
        (w, h): (i64, i64),
        frame: &ClipFrame,
        spans: &mut [Span; 4],
    ) -> usize {
        let cx0 = i64::from(self.rect.left) - dst_x;
        let cy0 = i64::from(self.rect.top) - dst_y;
        let cx1 = i64::from(self.rect.right) - dst_x;
        let cy1 = i64::from(self.rect.bottom) - dst_y;
        let bands = [
            Span {
                x0: 0,
                y0: 0,
                x1: cx0.min(w),
                y1: h,
            },
            Span {
                x0: cx0.max(0),
                y0: 0,
                x1: cx1.min(w),
                y1: cy0.min(h),
            },
            Span {
                x0: cx0.max(0),
                y0: cy1.max(0),
                x1: cx1.min(w),
                y1: h,
            },
            Span {
                x0: cx1.max(0),
                y0: 0,
                x1: w,
                y1: h,
            },
        ];
        let area = frame.content;
        let (zx, zy) = (i64::from(area.left), i64::from(area.top));
        let (sx, sy) = (i64::from(area.right), i64::from(area.bottom));
        let mut count = 0;
        for band in bands.into_iter().filter(|s| !s.is_empty()) {
            spans[count] = Span {
                x0: if band.x0 + dst_x < zx {
                    zx - dst_x
                } else {
                    band.x0
                },
                y0: if band.y0 + dst_y < zy {
                    zy - dst_y
                } else {
                    band.y0
                },
                x1: if band.x1 + dst_x > sx {
                    sx - dst_x
                } else {
                    band.x1
                },
                y1: if band.y1 + dst_y > sy {
                    sy - dst_y
                } else {
                    band.y1
                },
            };
            count += 1;
        }
        count
    }
}
