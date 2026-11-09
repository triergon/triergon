//! The last stage: an event's placed glyphs drawn and combined run by run,
//! as libass's `render_and_combine_glyphs` does through the bitmap and
//! composite caches: each glyph's fill and border, then each run's fill,
//! border and shadow composited.

use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};
use triergon_raster::{
    Bitmap, BitmapTransform, BorderStroke, EffectsKey, FrameScale, GlyphBitmaps, Matrix,
    PlacedBitmap, PlacedBorder, Quantized, Raster, Run, Shape, Vector,
};

use super::outlines::OutlineValue;
use super::placement::{GlyphPlacement, PlacedRun, Placer, Runs};
use super::units::d6_to_double;
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

/// What a bitmap is drawn from: the outline of libass's bitmap key.
#[derive(Clone, Debug, PartialEq)]
enum BitmapSource {
    /// A glyph's or drawing's outline.
    Outline(OutlineId),
    /// The border stroked around an outline.
    Border(OutlineId, BorderStroke),
    /// The opaque box, the same for every glyph.
    Box,
}

/// What identifies a bitmap (libass's `BitmapHashKey`): what it is drawn
/// from and the quantised transform it is drawn under.
#[derive(Clone, Debug, PartialEq)]
struct BitmapKey {
    source: BitmapSource,
    transform: BitmapTransform,
}

/// One glyph of a run's composite key (libass's `BitmapRef`): its fill and
/// border bitmaps, each `None` when it draws nothing, and their origins in
/// the run.
#[derive(Clone, Debug, PartialEq)]
struct GlyphRef {
    fill: Option<BitmapKey>,
    border: Option<BitmapKey>,
    origin: Vector,
    border_origin: Vector,
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

/// A glyph's bitmaps while its run is collected, with their keys.
#[derive(Debug)]
struct Drawn {
    bitmaps: GlyphBitmaps,
    fill: Option<BitmapKey>,
    border: Option<BitmapKey>,
}

/// A bitmap drawn for a glyph, with its key.
type Keyed = (BitmapKey, PlacedBitmap);

/// The renderer's drawing state: the rasterising context and the unit
/// square opaque boxes are drawn from.
#[derive(Debug)]
pub(super) struct Painter {
    budget: Budget,
    raster: Raster,
    unit_box: Shape,
}

impl Painter {
    pub(super) fn new(budget: &Budget, limits: triergon_raster::Limits) -> Result<Self> {
        Ok(Self {
            budget: budget.clone(),
            raster: Raster::new(budget, limits),
            unit_box: Shape::unit_box(budget)?,
        })
    }

    /// Draws `shape` under a plain matrix as a bitmap of its own: a vector
    /// clip. `None` when nothing is drawn.
    pub(super) fn draw_shape(
        &mut self,
        shape: &Shape,
        matrix: &Matrix,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap>> {
        let Some(at) = Quantized::standalone(shape, matrix) else {
            return Ok(None);
        };
        Ok(self
            .raster
            .draw(shape, &at.transform, work)?
            .map(|bitmap| placed(bitmap, at)))
    }

    /// Draws and combines an event's runs, in order, into `out`.
    pub(super) fn draw(
        &mut self,
        runs: &Runs,
        placer: &Placer<'_>,
        frame: &FrameScale,
        out: &mut BudgetVec<Combined>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        out.clear();
        for (placed, glyphs) in runs.iter() {
            // A run's glyph bitmaps live until it is composited.
            let mut run = Run::default();
            let mut drawn = BudgetVec::new(&self.budget);
            for glyph in glyphs.iter().filter_map(|g| placer.glyph(g)) {
                if let Some(glyph) = self.draw_glyph(&glyph, frame, &mut run, work)? {
                    drawn.try_push(glyph).map_err(|e| e.error)?;
                }
            }
            self.finish(placed, &run, &drawn, frame, out, work)?;
        }
        Ok(())
    }

    /// Places one glyph in `run` and draws its fill and border, as libass's
    /// `get_bitmap_glyph` does; `None` when it draws nothing.
    fn draw_glyph(
        &mut self,
        glyph: &GlyphPlacement<'_>,
        frame: &FrameScale,
        run: &mut Run,
        work: &mut WorkBudget,
    ) -> Result<Option<Drawn>> {
        let Some(placement) = run.place(&glyph.request(frame)) else {
            return Ok(None);
        };
        let shape = &glyph.outline.shape;
        let outline = || OutlineId(Arc::clone(glyph.outline));
        let fill_at = placement.fill();
        let fill = self
            .raster
            .draw(shape, &fill_at.transform, work)?
            .map(|bitmap| {
                let key = BitmapKey {
                    source: BitmapSource::Outline(outline()),
                    transform: fill_at.transform,
                };
                (key, placed(bitmap, fill_at))
            });
        let border: Option<Keyed> =
            match placement.border() {
                PlacedBorder::None => None,
                PlacedBorder::Fill => fill.clone(),
                PlacedBorder::Box(at) => self
                    .raster
                    .draw(&self.unit_box, &at.transform, work)?
                    .map(|bitmap| {
                        let key = BitmapKey {
                            source: BitmapSource::Box,
                            transform: at.transform,
                        };
                        (key, placed(bitmap, at))
                    }),
                PlacedBorder::Stroke(stroke) => {
                    let stroked = self.raster.stroke_border(shape, &stroke, work)?;
                    match stroked.and_then(|s| Some((placement.place_stroke(&s)?, s))) {
                        Some((at, stroked)) => self
                            .raster
                            .draw(&stroked, &at.transform, work)?
                            .map(|bitmap| {
                                let key = BitmapKey {
                                    source: BitmapSource::Border(outline(), stroke),
                                    transform: at.transform,
                                };
                                (key, placed(bitmap, at))
                            }),
                        None => None,
                    }
                }
            };
        let (fill_key, fill) = fill.unzip();
        let (border_key, border) = border.unzip();
        Ok(run.add(fill, border).map(|bitmaps| Drawn {
            bitmaps,
            fill: fill_key,
            border: border_key,
        }))
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
        let in_run = |at: Vector| {
            Vector::new(
                at.x.wrapping_sub(composite.origin.x),
                at.y.wrapping_sub(composite.origin.y),
            )
        };
        for d in drawn {
            key.glyphs
                .try_push(GlyphRef {
                    fill: d.fill.clone(),
                    border: d.border.clone(),
                    origin: in_run(d.bitmaps.origin()),
                    border_origin: in_run(d.bitmaps.border_origin()),
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

/// `bitmap` placed where `at` puts it.
fn placed(bitmap: Bitmap, at: Quantized) -> PlacedBitmap {
    PlacedBitmap {
        bitmap: Arc::new(bitmap),
        origin: at.position,
    }
}
