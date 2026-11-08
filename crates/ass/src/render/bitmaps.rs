//! The last stage: an event's placed glyphs drawn and combined run by run,
//! as libass's `render_and_combine_glyphs` does through the bitmap and
//! composite caches: each glyph's fill and border, then each run's fill,
//! border and shadow composited.

use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};
use triergon_raster::{
    Bitmap, EffectsKey, FrameScale, GlyphBitmaps, Matrix, Raster, Run, Shape, ShapedKey, Vector,
};

use super::layout::d6_to_double;
use super::outlines::OutlineValue;
use super::placement::{PlacedRun, Runs};
use crate::{Karaoke, Rgba};

/// One run of glyphs composited together (libass's `CombinedBitmapInfo`),
/// positioned in the frame.
#[derive(Debug)]
pub(super) struct Combined {
    /// The colours, faded.
    pub(super) colours: [Rgba; 4],
    pub(super) effect: Option<Karaoke>,
    /// For `\kf`, the frame column the fill gives way at.
    pub(super) effect_timing: i32,
    /// The frame position the bitmaps' [`position`](Bitmap::position)s
    /// count from.
    pub(super) origin: Vector,
    pub(super) fill: Option<Arc<Bitmap>>,
    pub(super) border: Option<Arc<Bitmap>>,
    pub(super) shadow: Option<Arc<Bitmap>>,
    /// What libass keys the composite by; equal keys give the same bitmaps.
    pub(super) key: Arc<RunKey>,
}

/// An outline as libass's caches identify it: by the value, shared while
/// anything holds it, as a cache entry is.
#[derive(Clone, Debug)]
struct OutlineId(Arc<OutlineValue>);

impl PartialEq for OutlineId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// One glyph of a run's composite key (libass's `BitmapRef`): its bitmaps,
/// with the outline they are drawn from where they depend on it, and their
/// origin in the run.
#[derive(Clone, Debug, PartialEq)]
struct GlyphRef {
    key: ShapedKey<OutlineId>,
    offset: Vector,
}

/// A run's composite key (libass's `CompositeHashKey`): its effects and every
/// glyph's bitmaps and place in the run.
#[derive(Debug)]
pub(super) struct RunKey {
    effects: EffectsKey,
    glyphs: BudgetVec<GlyphRef>,
}

impl PartialEq for RunKey {
    fn eq(&self, other: &Self) -> bool {
        self.effects == other.effects && self.glyphs.as_slice() == other.glyphs.as_slice()
    }
}

/// A glyph's bitmaps while its run is collected, with the outline they are
/// drawn from.
#[derive(Debug)]
struct Drawn {
    outline: Arc<OutlineValue>,
    bitmaps: GlyphBitmaps,
}

/// The renderer's drawing state: the rasterising context.
#[derive(Debug)]
pub(super) struct Painter {
    budget: Budget,
    raster: Raster,
}

impl Painter {
    pub(super) fn new(budget: &Budget, limits: triergon_raster::Limits) -> Self {
        Self {
            budget: budget.clone(),
            raster: Raster::new(budget, limits),
        }
    }

    /// Draws `shape` under a plain matrix as a bitmap of its own: a vector
    /// clip.
    pub(super) fn draw_shape(
        &mut self,
        shape: &Shape,
        matrix: &Matrix,
        work: &mut WorkBudget,
    ) -> Result<Option<GlyphBitmaps>> {
        self.raster.draw(shape, matrix, work)
    }

    /// Draws and combines an event's placed runs, in order.
    pub(super) fn draw(
        &mut self,
        runs: &Runs,
        frame: &FrameScale,
        out: &mut BudgetVec<Combined>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        out.clear();
        for (placed, glyphs) in runs.iter() {
            let mut run = Run::default();
            let mut drawn = BudgetVec::new(&self.budget);
            for glyph in glyphs {
                if let Some(bitmaps) = self.raster.render(&glyph.request(frame), &mut run, work)? {
                    drawn
                        .try_push(Drawn {
                            outline: Arc::clone(&glyph.outline),
                            bitmaps,
                        })
                        .map_err(|e| e.error)?;
                }
            }
            self.finish(placed, &run, &drawn, frame, out, work)?;
        }
        Ok(())
    }

    /// Composites a drawn run.
    fn finish(
        &mut self,
        placed: &PlacedRun,
        run: &Run,
        drawn: &[Drawn],
        frame: &FrameScale,
        out: &mut BudgetVec<Combined>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        if drawn.is_empty() {
            return Ok(());
        }
        let mut effect_timing = placed.effect_timing;
        if placed.effect == Some(Karaoke::Fill) {
            effect_timing = (d6_to_double(run.leftmost_x())
                + d6_to_double(effect_timing) * frame.pixel_aspect)
                .round() as i32;
        }
        let composite = self.raster.compose(
            &placed.effects,
            frame,
            drawn.iter().map(|d| &d.bitmaps),
            work,
        )?;
        let mut key = RunKey {
            effects: placed.effects.key(frame),
            glyphs: BudgetVec::new(&self.budget),
        };
        key.glyphs.try_reserve_exact(drawn.len())?;
        for d in drawn {
            let origin = d.bitmaps.origin();
            key.glyphs
                .try_push(GlyphRef {
                    key: d
                        .bitmaps
                        .key()
                        .with_shape(OutlineId(Arc::clone(&d.outline))),
                    offset: Vector::new(
                        origin.x.wrapping_sub(composite.origin.x),
                        origin.y.wrapping_sub(composite.origin.y),
                    ),
                })
                .map_err(|e| e.error)?;
        }
        out.try_push(Combined {
            colours: placed.colours,
            effect: placed.effect,
            effect_timing,
            origin: composite.origin,
            fill: composite.fill.map(Arc::new),
            border: composite.border.map(Arc::new),
            shadow: composite.shadow.map(Arc::new),
            key: Arc::new(key),
        })
        .map_err(|e| e.error)?;
        Ok(())
    }
}
