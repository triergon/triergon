//! Outlines as libass's outline cache builds them (`ass_outline_construct`):
//! a glyph loaded at its drawn size without hinting, with libass's
//! synthetic styles and decoration lines, or a drawing parsed, with its
//! advance and the face's ascent and descent; and a glyph's border
//! stroked.

use std::hash::{Hash, Hasher};
use std::ops::Deref;

use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};
use triergon_font::{Font, GlyphId, Orientation};
use triergon_raster::{BorderStroke, OUTLINE_MAX, Outline, Raster, Shape, Vector, pixels_26_6};

use super::cache::{Cache, CacheKey, Kept};
use super::fonts::{FontId, RequestedFont};
use super::text::{Decorations, DrawingText};
use crate::{Bold, Drawing, Italic, Limits, charge};

/// An outline and what layout needs of it.
#[derive(Debug)]
pub(super) struct OutlineValue {
    /// The glyph or drawing, 26.6, y down.
    pub shape: Shape,
    /// The advance, 26.6.
    pub advance: i32,
    /// The ascent and descent, 26.6.
    pub asc: i32,
    pub desc: i32,
}

/// A built outline the cache keeps. It is identified as libass's caches
/// identify an outline, by the value, which a key holds while it is kept,
/// as libass's keys hold theirs: two are equal when they are the same.
#[derive(Clone, Debug)]
pub(super) struct KeptOutline(Kept<Option<OutlineValue>>);

impl KeptOutline {
    /// The outline kept, `None` for one that cannot be built.
    fn built(kept: Kept<Option<OutlineValue>>) -> Option<Self> {
        kept.is_some().then_some(Self(kept))
    }
}

impl Deref for KeptOutline {
    type Target = OutlineValue;

    fn deref(&self) -> &OutlineValue {
        match &*self.0 {
            Some(value) => value,
            None => unreachable!("a kept outline is built"),
        }
    }
}

impl PartialEq for KeptOutline {
    fn eq(&self, other: &Self) -> bool {
        Kept::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for KeptOutline {}

impl Hash for KeptOutline {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Kept::as_ptr(&self.0).hash(state);
    }
}

/// A border outline (libass's `BorderHashKey`): a glyph's outline stroked.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BorderKey {
    glyph: KeptOutline,
    stroke: BorderStroke,
}

impl OutlineValue {
    /// A shape that is not laid out: a border or the unit square.
    fn of_shape(shape: Shape) -> Self {
        Self {
            shape,
            advance: 0,
            asc: 0,
            desc: 0,
        }
    }
}

/// A glyph outline: a glyph of a face of a requested font at a size, with
/// the style its outline is drawn with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct GlyphKey {
    pub(super) font: FontId,
    pub(super) face: usize,
    /// The size's bits.
    pub(super) size: u64,
    pub(super) glyph: GlyphId,
    pub(super) bold: Bold,
    pub(super) italic: Italic,
    pub(super) decorations: Decorations,
}

/// Work units charged per outline built beyond its points or text: sizing
/// the face and loading the glyph.
const OUTLINE_UNITS: usize = 64;

/// An outline's key in the cache (libass's `OutlineHashKey`).
#[derive(Debug)]
enum OutlineKey {
    Glyph(GlyphKey),
    /// A drawing by its text.
    Drawing(BudgetVec<u8>),
    Border(BorderKey),
    /// The unit square opaque boxes are drawn from.
    UnitBox,
}

/// An outline's key as it is looked up, a drawing's text borrowed from the
/// script.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum KeyRef<'a> {
    Glyph(&'a GlyphKey),
    Drawing(&'a [u8]),
    Border(&'a BorderKey),
    UnitBox,
}

impl CacheKey for OutlineKey {
    type View<'a> = KeyRef<'a>;

    fn view(&self) -> KeyRef<'_> {
        match self {
            Self::Glyph(glyph) => KeyRef::Glyph(glyph),
            Self::Drawing(text) => KeyRef::Drawing(text),
            Self::Border(border) => KeyRef::Border(border),
            Self::UnitBox => KeyRef::UnitBox,
        }
    }

    fn same(a: &KeyRef<'_>, b: &KeyRef<'_>) -> bool {
        a == b
    }
}

/// Outlines built so far: glyphs, drawings, borders and the unit square of
/// opaque boxes, each counted as 1 as libass counts its outline entries,
/// and cut to the outline limit of the renderer's [`CacheLimits`](super::CacheLimits) as
/// libass cuts its outline cache (see [`Cache`]). Their storage is charged
/// to the renderer's budget.
#[derive(Debug)]
pub(super) struct Outlines {
    budget: Budget,
    /// `None` for a key whose outline cannot be built, which libass also
    /// keeps.
    cache: Cache<OutlineKey, Option<OutlineValue>>,
    scratch: triergon_font::Outline,
}

impl Outlines {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            budget: budget.clone(),
            cache: Cache::new(budget),
            scratch: triergon_font::Outline::new(budget),
        }
    }

    /// Cuts the cache to `limit` outlines (`ass_cache_cut`).
    pub(super) fn cut(&mut self, limit: usize) {
        self.cache.cut(limit);
    }

    /// Cuts the cache to its limit again once the renderer has let go of
    /// outlines (see [`Cache::cut`]).
    pub(super) fn settle(&mut self) {
        self.cache.settle();
    }

    /// Keeps `value` for `key`, counted as 1; `None` for an outline that
    /// cannot be built.
    fn keep(
        &mut self,
        key: OutlineKey,
        value: Option<OutlineValue>,
    ) -> Result<Option<KeptOutline>> {
        self.cache.insert(key, value, 1).map(KeptOutline::built)
    }

    /// The outline of a glyph of `font`, `None` when its face cannot load it.
    pub(super) fn glyph(
        &mut self,
        glyph: GlyphKey,
        font: &RequestedFont,
        work: &mut WorkBudget,
    ) -> Result<Option<KeptOutline>> {
        if let Some(found) = self.cache.get(KeyRef::Glyph(&glyph)) {
            return Ok(KeptOutline::built(found));
        }
        charge(work, OUTLINE_UNITS)?;
        let source = &font.faces()[glyph.face].font;
        let size = f64::from_bits(glyph.size);
        let value = match self.load(source, font, size, glyph.glyph, glyph.decorations, work) {
            Ok(value) => Some(value),
            Err(Error::Invalid(_) | Error::Overflow) => None,
            Err(error) => return Err(error),
        };
        self.keep(OutlineKey::Glyph(glyph), value)
    }

    /// The border of `glyph` stroked by `stroke` with `raster`, `None` when
    /// it cannot be stroked.
    pub(super) fn border(
        &mut self,
        glyph: &KeptOutline,
        stroke: BorderStroke,
        raster: &mut Raster,
        work: &mut WorkBudget,
    ) -> Result<Option<KeptOutline>> {
        let key = BorderKey {
            glyph: glyph.clone(),
            stroke,
        };
        if let Some(found) = self.cache.get(KeyRef::Border(&key)) {
            return Ok(KeptOutline::built(found));
        }
        let value = match raster.stroke_border(&glyph.shape, &stroke, work)? {
            Some(mut shape) => {
                // The stroker grows its output as it goes; a kept border
                // keeps only what it holds.
                shape.shrink_to_fit()?;
                Some(OutlineValue::of_shape(shape))
            }
            None => None,
        };
        self.keep(OutlineKey::Border(key), value)
    }

    /// The unit square opaque boxes are drawn from, which libass keeps as
    /// an outline (`ass_render.c:1294-1309`, `1423`).
    pub(super) fn unit_box(&mut self) -> Result<KeptOutline> {
        if let Some(found) = self.cache.get(KeyRef::UnitBox).and_then(KeptOutline::built) {
            return Ok(found);
        }
        let value = OutlineValue::of_shape(Shape::unit_box(&self.budget)?);
        let kept = self.cache.insert(OutlineKey::UnitBox, Some(value), 1)?;
        Ok(KeptOutline(kept))
    }

    /// The outline of a drawing, `None` when libass discards it.
    pub(super) fn drawing(
        &mut self,
        drawing: &DrawingText<'_>,
        limits: &Limits,
        work: &mut WorkBudget,
    ) -> Result<Option<KeptOutline>> {
        if let Some(found) = self.cache.get(KeyRef::Drawing(drawing.text)) {
            return Ok(KeptOutline::built(found));
        }
        // The parser passes over the text once.
        charge(work, OUTLINE_UNITS.saturating_add(drawing.text.len()))?;
        let mut outline = Outline::new(&self.budget);
        let cbox = Drawing::parse(drawing.text, &mut outline, limits)?.map(|parsed| parsed.cbox);
        let value = cbox.map(|cbox| OutlineValue {
            advance: cbox.x_max.wrapping_sub(cbox.x_min),
            asc: cbox.y_max.wrapping_sub(cbox.y_min),
            desc: 0,
            shape: Shape::from_outlines(outline, Outline::new(&self.budget)),
        });
        let mut text = BudgetVec::new(&self.budget);
        text.try_extend_from_slice(drawing.text)?;
        self.keep(OutlineKey::Drawing(text), value)
    }

    /// Loads a glyph as libass does with hinting off.
    fn load(
        &mut self,
        source: &Font,
        font: &RequestedFont,
        size: f64,
        glyph: GlyphId,
        decorations: Decorations,
        work: &mut WorkBudget,
    ) -> Result<OutlineValue> {
        let sized = source.at_size(pixels_26_6(size))?;
        let scratch = &mut self.scratch;
        let metrics = sized.outline_into(glyph, scratch)?;
        // Synthesis, decoration and the conversion pass over its points.
        charge(work, scratch.points().len())?;
        sized.synthesize(scratch, source.synthesis(font.weight(), font.slant()))?;
        // A glyph an `@` font draws rotated advances by its linear vertical
        // advance (`d16_to_d6`, in `int`).
        let advance = if decorations.rotate {
            (metrics.linear_vertical_advance as i32).wrapping_add(512) >> 10
        } else {
            metrics.advance
        };
        let lines = [
            (decorations.underline, sized.underline()),
            (decorations.strike_out, sized.strikeout()),
        ]
        .map(|(on, band)| band.filter(|_| on && advance > 0));
        let mut outline = Outline::new(&self.budget);
        outline.push_glyph(scratch.points(), scratch.contour_ends())?;
        // Turned about the vertical advance and the typographic descender,
        // unless there is nothing to turn: libass returns an empty outline
        // before it checks the range.
        let empty = scratch.points().is_empty() && lines.iter().all(Option::is_none);
        if decorations.rotate && !empty {
            let y_scale = sized.metrics().y_scale;
            let desc = source.os2().map_or(0, |os2| {
                (i64::from(os2.typo_descender) * y_scale + 0x8000) >> 16
            });
            let dv = i64::from(metrics.vertical_advance) + desc;
            let limit = 2 * i64::from(OUTLINE_MAX);
            if desc.abs() > limit || dv.abs() > limit {
                return Err(Error::Invalid("rotated outline out of range"));
            }
            outline.rotate_90(Vector::new(dv as i32, -desc as i32))?;
        }
        // The lines run the way the glyph's contours do, so they add to its
        // fill; the orientation is the source outline's, as FreeType finds it.
        if lines.iter().any(Option::is_some) {
            let truetype = scratch.orientation() == Orientation::TrueType;
            for band in lines.into_iter().flatten() {
                // libass works y down: the band's top and bottom edges.
                let (top, bottom) = (band.y_max.wrapping_neg(), band.y_min.wrapping_neg());
                let (from, to) = if truetype {
                    (top, bottom)
                } else {
                    (bottom, top)
                };
                outline.push_rect(Vector::new(0, from), Vector::new(advance, to))?;
            }
        }
        let (asc, desc) = sized.ascent_descent();
        Ok(OutlineValue {
            shape: Shape::from_outlines(outline, Outline::new(&self.budget)),
            advance,
            asc,
            desc,
        })
    }
}
