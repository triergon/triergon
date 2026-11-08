//! Glyph and drawing outlines as libass's outline cache builds them
//! (`ass_outline_construct`): a glyph loaded at its drawn size without
//! hinting, with libass's synthetic styles and decoration lines, or a
//! drawing parsed; with its advance and the face's ascent and descent.

use std::collections::HashMap;
use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Error, Reservation, Result, WorkBudget};
use triergon_font::{Font, GlyphId, Orientation};
use triergon_raster::{Outline, Shape, Vector, pixels_26_6};

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

/// Bytes charged per entry beyond the outline's own storage and a drawing's
/// text, which is charged by length: the map slot, key and value header.
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
    glyphs: HashMap<GlyphKey, Entry>,
    /// Drawings by their text, so a lookup borrows the text it is given.
    drawings: HashMap<Box<[u8]>, Entry>,
    /// The bytes of the drawings' texts.
    text_bytes: usize,
    clock: u64,
    charge: Reservation,
    scratch: triergon_font::Outline,
}

impl Outlines {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            budget: budget.clone(),
            glyphs: HashMap::new(),
            drawings: HashMap::new(),
            text_bytes: 0,
            clock: 0,
            charge: Reservation::empty(budget),
            scratch: triergon_font::Outline::new(budget),
        }
    }

    fn len(&self) -> usize {
        self.glyphs.len() + self.drawings.len()
    }

    fn charge_entries(&mut self) -> Result<()> {
        self.charge.resize_to(
            self.len()
                .saturating_mul(ENTRY_BYTES)
                .saturating_add(self.text_bytes),
        )
    }

    /// Cuts the cache to `limit` outlines, oldest first, keeping those still
    /// held elsewhere (`ass_cache_cut`).
    pub(super) fn trim(&mut self, limit: usize) -> Result<()> {
        let excess = self.len().saturating_sub(limit);
        if excess == 0 {
            return Ok(());
        }
        // Every use is stamped apart, so the stamp of the `excess`th oldest
        // free entry divides those that go from those that stay.
        let free = |entry: &Entry| {
            entry
                .value
                .as_ref()
                .is_none_or(|v| Arc::strong_count(v) == 1)
        };
        let mut stamps = BudgetVec::new(&self.budget);
        stamps.try_reserve_exact(self.len())?;
        for entry in self.glyphs.values().chain(self.drawings.values()) {
            if free(entry) {
                stamps.try_push(entry.used).map_err(|e| e.error)?;
            }
        }
        if stamps.is_empty() {
            return Ok(());
        }
        stamps.as_mut_slice().sort_unstable();
        let oldest = stamps.as_slice()[excess.min(stamps.len()) - 1];
        let stays = |entry: &Entry| entry.used > oldest || !free(entry);
        self.glyphs.retain(|_, entry| stays(entry));
        let mut freed = 0;
        self.drawings.retain(|text, entry| {
            let keep = stays(entry);
            if !keep {
                freed += text.len();
            }
            keep
        });
        self.text_bytes -= freed;
        self.charge_entries()
    }

    /// Marks `entry` as used now.
    fn touch(clock: &mut u64, entry: &mut Entry) -> Option<Arc<OutlineValue>> {
        *clock += 1;
        entry.used = *clock;
        entry.value.clone()
    }

    /// The outline of a glyph of `font`, `None` when its face cannot load it.
    pub(super) fn glyph(
        &mut self,
        glyph: GlyphKey,
        font: &RequestedFont,
        work: &mut WorkBudget,
    ) -> Result<Option<Arc<OutlineValue>>> {
        if let Some(entry) = self.glyphs.get_mut(&glyph) {
            return Ok(Self::touch(&mut self.clock, entry));
        }
        charge(work, OUTLINE_UNITS)?;
        let source = &font.faces()[glyph.face].font;
        let size = f64::from_bits(glyph.size);
        let value = match self.load(source, font, size, glyph.glyph, glyph.decorations, work) {
            Ok(value) => Some(Arc::new(value)),
            Err(Error::Invalid(_) | Error::Overflow) => None,
            Err(error) => return Err(error),
        };
        self.clock += 1;
        let entry = Entry {
            value: value.clone(),
            used: self.clock,
        };
        self.glyphs.insert(glyph, entry);
        self.charge_entries()?;
        Ok(value)
    }

    /// The outline of a drawing, `None` when libass discards it.
    pub(super) fn drawing(
        &mut self,
        drawing: &DrawingText<'_>,
        limits: &Limits,
        work: &mut WorkBudget,
    ) -> Result<Option<Arc<OutlineValue>>> {
        if let Some(entry) = self.drawings.get_mut(drawing.text) {
            return Ok(Self::touch(&mut self.clock, entry));
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
        self.clock += 1;
        let entry = Entry {
            value: value.clone(),
            used: self.clock,
        };
        self.text_bytes += drawing.text.len();
        self.drawings.insert(drawing.text.into(), entry);
        self.charge_entries()?;
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
