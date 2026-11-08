//! Drawing an event's glyphs and combining them run by run, as libass's
//! `render_and_combine_glyphs` does through the bitmap and composite caches:
//! each glyph's fill and border placed in the frame, then each run's fill,
//! border and shadow composited.

use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};
use triergon_raster::{
    Bitmap, Border, Effects, FrameScale, GlyphBitmaps, GlyphRequest, Matrix, Placement, Raster,
    Run, Shadow, Shape, Vector, pixels_26_6,
};

use super::math::d6_to_double;
use super::text::{GlyphInfo, Glyphs};
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
}

/// A run being collected.
#[derive(Debug)]
struct Open {
    colours: [Rgba; 4],
    effect: Option<Karaoke>,
    effect_timing: i32,
    effects: Effects,
    run: Run,
    glyphs: BudgetVec<GlyphBitmaps>,
}

/// The effects a glyph is drawn with, as its state sets them (libass's
/// filter flags, which it takes per glyph): its border and opaque box, and,
/// for the glyph that starts a run, the run's composite.
fn effects(g: &GlyphInfo<'_>) -> Effects {
    let stroked = g.border[0] != 0.0 || g.border[1] != 0.0;
    let alpha = |c: Rgba| c.0 & 0xff;
    let border = if g.border_style.0 == 3 {
        Border::OpaqueBox {
            size: g.border,
            shown: stroked,
        }
    } else if stroked {
        Border::Outline {
            size: g.border,
            keeps_fill: alpha(g.colours[0]) == 0 && alpha(g.colours[1]) == 0 && g.fade == 0,
        }
    } else {
        Border::None
    };
    Effects {
        border,
        shadow: Shadow {
            offset: g.shadow,
            keeps_fill: matches!(g.effect, Some(Karaoke::Fill | Karaoke::Outline))
                || alpha(g.colours[0]) != 0xff,
        },
        blur: g.blur,
        be: u32::from(g.be),
    }
}

/// `ass_apply_fade`: a positive fade raises the alpha towards transparent.
pub(super) fn apply_fade(colour: Rgba, fade: i32) -> Rgba {
    if fade <= 0 {
        return colour;
    }
    let a = u64::from(colour.0 & 0xff);
    let fade = u64::from(fade.unsigned_abs());
    let faded = a - (a * fade + 0x7f) / 0xff + fade;
    Rgba((colour.0 & 0xffff_ff00) | (faded as u32 & 0xff))
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

    /// Draws and combines the glyphs of an event placed at `device` (pre-
    /// aspect pixels); the runs in logical order.
    pub(super) fn render(
        &mut self,
        glyphs: &mut Glyphs<'_>,
        device: (f64, f64),
        frame: &FrameScale,
        left_margin: i32,
        out: &mut BudgetVec<Combined>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        out.clear();
        let par = frame.pixel_aspect;
        let left = f64::from(left_margin);
        let device_x = (device.0 - left) * par + left;
        let mut new_run = true;
        let mut open: Option<Open> = None;
        for i in 0..glyphs.len {
            if glyphs.get(i).starts_new_run {
                new_run = true;
            }
            if glyphs.get(i).skip {
                continue;
            }
            let mut member = Some(i);
            while let Some(g) = member {
                member = glyphs.get(g).next;
                let effects = effects(glyphs.get(g));
                if new_run {
                    if let Some(run) = open.take() {
                        self.finish(run, frame, out, work)?;
                    }
                    open = Some(Self::open(glyphs.get(g), effects, &self.budget));
                    new_run = false;
                }
                let info = glyphs.get_mut(g);
                info.pos.x = pixels_26_6(device_x + d6_to_double(info.pos.x) * par);
                info.pos.y = pixels_26_6(device.1).wrapping_add(info.pos.y);
                let Some(run) = open.as_mut() else { continue };
                if let Some(drawn) = self.draw(info, &effects, frame, &mut run.run, work)? {
                    run.glyphs.try_push(drawn).map_err(|e| e.error)?;
                }
            }
        }
        if let Some(run) = open.take() {
            self.finish(run, frame, out, work)?;
        }
        Ok(())
    }

    fn open(g: &GlyphInfo<'_>, effects: Effects, budget: &Budget) -> Open {
        Open {
            colours: g.colours.map(|c| apply_fade(c, g.fade)),
            effect: g.effect,
            effect_timing: g.effect_timing,
            effects,
            run: Run::default(),
            glyphs: BudgetVec::new(budget),
        }
    }

    /// `get_bitmap_glyph`: the glyph's bitmaps, `None` when it draws nothing.
    fn draw(
        &mut self,
        info: &GlyphInfo<'_>,
        effects: &Effects,
        frame: &FrameScale,
        run: &mut Run,
        work: &mut WorkBudget,
    ) -> Result<Option<GlyphBitmaps>> {
        let Some(outline) = &info.outline else {
            return Ok(None);
        };
        if info.symbol == u32::from(b'\n') || info.symbol == 0 || info.skip {
            return Ok(None);
        }
        let request = GlyphRequest {
            shape: &outline.shape,
            placement: Placement {
                rotation: info.rotation,
                shear: info.shear,
                scale: info.scale,
                shift: info.shift,
                ascender: info.asc,
                descender: info.desc,
                advance: info.spacing_scaled.wrapping_add(info.advance.x),
                scale_fix: info.scale_fix,
                position: info.pos,
                outline_scale: info.outline_scale,
                outline_offset: info.outline_offset,
            },
            frame: *frame,
            effects,
            karaoke: info.effect == Some(Karaoke::Fill),
        };
        self.raster.render(&request, run, work)
    }

    /// Composites a collected run.
    fn finish(
        &mut self,
        mut run: Open,
        frame: &FrameScale,
        out: &mut BudgetVec<Combined>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        if run.glyphs.is_empty() {
            return Ok(());
        }
        if run.effect == Some(Karaoke::Fill) {
            run.effect_timing = (d6_to_double(run.run.leftmost_x())
                + d6_to_double(run.effect_timing) * frame.pixel_aspect)
                .round() as i32;
        }
        let composite = self
            .raster
            .compose(&run.effects, frame, run.glyphs.as_slice(), work)?;
        out.try_push(Combined {
            colours: run.colours,
            effect: run.effect,
            effect_timing: run.effect_timing,
            origin: composite.origin,
            fill: composite.fill.map(Arc::new),
            border: composite.border.map(Arc::new),
            shadow: composite.shadow.map(Arc::new),
        })
        .map_err(|e| e.error)?;
        Ok(())
    }
}
