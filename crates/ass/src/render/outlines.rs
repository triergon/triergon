//! Glyph and drawing outlines as libass's outline cache builds them
//! (`ass_outline_construct`): a glyph loaded at its drawn size without
//! hinting, with libass's synthetic styles and decoration lines, or a
//! drawing parsed; with its advance and the face's ascent and descent.

use std::collections::HashMap;
use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Error, Reservation, Result};
use triergon_font::{Font, GlyphId, Orientation};
use triergon_raster::{Outline, Shape, Vector, pixels_26_6};

use super::fonts::{FontId, RequestedFont};
use super::text::{Decorations, DrawingText};
use crate::{Bold, Drawing, Italic, Limits};

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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Glyph(GlyphKey),
    Drawing(Box<[u8]>),
}

/// Bytes charged per entry beyond the outline's own storage: the map slot,
/// key and value header.
const ENTRY_BYTES: usize = 192;

/// An outline kept, `None` for one that cannot be built, and when it was
/// last used.
#[derive(Debug)]
struct Entry {
    value: Option<Arc<OutlineValue>>,
    used: u64,
}

/// Outlines built so far, cut to the glyph limit of the renderer's
/// [`CacheLimits`](super::CacheLimits) at each frame as libass cuts its
/// outline cache: the least recently used that nothing else holds go first,
/// so the outlines the last frame's images were drawn from stay. Their
/// storage is charged to the renderer's budget.
#[derive(Debug)]
pub(super) struct Outlines {
    budget: Budget,
    entries: HashMap<Key, Entry>,
    clock: u64,
    charge: Reservation,
    scratch: triergon_font::Outline,
}

impl Outlines {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            budget: budget.clone(),
            entries: HashMap::new(),
            clock: 0,
            charge: Reservation::empty(budget),
            scratch: triergon_font::Outline::new(budget),
        }
    }

    /// Cuts the cache to `limit` outlines, oldest first, keeping those still
    /// held elsewhere (`ass_cache_cut`).
    pub(super) fn trim(&mut self, limit: usize) -> Result<()> {
        if self.entries.len() <= limit {
            return Ok(());
        }
        let mut free = BudgetVec::new(&self.budget);
        for (key, entry) in &self.entries {
            if entry
                .value
                .as_ref()
                .is_none_or(|v| Arc::strong_count(v) == 1)
            {
                free.try_push((entry.used, key.clone()))
                    .map_err(|e| e.error)?;
            }
        }
        free.as_mut_slice().sort_unstable_by_key(|(used, _)| *used);
        let excess = self.entries.len() - limit;
        for (_, key) in free.as_slice().iter().take(excess) {
            self.entries.remove(key);
        }
        self.charge
            .resize_to(self.entries.len().saturating_mul(ENTRY_BYTES))
    }

    /// The outline kept for `key`, marked as used now.
    fn get(&mut self, key: &Key) -> Option<&Entry> {
        self.clock += 1;
        let entry = self.entries.get_mut(key)?;
        entry.used = self.clock;
        Some(entry)
    }

    /// The outline of a glyph of `font`, `None` when its face cannot load it.
    pub(super) fn glyph(
        &mut self,
        glyph: GlyphKey,
        font: &RequestedFont,
    ) -> Result<Option<Arc<OutlineValue>>> {
        let key = Key::Glyph(glyph);
        if let Some(entry) = self.get(&key) {
            return Ok(entry.value.clone());
        }
        let source = &font.faces()[glyph.face].font;
        let size = f64::from_bits(glyph.size);
        let value = match self.load(source, font, size, glyph.glyph, glyph.decorations) {
            Ok(value) => Some(Arc::new(value)),
            Err(Error::Invalid(_) | Error::Overflow) => None,
            Err(error) => return Err(error),
        };
        self.insert(key, value.clone())?;
        Ok(value)
    }

    fn insert(&mut self, key: Key, value: Option<Arc<OutlineValue>>) -> Result<()> {
        self.clock += 1;
        let used = self.clock;
        self.entries.insert(key, Entry { value, used });
        self.charge
            .resize_to(self.entries.len().saturating_mul(ENTRY_BYTES))
    }

    /// The outline of a drawing, `None` when libass discards it.
    pub(super) fn drawing(
        &mut self,
        drawing: &DrawingText<'_>,
        limits: &Limits,
    ) -> Result<Option<Arc<OutlineValue>>> {
        let key = Key::Drawing(drawing.text.into());
        if let Some(entry) = self.get(&key) {
            return Ok(entry.value.clone());
        }
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
        self.insert(key, value.clone())?;
        Ok(value)
    }

    /// Loads a glyph as libass does with hinting off.
    fn load(
        &mut self,
        source: &Font,
        font: &RequestedFont,
        size: f64,
        glyph: GlyphId,
        decorations: Decorations,
    ) -> Result<OutlineValue> {
        if decorations.rotate {
            return Err(Error::Unsupported("the vertical advance of an @ font"));
        }
        let sized = source.at_size(pixels_26_6(size))?;
        let scratch = &mut self.scratch;
        let metrics = sized.outline_into(glyph, scratch)?;
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
