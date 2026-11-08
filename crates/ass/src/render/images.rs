//! The images a frame is drawn from (libass's `ASS_Image` list): each
//! run's shadow, border and fill clipped by the event's clip rectangle and
//! split for `\kf` karaoke (`render_text`, `render_glyph`,
//! `render_glyph_i`), multiplied by a vector clip (`blend_vector_clip`), and
//! the opaque background of `BorderStyle` 4 (`add_background`).

use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Result};
use triergon_raster::{Bitmap, Clip, ClipFrame, Colour, GlyphBitmaps, Limits, Rect};

use super::bitmaps::{Combined, RunKey};
use crate::{Karaoke, Rgba};

/// What an image draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageKind {
    /// Glyph fills (libass's `IMAGE_TYPE_CHARACTER`).
    Fill,
    /// Borders (`IMAGE_TYPE_OUTLINE`).
    Border,
    /// Shadows (`IMAGE_TYPE_SHADOW`).
    Shadow,
    /// The opaque box of `BorderStyle` 4 behind an event, for which libass
    /// leaves the type uninitialised.
    Background,
}

/// How a frame differs from the one rendered before it, as libass's
/// `ass_detect_change` reports it: a player may keep showing the previous
/// images when nothing changed, or move them when only positions did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Change {
    /// The same images, pixels and places (libass's 0).
    None,
    /// The same images and pixels, some in other places (1).
    Positions,
    /// Other images or pixels (2): a different count, size, colour or bitmap.
    Content,
}

impl Change {
    /// `ass_detect_change`: image by image, the greatest difference; a list
    /// longer or shorter than the previous one is new content.
    pub(super) fn between(previous: &[Image], current: &[Image]) -> Self {
        if previous.len() != current.len() {
            return Self::Content;
        }
        previous
            .iter()
            .zip(current)
            .map(|(a, b)| a.compare(b))
            .max()
            .unwrap_or(Self::None)
    }
}

/// The part of a composited run an image shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Fill,
    Border,
    Shadow,
}

/// Where an image's pixels come from: a composited run, which libass's
/// composite cache gives the same bitmap for an equal key, or pixels made
/// for this image alone.
#[derive(Clone, Debug)]
enum Source {
    Run(Arc<RunKey>, Part),
    Own,
}

/// One image of a frame: an alpha bitmap to blend in one colour at a frame
/// position. Its pixels may be shared with other images and with the
/// renderer's caches.
#[derive(Clone, Debug)]
pub struct Image {
    /// What the image draws.
    pub kind: ImageKind,
    /// The frame column of its left edge; it may lie outside the frame.
    pub x: i32,
    /// The frame row of its top edge.
    pub y: i32,
    /// The colour, alpha inverted as in ASS: 0 is opaque.
    pub colour: Rgba,
    width: usize,
    height: usize,
    bitmap: Arc<Bitmap>,
    column: usize,
    row: usize,
    /// libass's row stride, 0 for an image a vector clip emptied.
    stride: usize,
    source: Source,
}

impl Image {
    /// `ass_image_compare`: other content when the size, stride, colour or
    /// bitmap differ, else other positions when the place differs.
    fn compare(&self, other: &Self) -> Change {
        let same_pixels = match (&self.source, &other.source) {
            (Source::Run(a, p), Source::Run(b, q)) => {
                p == q
                    && self.column == other.column
                    && self.row == other.row
                    && (Arc::ptr_eq(a, b) || **a == **b)
            }
            _ => false,
        };
        if self.width != other.width
            || self.height != other.height
            || self.stride != other.stride
            || self.colour != other.colour
            || !same_pixels
        {
            Change::Content
        } else if self.x != other.x || self.y != other.y {
            Change::Positions
        } else {
            Change::None
        }
    }

    /// The width in pixels.
    pub const fn width(&self) -> usize {
        self.width
    }

    /// The height in pixels.
    pub const fn height(&self) -> usize {
        self.height
    }

    /// The alpha rows, top to bottom, each [`Image::width`] bytes: 255 is
    /// fully covered.
    pub fn rows(&self) -> impl ExactSizeIterator<Item = &[u8]> + '_ {
        let stride = self.bitmap.stride();
        let data = self.bitmap.data();
        (0..self.height).map(move |y| {
            let start = (self.row + y) * stride + self.column;
            &data[start..start + self.width]
        })
    }

    /// Moves the image down by `shift` rows, cutting it to the frame's
    /// `height` (libass's `shift_event`).
    pub(super) fn shift(&mut self, shift: i32, frame_height: i32) {
        let mut y = self.y.wrapping_add(shift);
        let mut h = self.height as i32;
        if y < 0 {
            h += y;
            self.row = self.row.wrapping_add(y.unsigned_abs() as usize);
            y = 0;
        }
        if y.wrapping_add(h) >= frame_height {
            h -= y.wrapping_add(h) - frame_height;
        }
        if h <= 0 {
            h = 0;
            y = 0;
        }
        self.y = y;
        self.height = h as usize;
    }

    fn is_transparent(&self) -> bool {
        self.colour.0 & 0xff == 0xff
    }
}

/// How an event's images are clipped.
#[derive(Clone, Copy, Debug)]
pub(super) struct Clipping {
    pub clip: Clip,
    pub frame: ClipFrame,
}

/// A vector clip drawn in the frame: its drawing's fill.
#[derive(Debug)]
pub(super) struct ClipMask {
    pub bitmaps: GlyphBitmaps,
    pub inverse: bool,
}

/// `render_text`: the shadows, then the borders, then the fills of every run
/// (a `\ko` border waits for its syllable), clipped, then the vector clip.
pub(super) fn render_text(
    runs: &[Combined],
    clipping: &Clipping,
    with_shadows: bool,
    mask: Option<&ClipMask>,
    budget: &Budget,
    out: &mut BudgetVec<Image>,
) -> Result<()> {
    let start = out.len();
    let mut emit = |run: &Combined,
                    which: Part,
                    bitmap: &Arc<Bitmap>,
                    colours: [Rgba; 2],
                    split: i32,
                    kind: ImageKind|
     -> Result<()> {
        for part in clipping
            .clip
            .parts(bitmap, run.origin, split, &clipping.frame)
        {
            out.try_push(Image {
                kind,
                x: part.position.x,
                y: part.position.y,
                colour: match part.colour {
                    Colour::First => colours[0],
                    Colour::Second => colours[1],
                },
                width: part.width,
                height: part.height,
                bitmap: Arc::clone(bitmap),
                column: part.source_x,
                row: part.source_y,
                stride: bitmap.stride(),
                source: Source::Run(Arc::clone(&run.key), which),
            })
            .map_err(|e| e.error)?;
        }
        Ok(())
    };
    const WHOLE: i32 = 1_000_000;
    if with_shadows {
        for run in runs {
            if let Some(shadow) = &run.shadow {
                emit(
                    run,
                    Part::Shadow,
                    shadow,
                    [run.colours[3], Rgba(0)],
                    WHOLE,
                    ImageKind::Shadow,
                )?;
            }
        }
    }
    for run in runs {
        if let Some(border) = &run.border
            && !(run.effect == Some(Karaoke::Outline) && run.effect_timing <= 0)
        {
            emit(
                run,
                Part::Border,
                border,
                [run.colours[2], Rgba(0)],
                WHOLE,
                ImageKind::Border,
            )?;
        }
    }
    for run in runs {
        let Some(fill) = &run.fill else { continue };
        let (colours, split) = match run.effect {
            Some(Karaoke::Switch | Karaoke::Outline) => {
                let colour = if run.effect_timing > 0 {
                    run.colours[0]
                } else {
                    run.colours[1]
                };
                ([colour, Rgba(0)], WHOLE)
            }
            Some(Karaoke::Fill) => ([run.colours[0], run.colours[1]], run.effect_timing),
            None => ([run.colours[0], Rgba(0)], WHOLE),
        };
        emit(run, Part::Fill, fill, colours, split, ImageKind::Fill)?;
    }
    if let Some(mask) = mask {
        for image in &mut out.as_mut_slice()[start..] {
            blend_vector_clip(image, mask, budget)?;
        }
    }
    Ok(())
}

/// `blend_vector_clip` on one image: multiplied by the clip (or its inverse)
/// where they overlap, into new pixels; a regular clip leaves an empty image
/// where they do not.
fn blend_vector_clip(image: &mut Image, mask: &ClipMask, budget: &Budget) -> Result<()> {
    let (ax, ay) = (image.x, image.y);
    let (aw, ah) = (image.width as i32, image.height as i32);
    // `Raster::draw` returns the fill whenever it draws anything.
    let Some(clip) = mask.bitmaps.glyph() else {
        return Ok(());
    };
    let origin = mask.bitmaps.origin();
    let bx = origin.x.wrapping_add(clip.position.x);
    let by = origin.y.wrapping_add(clip.position.y);
    let (bw, bh) = (clip.width() as i32, clip.height() as i32);
    let left = ax.max(bx);
    let top = ay.max(by);
    let right = (ax + aw).min(bx + bw);
    let bottom = (ay + ah).min(by + bh);
    let (w, h) = (right - left, bottom - top);
    let disjoint = ax + aw < bx || ay + ah < by || ax > bx + bw || ay > by + bh || h == 0 || w == 0;
    if mask.inverse {
        if disjoint {
            return Ok(());
        }
        let mut pixels = copy(image, 0, 0, image.width, image.height, budget)?;
        pixels.multiply_inverse(clip, bx - ax, by - ay);
        image.bitmap = Arc::new(pixels);
        (image.column, image.row) = (0, 0);
        image.source = Source::Own;
    } else {
        if disjoint {
            image.width = 0;
            image.height = 0;
            image.stride = 0;
            return Ok(());
        }
        let (aleft, atop) = ((left - ax) as usize, (top - ay) as usize);
        let mut pixels = copy(image, aleft, atop, w as usize, h as usize, budget)?;
        pixels.multiply(clip, bx - left, by - top);
        image.bitmap = Arc::new(pixels);
        (image.column, image.row) = (0, 0);
        image.x = left;
        image.y = top;
        image.width = w as usize;
        image.height = h as usize;
        image.stride = image.bitmap.stride();
        image.source = Source::Own;
    }
    Ok(())
}

/// The `width` × `height` pixels of `image` from (`x`, `y`) as a bitmap of
/// their own.
fn copy(
    image: &Image,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    budget: &Budget,
) -> Result<Bitmap> {
    let mut bitmap = Bitmap::new(budget, width, height, &Limits::default())?;
    for (dst, src) in bitmap.rows_mut().zip(image.rows().skip(y)) {
        dst[..width].copy_from_slice(&src[x..x + width]);
    }
    Ok(bitmap)
}

/// `add_background`: the box of a `BorderStyle` 4 event, its positive shadow
/// added around it, in the back colour.
pub(super) fn background(
    area: Rect,
    frame: (i32, i32),
    colour: Rgba,
    budget: &Budget,
) -> Result<Option<Image>> {
    let clamp = |v: i32, max: i32| v.clamp(0, max);
    let left = clamp(area.x_min, frame.0);
    let top = clamp(area.y_min, frame.1);
    let right = clamp(area.x_max, frame.0);
    let bottom = clamp(area.y_max, frame.1);
    let (w, h) = (right - left, bottom - top);
    if w < 1 || h < 1 {
        return Ok(None);
    }
    let mut bitmap = Bitmap::new(budget, w as usize, h as usize, &Limits::default())?;
    for row in bitmap.rows_mut() {
        row.fill(0xff);
    }
    Ok(Some(Image {
        kind: ImageKind::Background,
        x: left,
        y: top,
        colour,
        width: w as usize,
        height: h as usize,
        stride: bitmap.stride(),
        bitmap: Arc::new(bitmap),
        column: 0,
        row: 0,
        source: Source::Own,
    }))
}

/// Drops the fully transparent images, as libass does when it joins the
/// events' lists.
pub(super) fn drop_transparent(images: &mut BudgetVec<Image>) {
    let mut kept = 0;
    let slice = images.as_mut_slice();
    for i in 0..slice.len() {
        if !slice[i].is_transparent() {
            slice.swap(kept, i);
            kept += 1;
        }
    }
    while images.len() > kept {
        images.pop();
    }
}
