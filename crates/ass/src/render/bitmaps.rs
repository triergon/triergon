//! The last stage: an event's placed glyphs drawn and combined run by run,
//! as libass's `render_and_combine_glyphs` does, through a bitmap cache and
//! a composite cache: each glyph's fill and border, then each run's fill,
//! border and shadow composited.

use std::borrow::Borrow;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};
use triergon_raster::{
    Bitmap, BitmapTransform, BorderStroke, EffectsKey, FrameScale, GlyphBitmaps, Matrix,
    PlacedBitmap, PlacedBorder, Quantized, Raster, Run, Shape, Vector,
};

use super::CacheLimits;
use super::cache::{Cache, sizes};
use super::outlines::{OutlineId, OutlineValue, Outlines};
use super::placement::{GlyphPlacement, PlacedRun, Placer, Runs};
use super::units::d6_to_double;
use crate::{Karaoke, Rgba, charge};

/// The work of looking up a bitmap, and of looking up a composite per
/// glyph of its key: a hash and a comparison of a few words.
const LOOKUP_UNITS: usize = 1;

/// A run composited (libass's `CompositeHashValue`): its fill, border and
/// shadow, placed against the run's origin. Images show its bitmaps and
/// are told apart by it, as libass's are by the composite's buffers.
#[derive(Debug)]
pub(super) struct Composited {
    pub(super) fill: Option<Arc<Bitmap>>,
    pub(super) border: Option<Arc<Bitmap>>,
    pub(super) shadow: Option<Arc<Bitmap>>,
}

/// One run of glyphs composited together (libass's `CombinedBitmapInfo`),
/// positioned in the frame.
#[derive(Debug)]
pub(super) struct Combined {
    /// The colours, faded.
    pub(super) colours: [Rgba; 4],
    pub(super) effect: Option<Karaoke>,
    /// For `\kf`, the frame column the fill gives way at.
    pub(super) effect_timing: i32,
    /// The frame position the composite's bitmaps'
    /// [`position`](Bitmap::position)s count from: the least origin of
    /// the run's glyphs.
    pub(super) origin: Vector,
    pub(super) composite: Arc<Composited>,
}

/// What a bitmap is drawn from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum BitmapSource {
    /// A glyph's or drawing's outline.
    Outline(OutlineId),
    /// The border stroked around a glyph's outline: libass's border
    /// outline, which is the same outline for the same glyph and stroke.
    Border(OutlineId, BorderStroke),
    /// The unit square of opaque boxes.
    UnitBox,
}

/// What identifies a bitmap (libass's `BitmapHashKey`): what it is drawn
/// from and the quantised transform it is drawn under. Equal keys draw
/// equal bitmaps.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BitmapKey {
    source: BitmapSource,
    transform: BitmapTransform,
}

/// One glyph of a run's composite key (libass's `BitmapRef`): the keys of
/// its fill and border bitmaps, each `None` when it draws nothing, and
/// their origins in the run. libass's key holds the bitmaps themselves;
/// their keys find the same composite, since an equal key draws an equal
/// bitmap and the key holds the outlines it names, and they let the
/// bitmaps leave the cache while the composite stays.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct GlyphRef {
    fill: Option<BitmapKey>,
    border: Option<BitmapKey>,
    origin: Vector,
    border_origin: Vector,
}

/// A glyph's bitmaps while its run is drawn, with their keys.
#[derive(Debug)]
struct Drawn {
    bitmaps: GlyphBitmaps,
    fill: Option<BitmapKey>,
    border: Option<BitmapKey>,
}

/// A bitmap drawn for a glyph, with its key.
type Keyed = (BitmapKey, PlacedBitmap);

/// A run's composite key (libass's `CompositeHashKey`): its effects and
/// every glyph's bitmaps and place in the run.
#[derive(Clone, Debug)]
struct RunKey {
    effects: EffectsKey,
    glyphs: Arc<BudgetVec<GlyphRef>>,
}

/// A run's composite key while it is made and looked up, in storage the
/// painter reuses.
#[derive(Debug)]
struct RunScratch {
    effects: EffectsKey,
    glyphs: BudgetVec<GlyphRef>,
}

/// What finds a composite: a key kept or the key being looked up.
trait RunLookup {
    fn effects(&self) -> EffectsKey;
    fn glyphs(&self) -> &[GlyphRef];
}

impl RunLookup for RunKey {
    fn effects(&self) -> EffectsKey {
        self.effects
    }

    fn glyphs(&self) -> &[GlyphRef] {
        &self.glyphs
    }
}

impl RunLookup for RunScratch {
    fn effects(&self) -> EffectsKey {
        self.effects
    }

    fn glyphs(&self) -> &[GlyphRef] {
        &self.glyphs
    }
}

impl PartialEq for dyn RunLookup + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.effects() == other.effects() && self.glyphs() == other.glyphs()
    }
}

impl Eq for dyn RunLookup + '_ {}

impl Hash for dyn RunLookup + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.effects().hash(state);
        self.glyphs().hash(state);
    }
}

impl PartialEq for RunKey {
    fn eq(&self, other: &Self) -> bool {
        (self as &dyn RunLookup) == (other as &dyn RunLookup)
    }
}

impl Eq for RunKey {}

impl Hash for RunKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self as &dyn RunLookup).hash(state);
    }
}

impl<'a> Borrow<dyn RunLookup + 'a> for RunKey {
    fn borrow(&self) -> &(dyn RunLookup + 'a) {
        self
    }
}

/// The renderer's drawing state: the rasterising context, the bitmaps and
/// composites kept between frames, and the run being drawn.
#[derive(Debug)]
pub(super) struct Painter {
    budget: Budget,
    raster: Raster,
    bitmaps: Cache<BitmapKey, Option<Arc<Bitmap>>>,
    composites: Cache<RunKey, Arc<Composited>>,
    /// The bitmaps of the run being drawn.
    drawn: BudgetVec<Drawn>,
    /// Its composite key, made in this storage for every run; it moves into
    /// the cache with a new composite.
    key: RunScratch,
}

impl Painter {
    pub(super) fn new(budget: &Budget, limits: triergon_raster::Limits) -> Self {
        Self {
            budget: budget.clone(),
            raster: Raster::new(budget, limits),
            bitmaps: Cache::new(budget),
            composites: Cache::new(budget),
            drawn: BudgetVec::new(budget),
            key: RunScratch {
                effects: EffectsKey::default(),
                glyphs: BudgetVec::new(budget),
            },
        }
    }

    /// Cuts the composites and then the bitmaps to their limits, as
    /// libass's `check_cache_limits` does when a frame starts
    /// (`ass_render.c:3021-3026`); nothing but the last frame's images may
    /// hold them then.
    pub(super) fn cut(&mut self, limits: &CacheLimits) -> Result<()> {
        self.composites.cut(limits.composite_bytes)?;
        self.bitmaps.cut(limits.bitmap_bytes)
    }

    /// Cuts both caches to their limits again once an event's images are
    /// made: its composites and its vector clip are held by what shows them
    /// or let go (see [`Cache::settle`]).
    pub(super) fn settle(&mut self) -> Result<()> {
        self.composites.settle()?;
        self.bitmaps.settle()
    }

    /// The bitmap of `outline` under a plain matrix, placed on its own: a
    /// vector clip, which libass draws through its bitmap cache too
    /// (`blend_vector_clip`). `None` when nothing is drawn.
    pub(super) fn draw_shape(
        &mut self,
        outline: &Arc<OutlineValue>,
        matrix: &Matrix,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap>> {
        let Some(at) = Quantized::standalone(&outline.shape, matrix) else {
            return Ok(None);
        };
        let source = BitmapSource::Outline(OutlineId(Arc::clone(outline)));
        Ok(self
            .bitmap(source, &outline.shape, at, work)?
            .map(|(_, bitmap)| bitmap))
    }

    /// Draws and combines an event's runs, in order, into `out`.
    pub(super) fn draw(
        &mut self,
        runs: &Runs,
        placer: &Placer<'_>,
        frame: &FrameScale,
        outlines: &mut Outlines,
        out: &mut BudgetVec<Combined>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        out.clear();
        let drawn = self.draw_runs(runs, placer, frame, outlines, out, work);
        // A run's glyph bitmaps are let go once it is composited, and the
        // key's when it has been looked up.
        self.drawn.clear();
        self.key.glyphs.clear();
        drawn?;
        super::fit(&mut self.drawn)?;
        super::fit(&mut self.key.glyphs)
    }

    fn draw_runs(
        &mut self,
        runs: &Runs,
        placer: &Placer<'_>,
        frame: &FrameScale,
        outlines: &mut Outlines,
        out: &mut BudgetVec<Combined>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        for (placed, glyphs) in runs.iter() {
            self.drawn.clear();
            let mut run = Run::default();
            for glyph in glyphs.iter().filter_map(|g| placer.glyph(g)) {
                if let Some(drawn) = self.draw_glyph(&glyph, frame, outlines, &mut run, work)? {
                    self.drawn.try_push(drawn).map_err(|e| e.error)?;
                }
            }
            self.finish(placed, &run, frame, out, work)?;
            // The run's glyph bitmaps are let go once it is composited.
            self.drawn.clear();
            self.bitmaps.settle()?;
        }
        Ok(())
    }

    /// Places one glyph in `run` and draws its fill and border, as libass's
    /// `get_bitmap_glyph` does; `None` when it draws nothing.
    fn draw_glyph(
        &mut self,
        glyph: &GlyphPlacement<'_>,
        frame: &FrameScale,
        outlines: &mut Outlines,
        run: &mut Run,
        work: &mut WorkBudget,
    ) -> Result<Option<Drawn>> {
        let Some(placement) = run.place(&glyph.request(frame)) else {
            return Ok(None);
        };
        let outline = || OutlineId(Arc::clone(glyph.outline));
        let shape = &glyph.outline.shape;
        let fill = self.bitmap(
            BitmapSource::Outline(outline()),
            shape,
            placement.fill(),
            work,
        )?;
        let border: Option<Keyed> = match placement.border() {
            PlacedBorder::None => None,
            PlacedBorder::Fill => fill.clone(),
            PlacedBorder::Box(at) => {
                let unit_box = outlines.unit_box()?;
                self.bitmap(BitmapSource::UnitBox, &unit_box.shape, at, work)?
            }
            PlacedBorder::Stroke(stroke) => {
                match outlines.border(glyph.outline, stroke, &mut self.raster, work)? {
                    Some(border) => match placement.place_stroke(&border.shape) {
                        Some(at) => {
                            let source = BitmapSource::Border(outline(), stroke);
                            self.bitmap(source, &border.shape, at, work)?
                        }
                        None => None,
                    },
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

    /// The bitmap of `shape`, drawn from `source`, placed at `at`, with
    /// its key: from the cache, or drawn and kept. `None` when it draws
    /// nothing, which is kept too.
    fn bitmap(
        &mut self,
        source: BitmapSource,
        shape: &Shape,
        at: Quantized,
        work: &mut WorkBudget,
    ) -> Result<Option<Keyed>> {
        charge(work, LOOKUP_UNITS)?;
        let key = BitmapKey {
            source,
            transform: at.transform,
        };
        let bitmap = if let Some(bitmap) = self.bitmaps.get(&key)? {
            bitmap
        } else {
            let bitmap = self.raster.draw(shape, &at.transform, work)?.map(Arc::new);
            let [first, second] = shape.outlines();
            let size = sizes::bitmap(
                bitmap.as_deref().map_or(0, pixels),
                first.points().len() + second.points().len(),
                first.segments().len() + second.segments().len(),
            );
            self.bitmaps.insert(key.clone(), bitmap, size)?
        };
        Ok(bitmap.map(|bitmap| {
            let placed = PlacedBitmap {
                bitmap,
                origin: at.position,
            };
            (key, placed)
        }))
    }

    /// Composites the run drawn, from the cache or anew.
    fn finish(
        &mut self,
        placed: &PlacedRun,
        run: &Run,
        frame: &FrameScale,
        out: &mut BudgetVec<Combined>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let drawn = self.drawn.as_slice();
        let Some(origin) = drawn
            .iter()
            .map(|d| d.bitmaps.origin())
            .reduce(|a, b| Vector::new(a.x.min(b.x), a.y.min(b.y)))
        else {
            return Ok(());
        };
        let mut effect_timing = placed.effect_timing;
        if placed.effect == Some(Karaoke::Fill) {
            effect_timing = (d6_to_double(run.leftmost_x())
                + d6_to_double(effect_timing) * frame.pixel_aspect)
                .round() as i32;
        }
        charge(work, LOOKUP_UNITS.saturating_mul(drawn.len()))?;
        let in_run =
            |at: Vector| Vector::new(at.x.wrapping_sub(origin.x), at.y.wrapping_sub(origin.y));
        let key = &mut self.key;
        key.effects = placed.effects.key(frame);
        key.glyphs.clear();
        key.glyphs.try_reserve_exact(drawn.len())?;
        for glyph in drawn {
            key.glyphs
                .try_push(GlyphRef {
                    fill: glyph.fill.clone(),
                    border: glyph.border.clone(),
                    origin: in_run(glyph.bitmaps.origin()),
                    border_origin: in_run(glyph.bitmaps.border_origin()),
                })
                .map_err(|e| e.error)?;
        }
        let composite = if let Some(composite) = self.composites.get(&*key as &dyn RunLookup)? {
            composite
        } else {
            let glyphs = drawn.iter().map(|d| &d.bitmaps);
            let composite = self.raster.compose(&placed.effects, frame, glyphs, work)?;
            let parts = [composite.fill, composite.border, composite.shadow];
            let size = sizes::composite(drawn.len(), parts.iter().flatten().map(pixels).sum());
            let [fill, border, shadow] = parts.map(|part| part.map(Arc::new));
            let glyphs = std::mem::replace(&mut self.key.glyphs, BudgetVec::new(&self.budget));
            let key = RunKey {
                effects: self.key.effects,
                glyphs: Arc::new(glyphs),
            };
            let composite = Arc::new(Composited {
                fill,
                border,
                shadow,
            });
            self.composites.insert(key, composite, size)?
        };
        out.try_push(Combined {
            colours: placed.colours,
            effect: placed.effect,
            effect_timing,
            origin,
            composite,
        })
        .map_err(|e| e.error)?;
        Ok(())
    }
}

/// The bytes libass counts for a bitmap's pixels: its stride times its
/// height (`bitmap_size`).
fn pixels(bitmap: &Bitmap) -> usize {
    bitmap.stride() * bitmap.height()
}
