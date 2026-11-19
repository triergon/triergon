//! The images a frame is drawn from (libass's `ASS_Image` list): each
//! run's shadow, border and fill clipped by the event's clip rectangle and
//! split for `\kf` karaoke (`render_text`, `render_glyph`,
//! `render_glyph_i`), multiplied by a vector clip (`blend_vector_clip`), and
//! the opaque background of `BorderStyle` 4 (`add_background`).

use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};
use triergon_raster::{Bitmap, Clip, ClipFrame, Colour, Limits, PlacedBitmap, Rect};

use super::bitmaps::{Combined, Composited, SharedBitmap};
use super::cache::Kept;
use crate::{Karaoke, Rgba, charge};

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

/// Where an image's pixels come from: a part of a composited run, which
/// the composite cache gives for an equal key as long as anything holds
/// it, or pixels made for this image alone, which `blend_vector_clip`
/// makes for every clipped image.
#[derive(Clone, Debug)]
enum Source {
    Run(Kept<Composited>, Part),
    Own(Arc<Bitmap>),
    /// Every row of a style-4 background shows the same opaque pixels.
    Solid(Arc<BudgetVec<u8>>),
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
    column: usize,
    row: usize,
    /// libass's row stride, 0 for an image a vector clip emptied.
    stride: usize,
    source: Source,
}

impl Image {
    /// `ass_image_compare`: other content when the size, stride, colour or
    /// bitmap differ, else other positions when the place differs. libass
    /// compares the bitmap's address: the same pixels of the same cached
    /// composite (`ass_render.c:3286-3306`).
    fn compare(&self, other: &Self) -> Change {
        let same_pixels = match (&self.source, &other.source) {
            (Source::Run(a, p), Source::Run(b, q)) => {
                Kept::ptr_eq(a, b) && p == q && self.column == other.column && self.row == other.row
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
        let (stride, data) = match &self.source {
            Source::Run(composite, part) => match part {
                Part::Fill => composite.fill.as_ref(),
                Part::Border => composite.border.as_ref(),
                Part::Shadow => composite.shadow.as_ref(),
            }
            .map_or((0, &[][..]), |b| (b.stride(), b.data())),
            Source::Own(bitmap) => (bitmap.stride(), bitmap.data()),
            Source::Solid(row) => (0, row.as_slice()),
        };
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

/// A vector clip drawn in the frame: its drawing's fill, held in the bitmap
/// cache while it clips.
#[derive(Debug)]
pub(super) struct ClipMask {
    pub bitmap: PlacedBitmap<SharedBitmap>,
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
    work: &mut WorkBudget,
    out: &mut BudgetVec<Image>,
) -> Result<()> {
    let mut emit = |run: &Combined,
                    which: Part,
                    bitmap: &Bitmap,
                    colours: [Rgba; 2],
                    split: i32,
                    kind: ImageKind|
     -> Result<()> {
        for part in clipping
            .clip
            .parts(bitmap, run.origin, split, &clipping.frame)
        {
            let mut image = Image {
                kind,
                x: part.position.x,
                y: part.position.y,
                colour: match part.colour {
                    Colour::First => colours[0],
                    Colour::Second => colours[1],
                },
                width: part.width,
                height: part.height,
                column: part.source_x,
                row: part.source_y,
                stride: bitmap.stride(),
                source: Source::Run(run.composite.clone(), which),
            };
            if let Some(mask) = mask {
                blend_vector_clip(&mut image, bitmap, mask, budget, work)?;
            }
            out.try_push(image).map_err(|e| e.error)?;
        }
        Ok(())
    };
    const WHOLE: i32 = 1_000_000;
    if with_shadows {
        for run in runs {
            if let Some(shadow) = &run.composite.shadow {
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
        if let Some(border) = &run.composite.border
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
        let Some(fill) = &run.composite.fill else {
            continue;
        };
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
    Ok(())
}

/// `blend_vector_clip` on one image: multiplied by the clip (or its inverse)
/// where they overlap, into new pixels; a regular clip leaves an empty image
/// where they do not.
fn blend_vector_clip(
    image: &mut Image,
    source: &Bitmap,
    mask: &ClipMask,
    budget: &Budget,
    work: &mut WorkBudget,
) -> Result<()> {
    let (ax, ay) = (image.x, image.y);
    let clip = &*mask.bitmap.bitmap;
    let origin = mask.bitmap.origin;
    let bx = origin.x.wrapping_add(clip.position.x);
    let by = origin.y.wrapping_add(clip.position.y);
    let left = ax.max(bx);
    let top = ay.max(by);
    let right = (i64::from(ax) + image.width as i64).min(i64::from(bx) + clip.width() as i64);
    let bottom = (i64::from(ay) + image.height as i64).min(i64::from(by) + clip.height() as i64);
    if right <= i64::from(left) || bottom <= i64::from(top) {
        if !mask.inverse {
            image.width = 0;
            image.height = 0;
            image.stride = 0;
        }
        return Ok(());
    }
    let w = (right - i64::from(left)) as usize;
    let h = (bottom - i64::from(top)) as usize;
    // Both passes are bounded before allocating: only the overlap is
    // multiplied, but an inverse clip keeps the rest of the image as well.
    charge(work, w.div_ceil(16).saturating_mul(h))?;
    let (x, y, width, height) = if mask.inverse {
        (image.column, image.row, image.width, image.height)
    } else {
        (
            image.column + (left - ax) as usize,
            image.row + (top - ay) as usize,
            w,
            h,
        )
    };
    let mut pixels = source.copy_region(
        x..x + width,
        y..y + height,
        budget,
        &Limits::default(),
        work,
    )?;
    if mask.inverse {
        pixels.multiply_inverse(clip, bx - ax, by - ay);
    } else {
        pixels.multiply(clip, bx - left, by - top);
        image.x = left;
        image.y = top;
        image.width = w;
        image.height = h;
        image.stride = pixels.stride();
    }
    image.source = Source::Own(Arc::new(pixels));
    (image.column, image.row) = (0, 0);
    Ok(())
}

/// `add_background`: the box of a `BorderStyle` 4 event, its positive shadow
/// added around it, in the back colour.
pub(super) fn background(
    area: Rect,
    frame: (i32, i32),
    colour: Rgba,
    budget: &Budget,
    work: &mut WorkBudget,
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
    let (width, height) = (w as usize, h as usize);
    let stride = Bitmap::stride_within(width, height, &Limits::default())?;
    charge(work, width.div_ceil(16))?;
    let mut row = BudgetVec::new(budget);
    row.try_resize(width, 0xff)?;
    Ok(Some(Image {
        kind: ImageKind::Background,
        x: left,
        y: top,
        colour,
        width,
        height,
        stride,
        column: 0,
        row: 0,
        source: Source::Solid(Arc::new(row)),
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
