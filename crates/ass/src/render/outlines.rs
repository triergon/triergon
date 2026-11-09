//! Outlines as libass's outline cache builds them (`ass_outline_construct`):
//! a glyph loaded at its drawn size without hinting, with libass's
//! synthetic styles and decoration lines, or a drawing parsed, with its
//! advance and the face's ascent and descent; a glyph's border stroked; and
//! the unit square opaque boxes are drawn from.

use std::borrow::Borrow;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};
use triergon_font::{Font, GlyphId, Orientation};
use triergon_raster::{BorderStroke, Outline, Raster, Shape, Vector, pixels_26_6};

use super::cache::Cache;
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

/// An outline as libass's caches identify it: by the value, which a key
/// holds while it is kept, as libass's keys hold theirs.
#[derive(Clone, Debug)]
pub(super) struct OutlineId(pub(super) Arc<OutlineValue>);

impl PartialEq for OutlineId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for OutlineId {}

impl Hash for OutlineId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

/// A border outline (libass's `BorderHashKey`): a glyph's outline stroked.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BorderKey {
    glyph: OutlineId,
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

/// What finds an outline in the cache: a key kept or a key looked up.
trait Lookup {
    fn key(&self) -> KeyRef<'_>;
}

impl Lookup for OutlineKey {
    fn key(&self) -> KeyRef<'_> {
        match self {
            Self::Glyph(glyph) => KeyRef::Glyph(glyph),
            Self::Drawing(text) => KeyRef::Drawing(text),
            Self::Border(border) => KeyRef::Border(border),
            Self::UnitBox => KeyRef::UnitBox,
        }
    }
}

impl Lookup for KeyRef<'_> {
    fn key(&self) -> KeyRef<'_> {
        *self
    }
}

impl PartialEq for dyn Lookup + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for dyn Lookup + '_ {}

impl Hash for dyn Lookup + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl PartialEq for OutlineKey {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for OutlineKey {}

impl Hash for OutlineKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl<'a> Borrow<dyn Lookup + 'a> for OutlineKey {
    fn borrow(&self) -> &(dyn Lookup + 'a) {
        self
    }
}

/// Outlines built so far: glyphs, drawings, borders and the unit square,
/// each counted as 1 as libass counts its outline entries, and cut to the
/// outline limit of the renderer's [`CacheLimits`](super::CacheLimits) at
/// each frame as libass cuts its outline cache (see [`Cache`]). Their
/// storage is charged to the renderer's budget.
#[derive(Debug)]
pub(super) struct Outlines {
    budget: Budget,
    cache: Cache<OutlineKey, Option<Arc<OutlineValue>>>,
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
    pub(super) fn cut(&mut self, limit: usize) -> Result<()> {
        self.cache.cut(limit)
    }

    /// Keeps `value` for `key`, counted as 1.
    fn keep(
        &mut self,
        key: OutlineKey,
        value: Option<Arc<OutlineValue>>,
    ) -> Result<Option<Arc<OutlineValue>>> {
        self.cache.insert(key, value, 1)
    }

    /// The outline of a glyph of `font`, `None` when its face cannot load it.
    pub(super) fn glyph(
        &mut self,
        glyph: GlyphKey,
        font: &RequestedFont,
        work: &mut WorkBudget,
    ) -> Result<Option<Arc<OutlineValue>>> {
        if let Some(value) = self.cache.get(&KeyRef::Glyph(&glyph) as &dyn Lookup) {
            return Ok(value);
        }
        charge(work, OUTLINE_UNITS)?;
        let source = &font.faces()[glyph.face].font;
        let size = f64::from_bits(glyph.size);
        let value = match self.load(source, font, size, glyph.glyph, glyph.decorations, work) {
            Ok(value) => Some(Arc::new(value)),
            Err(Error::Invalid(_) | Error::Overflow) => None,
            Err(error) => return Err(error),
        };
        self.keep(OutlineKey::Glyph(glyph), value)
    }

    /// The border of `glyph` stroked by `stroke` with `raster`, `None` when
    /// it cannot be stroked.
    pub(super) fn border(
        &mut self,
        glyph: &Arc<OutlineValue>,
        stroke: BorderStroke,
        raster: &mut Raster,
        work: &mut WorkBudget,
    ) -> Result<Option<Arc<OutlineValue>>> {
        let key = BorderKey {
            glyph: OutlineId(Arc::clone(glyph)),
            stroke,
        };
        if let Some(value) = self.cache.get(&KeyRef::Border(&key) as &dyn Lookup) {
            return Ok(value);
        }
        let value = raster
            .stroke_border(&glyph.shape, &stroke, work)?
            .map(|shape| Arc::new(OutlineValue::of_shape(shape)));
        self.keep(OutlineKey::Border(key), value)
    }

    /// The unit square opaque boxes are drawn from.
    pub(super) fn unit_box(&mut self) -> Result<Arc<OutlineValue>> {
        if let Some(Some(value)) = self.cache.get(&KeyRef::UnitBox as &dyn Lookup) {
            return Ok(value);
        }
        let value = Arc::new(OutlineValue::of_shape(Shape::unit_box(&self.budget)?));
        self.keep(OutlineKey::UnitBox, Some(Arc::clone(&value)))?;
        Ok(value)
    }

    /// The outline of a drawing, `None` when libass discards it.
    pub(super) fn drawing(
        &mut self,
        drawing: &DrawingText<'_>,
        limits: &Limits,
        work: &mut WorkBudget,
    ) -> Result<Option<Arc<OutlineValue>>> {
        if let Some(value) = self
            .cache
            .get(&KeyRef::Drawing(drawing.text) as &dyn Lookup)
        {
            return Ok(value);
        }
        // The parser passes over the text once.
        charge(work, OUTLINE_UNITS.saturating_add(drawing.text.len()))?;
        let mut outline = Outline::new(&self.budget);
        let cbox = Drawing::parse(drawing.text, &mut outline, limits)?.map(|parsed| parsed.cbox);
        let value = cbox.map(|cbox| {
            Arc::new(OutlineValue {
                advance: cbox.x_max.wrapping_sub(cbox.x_min),
                asc: cbox.y_max.wrapping_sub(cbox.y_min),
                desc: 0,
                shape: Shape::from_outlines(outline, Outline::new(&self.budget)),
            })
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
        if decorations.rotate {
            return Err(Error::Unsupported("the vertical advance of an @ font"));
        }
        let sized = source.at_size(pixels_26_6(size))?;
        let scratch = &mut self.scratch;
        let metrics = sized.outline_into(glyph, scratch)?;
        // Synthesis, decoration and the conversion pass over its points.
        charge(work, scratch.points().len())?;
        sized.synthesize(scratch, source.synthesis(font.weight(), font.slant()))?;
        let advance = metrics.advance;
        let lines = [
            (decorations.underline, sized.underline()),
            (decorations.strike_out, sized.strikeout()),
        ]
        .map(|(on, band)| band.filter(|_| on && advance > 0));
        let mut outline = Outline::new(&self.budget);
        outline.push_glyph(scratch.points(), scratch.contour_ends())?;
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
