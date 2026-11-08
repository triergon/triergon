//! The images a frame is drawn from (libass's `ASS_Image` list): each
//! run's shadow, border and fill clipped by the event's clip rectangle and
//! split for `\kf` karaoke (`render_text`, `render_glyph`,
//! `render_glyph_i`), multiplied by a vector clip (`blend_vector_clip`), and
//! the opaque background of `BorderStyle` 4 (`add_background`).

use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Result};
use triergon_raster::{Bitmap, Clip, ClipFrame, Colour, GlyphBitmaps, Limits, Rect, Vector};

use super::bitmaps::Combined;
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
    /// leaves the type unset.
    Background,
}

impl ImageKind {
    /// libass's number for the kind; the background's is the next free one.
    pub const fn number(self) -> i32 {
        match self {
            Self::Fill => 0,
            Self::Border => 1,
            Self::Shadow => 2,
            Self::Background => 3,
        }
    }
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
}

impl Image {
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
    let mut emit = |bitmap: &Arc<Bitmap>,
                    position: Vector,
                    colours: [Rgba; 2],
                    split: i32,
                    kind: ImageKind|
     -> Result<()> {
        for part in clipping
            .clip
            .parts(bitmap, position, split, &clipping.frame)
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
                    shadow,
                    run.origin,
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
                border,
                run.origin,
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
        emit(fill, run.origin, colours, split, ImageKind::Fill)?;
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
    } else {
        if disjoint {
            image.width = 0;
            image.height = 0;
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
        bitmap: Arc::new(bitmap),
        column: 0,
        row: 0,
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
