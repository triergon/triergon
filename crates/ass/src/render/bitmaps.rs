//! The last stage: an event's placed glyphs drawn and combined run by run,
//! as libass's `render_and_combine_glyphs` does, through a bitmap cache and
//! a composite cache: each glyph's fill and border, then each run's fill,
//! border and shadow composited.

use std::ops::Deref;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};
use triergon_raster::{
    Bitmap, BitmapTransform, BorderStroke, EffectsKey, FrameScale, GlyphBitmaps, GlyphPlacement,
    GlyphSource, Limits, Matrix, PlacedBitmap, Quantized, Raster, Run, Shape, Vector,
};

use super::CacheLimits;
use super::cache::{Cache, CacheKey, Kept, Lookup, sizes};
use super::outlines::{OutlineId, OutlineValue, Outlines};
use super::placement::{GlyphDrawing, PlacedRun, Placer, Runs};
use super::units::d6_to_double;
use crate::{Karaoke, Rgba, charge};

/// The work of a lookup: of a bitmap, and of a composite per glyph of its
/// key; a hash and a comparison of a few words. A miss charges the drawing
/// besides.
const LOOKUP_UNITS: usize = 1;

/// A run composited (libass's `CompositeHashValue`): its fill, border and
/// shadow, placed against the run's origin. Images show its bitmaps and are
/// told apart by it, as libass's are by the composite's buffers.
#[derive(Debug)]
pub(super) struct Composited {
    pub(super) fill: Option<Bitmap>,
    pub(super) border: Option<Bitmap>,
    pub(super) shadow: Option<Bitmap>,
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
    pub(super) composite: Kept<Composited>,
}

/// What a bitmap is drawn from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum BitmapSource {
    /// A glyph's or drawing's outline, or the unit square of opaque boxes.
    Outline(OutlineId),
    /// The border stroked around a glyph's outline by a stroke: libass's
    /// border outline, which is one outline for one glyph and stroke.
    Border(OutlineId, BorderStroke),
}

/// What identifies a bitmap (libass's `BitmapHashKey`): what it is drawn
/// from and the quantised transform it is drawn under. Equal keys draw
/// equal bitmaps.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BitmapKey {
    source: BitmapSource,
    transform: BitmapTransform,
}

impl CacheKey for BitmapKey {
    type View<'a> = &'a Self;

    fn view(&self) -> &Self {
        self
    }

    fn same(a: &&Self, b: &&Self) -> bool {
        a == b
    }
}

/// A bitmap kept (libass's `Bitmap`), empty when it draws nothing, with
/// the stroked border it was drawn from, which libass's bitmap key holds as
/// long as the bitmap is kept.
#[derive(Debug)]
struct KeptBitmap {
    bitmap: Bitmap,
    _drawn_from: Option<Kept<OutlineValue>>,
}

/// A bitmap the bitmap cache keeps, which stays in the cache, counted, as
/// long as a run being drawn or a vector clip holds it.
#[derive(Clone, Debug)]
pub(super) struct SharedBitmap(Kept<KeptBitmap>);

impl Deref for SharedBitmap {
    type Target = Bitmap;

    fn deref(&self) -> &Bitmap {
        &self.0.bitmap
    }
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

/// A run's composite key (libass's `CompositeHashKey`): its effects and
/// every glyph's bitmaps and place in the run.
#[derive(Debug)]
struct RunKey {
    effects: EffectsKey,
    glyphs: BudgetVec<GlyphRef>,
}

/// A run's composite key as it is looked up, its glyphs in the painter's
/// storage.
#[derive(Debug, PartialEq, Eq, Hash)]
struct RunView<'a> {
    effects: EffectsKey,
    glyphs: &'a [GlyphRef],
}

impl RunKey {
    /// The key of a run with `effects`, its glyphs taken from the storage
    /// they were made in, which is left empty for the next run, and fitted
    /// to them: a key keeps no more than it holds, as libass's does.
    fn taking(effects: EffectsKey, glyphs: &mut BudgetVec<GlyphRef>) -> Result<Self> {
        let budget = glyphs.budget().clone();
        let mut glyphs = std::mem::replace(glyphs, BudgetVec::new(&budget));
        glyphs.try_shrink_to(glyphs.len())?;
        Ok(Self { effects, glyphs })
    }
}

impl CacheKey for RunKey {
    type View<'a> = RunView<'a>;

    fn view(&self) -> RunView<'_> {
        RunView {
            effects: self.effects,
            glyphs: &self.glyphs,
        }
    }

    fn same(a: &RunView<'_>, b: &RunView<'_>) -> bool {
        a.effects == b.effects && a.glyphs == b.glyphs
    }
}

#[cfg(test)]
impl RunKey {
    fn view_of(effects: EffectsKey, glyphs: &[GlyphRef]) -> RunView<'_> {
        RunView { effects, glyphs }
    }
}

/// A glyph's bitmaps while its run is drawn, held as libass's run holds
/// them, with their keys.
#[derive(Debug)]
struct Drawn {
    bitmaps: GlyphBitmaps<SharedBitmap>,
    fill: Option<BitmapKey>,
    border: Option<BitmapKey>,
}

/// The renderer's drawing state: the rasterising context, the bitmaps and
/// composites kept between frames, and the run being drawn.
#[derive(Debug)]
pub(super) struct Painter {
    raster: Raster,
    bitmaps: Bitmaps,
    composites: Cache<RunKey, Composited>,
    /// The bitmaps of the run being drawn.
    drawn: BudgetVec<Drawn>,
    /// Its composite key's glyphs, made in this storage for every run; they
    /// move into the cache with a new composite.
    glyphs: BudgetVec<GlyphRef>,
}

impl Painter {
    pub(super) fn new(budget: &Budget, limits: Limits) -> Self {
        Self {
            raster: Raster::new(budget, limits),
            bitmaps: Bitmaps {
                budget: budget.clone(),
                cache: Cache::new(budget),
            },
            composites: Cache::new(budget),
            drawn: BudgetVec::new(budget),
            glyphs: BudgetVec::new(budget),
        }
    }

    /// Cuts the composites and then the bitmaps to their limits, as
    /// libass's `check_cache_limits` does when a frame starts
    /// (`ass_render.c:3021-3026`); nothing but the last frame's images may
    /// hold them then.
    pub(super) fn cut(&mut self, limits: &CacheLimits) {
        self.composites.cut(limits.composite_bytes);
        self.bitmaps.cache.cut(limits.bitmap_bytes);
    }

    /// Cuts both caches to their limits again once an event's images are
    /// made: its composites and its vector clip are held by what shows them
    /// or let go (see [`Cache::cut`]).
    pub(super) fn settle(&mut self) {
        self.composites.settle();
        self.bitmaps.cache.settle();
    }

    /// The bitmap of `outline` under a plain matrix, placed on its own: a
    /// vector clip, which libass draws through its bitmap cache too
    /// (`blend_vector_clip`), and holds while it clips. `None` when the
    /// matrix cannot be quantised; a drawing that draws nothing gives a
    /// bitmap without pixels.
    pub(super) fn draw_shape(
        &mut self,
        outline: &Kept<OutlineValue>,
        matrix: &Matrix,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap<SharedBitmap>>> {
        let Some(at) = Quantized::standalone(&outline.shape, matrix) else {
            return Ok(None);
        };
        let key = BitmapKey {
            source: BitmapSource::Outline(OutlineId(outline.clone())),
            transform: at.transform,
        };
        let raster = &mut self.raster;
        self.bitmaps
            .get(raster, key, &outline.shape, None, at, work)
            .map(Some)
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
        self.glyphs.clear();
        drawn?;
        super::fit(&mut self.drawn)?;
        super::fit(&mut self.glyphs)
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
            self.bitmaps.cache.settle();
        }
        Ok(())
    }

    /// Places one glyph in `run` and draws its fill and border, as libass's
    /// `get_bitmap_glyph` does, through the bitmap cache; `None` when it
    /// draws nothing.
    fn draw_glyph(
        &mut self,
        glyph: &GlyphDrawing<'_>,
        frame: &FrameScale,
        outlines: &mut Outlines,
        run: &mut Run,
        work: &mut WorkBudget,
    ) -> Result<Option<Drawn>> {
        let mut source = CachedBitmaps {
            bitmaps: &mut self.bitmaps,
            outlines,
            glyph: glyph.outline,
            fill: None,
            border: None,
        };
        let request = glyph.request(frame);
        let Some(bitmaps) = self.raster.render_with(&request, run, &mut source, work)? else {
            return Ok(None);
        };
        // A border that rounds to nothing is the fill's own bitmap.
        let border = match (bitmaps.fill(), bitmaps.border()) {
            (Some(fill), Some(border)) if Kept::ptr_eq(&fill.bitmap.0, &border.bitmap.0) => {
                source.fill.clone()
            }
            _ => source.border,
        };
        Ok(Some(Drawn {
            bitmaps,
            fill: source.fill,
            border,
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
        let glyphs = &mut self.glyphs;
        glyphs.clear();
        glyphs.try_reserve_exact(drawn.len())?;
        for glyph in drawn {
            glyphs
                .try_push(GlyphRef {
                    fill: glyph.fill.clone(),
                    border: glyph.border.clone(),
                    origin: in_run(glyph.bitmaps.origin()),
                    border_origin: in_run(glyph.bitmaps.border_origin()),
                })
                .map_err(|e| e.error)?;
        }
        let effects = placed.effects.key(frame);
        let view = RunView {
            effects,
            glyphs: &self.glyphs,
        };
        let composite = match self.composites.get(view) {
            Lookup::Hit(composite) => composite,
            // Never kept: a run is always composited.
            Lookup::Empty => return Ok(()),
            Lookup::Miss => {
                let bitmaps = drawn.iter().map(|d| &d.bitmaps);
                let composite = self.raster.compose(&placed.effects, frame, bitmaps, work)?;
                let parts = [&composite.fill, &composite.border, &composite.shadow];
                let size = sizes::composite(
                    drawn.len(),
                    parts.iter().copied().flatten().map(pixels).sum(),
                );
                let key = RunKey::taking(effects, &mut self.glyphs)?;
                let composite = Composited {
                    fill: composite.fill,
                    border: composite.border,
                    shadow: composite.shadow,
                };
                self.composites.insert(key, composite, size)?
            }
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

/// A glyph's bitmaps from the bitmap cache, drawn and kept on a miss; the
/// keys of its fill and border as they are found.
struct CachedBitmaps<'a> {
    bitmaps: &'a mut Bitmaps,
    outlines: &'a mut Outlines,
    glyph: &'a Kept<OutlineValue>,
    fill: Option<BitmapKey>,
    border: Option<BitmapKey>,
}

impl GlyphSource for CachedBitmaps<'_> {
    type Bitmap = SharedBitmap;

    fn fill(
        &mut self,
        raster: &mut Raster,
        shape: &Shape,
        at: Quantized,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap<SharedBitmap>>> {
        let key = BitmapKey {
            source: BitmapSource::Outline(OutlineId(self.glyph.clone())),
            transform: at.transform,
        };
        let found = self
            .bitmaps
            .drawn(raster, key.clone(), shape, None, at, work)?;
        self.fill = found.is_some().then_some(key);
        Ok(found)
    }

    fn opaque_box(
        &mut self,
        raster: &mut Raster,
        at: Option<Quantized>,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap<SharedBitmap>>> {
        // libass looks the unit square up before it places the box
        // (`ass_render.c:1531`).
        let unit_box = self.outlines.unit_box()?;
        let Some(at) = at else {
            return Ok(None);
        };
        let key = BitmapKey {
            source: BitmapSource::Outline(OutlineId(unit_box.clone())),
            transform: at.transform,
        };
        let shape = &unit_box.shape;
        let found = self
            .bitmaps
            .drawn(raster, key.clone(), shape, None, at, work)?;
        self.border = found.is_some().then_some(key);
        Ok(found)
    }

    fn border(
        &mut self,
        raster: &mut Raster,
        _shape: &Shape,
        stroke: BorderStroke,
        placement: &GlyphPlacement,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap<SharedBitmap>>> {
        let Some(border) = self.outlines.border(self.glyph, stroke, raster, work)? else {
            return Ok(None);
        };
        let Some(at) = placement.place_stroke(&border.shape) else {
            return Ok(None);
        };
        let key = BitmapKey {
            source: BitmapSource::Border(OutlineId(self.glyph.clone()), stroke),
            transform: at.transform,
        };
        let shape = &border.shape;
        let found = self
            .bitmaps
            .drawn(raster, key.clone(), shape, Some(&border), at, work)?;
        self.border = found.is_some().then_some(key);
        Ok(found)
    }
}

/// The bitmap cache (libass's `bitmap_cache`).
#[derive(Debug)]
struct Bitmaps {
    budget: Budget,
    cache: Cache<BitmapKey, KeptBitmap>,
}

impl Bitmaps {
    /// The bitmap of `shape` under `key`, placed at `at`: from the cache, or
    /// drawn and kept with `drawn_from`. A shape that draws nothing gives a
    /// bitmap without pixels, which is kept too, as libass keeps it.
    fn get(
        &mut self,
        raster: &mut Raster,
        key: BitmapKey,
        shape: &Shape,
        drawn_from: Option<&Kept<OutlineValue>>,
        at: Quantized,
        work: &mut WorkBudget,
    ) -> Result<PlacedBitmap<SharedBitmap>> {
        charge(work, LOOKUP_UNITS)?;
        let kept = match self.cache.get(&key) {
            Lookup::Hit(kept) => kept,
            Lookup::Empty => unreachable!("bitmaps are kept with a value"),
            Lookup::Miss => {
                let bitmap = match raster.draw(shape, &at.transform, work)? {
                    Some(bitmap) => bitmap,
                    // libass keeps a bitmap without pixels.
                    None => Bitmap::new(&self.budget, 0, 0, &Limits::default())?,
                };
                let [first, second] = shape.outlines();
                let points = first.points().len().saturating_add(second.points().len());
                let segments = first
                    .segments()
                    .len()
                    .saturating_add(second.segments().len());
                let size = sizes::bitmap(pixels(&bitmap), points, segments);
                let kept = KeptBitmap {
                    bitmap,
                    _drawn_from: drawn_from.cloned(),
                };
                self.cache.insert(key, kept, size)?
            }
        };
        Ok(PlacedBitmap {
            bitmap: SharedBitmap(kept),
            origin: at.position,
        })
    }

    /// [`Bitmaps::get`] for a glyph, whose bitmaps are never empty: `None`
    /// when it draws nothing.
    fn drawn(
        &mut self,
        raster: &mut Raster,
        key: BitmapKey,
        shape: &Shape,
        drawn_from: Option<&Kept<OutlineValue>>,
        at: Quantized,
        work: &mut WorkBudget,
    ) -> Result<Option<PlacedBitmap<SharedBitmap>>> {
        let found = self.get(raster, key, shape, drawn_from, at, work)?;
        Ok((found.bitmap.width() != 0).then_some(found))
    }
}

/// The bytes libass counts for a bitmap's pixels: its stride times its
/// height (`bitmap_size`).
fn pixels(bitmap: &Bitmap) -> usize {
    bitmap.stride().saturating_mul(bitmap.height())
}

#[cfg(test)]
mod tests {
    use triergon_core::{Budget, BudgetVec};
    use triergon_raster::{Border, Effects, FrameScale, Shadow, Vector};

    use super::{CacheKey, EffectsKey, GlyphRef, RunKey};

    fn glyph(border_origin: i32) -> GlyphRef {
        GlyphRef {
            fill: None,
            border: None,
            origin: Vector::new(0, 0),
            border_origin: Vector::new(border_origin, 0),
        }
    }

    #[test]
    fn a_key_keeps_no_more_than_it_holds() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut glyphs = BudgetVec::new(&budget);
        for _ in 0..500 {
            glyphs.try_push(glyph(0)).unwrap();
        }
        // A long run was looked up; the next is one glyph, made anew.
        glyphs.clear();
        glyphs.try_push(glyph(0)).unwrap();
        let key = RunKey::taking(EffectsKey::default(), &mut glyphs).unwrap();
        assert_eq!(key.glyphs.capacity(), 1);
        assert_eq!(glyphs.capacity(), 0);
    }

    #[test]
    fn keys_tell_apart_what_composites_tell_apart() {
        let frame = FrameScale {
            blur: [1.0, 1.0],
            border: [1.0, 1.0],
            pixel_aspect: 1.0,
        };
        let plain = Effects {
            border: Border::Outline {
                size: [2.0, 2.0],
                keeps_fill: false,
            },
            ..Effects::default()
        };
        let variants = [
            Effects { be: 1, ..plain },
            Effects { blur: 2.0, ..plain },
            Effects {
                shadow: Shadow {
                    offset: [2.0, 2.0],
                    keeps_fill: true,
                },
                ..plain
            },
        ];
        let glyphs = [glyph(0)];
        let key = |effects: &Effects, glyphs| RunKey::view_of(effects.key(&frame), glyphs);
        for variant in &variants {
            assert!(!RunKey::same(&key(&plain, &glyphs), &key(variant, &glyphs)));
        }
        // Glyphs whose borders sit elsewhere in the run.
        let moved = [glyph(1)];
        assert!(!RunKey::same(&key(&plain, &glyphs), &key(&plain, &moved)));
        assert!(RunKey::same(
            &key(&plain, &glyphs),
            &key(&plain, &[glyph(0)])
        ));
    }
}
