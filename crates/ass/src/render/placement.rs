//! The fourth stage: a laid-out event placed in the frame. Its position by
//! `\pos` or `\move`, a scroll, or its alignment and margins; its rotation
//! origin (`calculate_rotation_params`); the runs its drawn glyphs are
//! composited in; and each glyph's placement and effects as it is drawn
//! (`render_and_combine_glyphs` up to drawing). Drawing them is the
//! painter's.

use std::ops::Range;

use triergon_core::{Budget, BudgetVec, Result};
use triergon_raster::{
    Border, Effects, FrameScale, GlyphRequest, Placement, Rect, Shadow, Vector, pixels_26_6,
};

use super::cache::Kept;
use super::geometry::{Geometry, Scales};
use super::layout::{LaidChar, Layout, TextBox, Timing};
use super::outlines::OutlineValue;
use super::shaping::Shaped;
use super::text::{Char, GlyphStyle, Text};
use super::units::d6_to_double;
use crate::{
    EventState, HAlign, Karaoke, Margins, Placement as EventPlacement, Rgba, ScrollDirection,
    VAlign,
};

/// A glyph placed in a run: glyph `glyph` of [`Layout::glyphs`], of
/// character `char`.
#[derive(Clone, Copy, Debug)]
pub(super) struct PlacedGlyph {
    char: usize,
    glyph: usize,
}

/// A run of glyphs composited together (libass's `CombinedBitmapInfo`
/// before drawing), as the glyph that starts it sets it.
#[derive(Debug)]
pub(super) struct PlacedRun {
    /// The colours, faded.
    pub(super) colours: [Rgba; 4],
    pub(super) effect: Option<Karaoke>,
    /// For `\kf`, the boundary from the first glyph's pen, pre-aspect 26.6.
    pub(super) effect_timing: i32,
    /// The effects the run is composited with.
    pub(super) effects: Effects,
    /// Its glyphs in [`Runs`].
    glyphs: Range<usize>,
}

/// An event's drawn glyphs in the runs that are composited together, in
/// text order. Placed again for each event, into the same storage.
#[derive(Debug)]
pub(super) struct Runs {
    runs: BudgetVec<PlacedRun>,
    glyphs: BudgetVec<PlacedGlyph>,
}

impl Runs {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            runs: BudgetVec::new(budget),
            glyphs: BudgetVec::new(budget),
        }
    }

    /// Each run with its glyphs.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&PlacedRun, &[PlacedGlyph])> {
        self.runs
            .iter()
            .map(|run| (run, &self.glyphs[run.glyphs.clone()]))
    }

    /// Groups the drawn glyphs of a laid-out event into runs: a run starts
    /// at a character that starts one and is set by the first drawn
    /// character after it. A glyph without an outline, and the glyph of a
    /// `\n` or a drawing's placeholder, joins none.
    pub(super) fn place(&mut self, text: &Text, shaped: &Shaped, layout: &Layout) -> Result<()> {
        self.runs.clear();
        self.glyphs.clear();
        let mut new_run = true;
        let chars = text
            .chars()
            .iter()
            .zip(&*shaped.clusters)
            .zip(&*layout.chars);
        for (char, ((c, cluster), laid)) in chars.enumerate() {
            new_run |= laid.starts_run;
            if !laid.drawn {
                continue;
            }
            if new_run {
                let style = text.style(c);
                let colours = laid.karaoke.colours(style.colours);
                let start = self.glyphs.len();
                self.runs
                    .try_push(PlacedRun {
                        colours: colours.map(|colour| apply_fade(colour, c.fade)),
                        effect: laid.karaoke.effect,
                        effect_timing: laid.karaoke.timing,
                        effects: effects(style, &colours, c.fade, laid.karaoke.effect),
                        glyphs: start..start,
                    })
                    .map_err(|e| e.error)?;
                new_run = false;
            }
            if c.symbol == u32::from(b'\n') || c.symbol == 0 {
                continue;
            }
            for glyph in cluster.glyphs.clone() {
                if layout.glyphs[glyph].outline.is_some() {
                    self.glyphs
                        .try_push(PlacedGlyph { char, glyph })
                        .map_err(|e| e.error)?;
                }
            }
            if let Some(run) = self.runs.as_mut_slice().last_mut() {
                run.glyphs.end = self.glyphs.len();
            }
        }
        super::fit(&mut self.runs)?;
        super::fit(&mut self.glyphs)
    }
}

/// Where an event is placed: its position, pre-aspect frame pixels, and the
/// origin it rotates about.
#[derive(Clone, Copy, Debug)]
pub(super) struct Anchor {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) origin: (f64, f64),
}

impl Anchor {
    /// The event's position by `\pos` or `\move`, by a scroll, or by its
    /// alignment and margins, and its rotation origin, `\org` or the
    /// alignment point.
    pub(super) fn new(
        geometry: &Geometry<'_>,
        state: &EventState<'_>,
        margins: Margins,
        layout: &Layout,
    ) -> Self {
        let (x, y) = position(geometry, state, margins, layout);
        let origin = if let Some((ox, oy)) = state.origin {
            (geometry.x_pos(ox), geometry.y_pos(oy))
        } else {
            let (bx, by) = base_point(
                &layout.text_box,
                state.alignment.horizontal,
                state.alignment.vertical,
            );
            (x + bx, y + by)
        };
        Self { x, y, origin }
    }

    /// The event's box for collision handling, in frame pixels: its lines
    /// and their borders, with VSFilter's biased roundings.
    pub(super) fn event_box(&self, layout: &Layout, par: f64) -> Rect {
        let (lines, text_box) = (&layout.lines, &layout.text_box);
        let top = (self.y - lines.first().asc - f64::from(lines.border_top)) as i32;
        let height =
            (lines.height + f64::from(lines.border_bottom) + f64::from(lines.border_top)) as i32;
        let border_x = f64::from(lines.border_x);
        let left = ((self.x + text_box.x_min) * par - border_x + 0.5) as i32;
        let width = ((text_box.x_max - text_box.x_min) * par + 2.0 * border_x + 0.5) as i32;
        Rect::new(
            left,
            top,
            left.wrapping_add(width),
            top.wrapping_add(height),
        )
    }
}

/// `get_base_point`: the point of the box the alignment anchors.
fn base_point(bbox: &TextBox, halign: HAlign, valign: VAlign) -> (f64, f64) {
    let x = match halign {
        HAlign::Left => bbox.x_min,
        HAlign::Center => (bbox.x_max + bbox.x_min) / 2.0,
        HAlign::Right => bbox.x_max,
        HAlign::None => 0.0,
    };
    let y = match valign {
        VAlign::Top => bbox.y_min,
        VAlign::Middle => (bbox.y_max + bbox.y_min) / 2.0,
        VAlign::Bottom => bbox.y_max,
        VAlign::TopAndMiddle => 0.0,
    };
    (x, y)
}

/// The event's position in pre-aspect frame pixels.
fn position(
    geometry: &Geometry<'_>,
    state: &EventState<'_>,
    margins: Margins,
    layout: &Layout,
) -> (f64, f64) {
    let explicit = state.explicit;
    let (bbox, lines) = (&layout.text_box, &layout.lines);
    let (mut x, mut y) = (0.0, 0.0);
    let positioned = match state.placement {
        EventPlacement::Positioned(px, py) => {
            let (bx, by) = base_point(bbox, state.alignment.horizontal, state.alignment.vertical);
            x = geometry.x_pos(px) - bx;
            y = geometry.y_pos(py) - by;
            true
        }
        EventPlacement::Margins => false,
    };
    let (play_x, play_y) = geometry.play_res;
    match state.scroll {
        Some(s) if s.direction == ScrollDirection::RightToLeft => {
            x = geometry.x_pos(f64::from(play_x) - s.shift);
        }
        Some(s) if s.direction == ScrollDirection::LeftToRight => {
            x = geometry.x_pos(s.shift) - (bbox.x_max - bbox.x_min);
        }
        _ if !positioned => x = geometry.x_left(f64::from(margins.left), explicit),
        _ => {}
    }
    let first_asc = lines.first().asc;
    match state.scroll.and_then(|s| s.band.map(|band| (s, band))) {
        Some((s, (y0, _))) if s.direction == ScrollDirection::TopToBottom => {
            y = geometry.y_mid(f64::from(y0) + s.shift, explicit) - bbox.y_max;
        }
        Some((s, (_, y1))) if s.direction == ScrollDirection::BottomToTop => {
            y = geometry.y_mid(f64::from(y1) - s.shift, explicit) - bbox.y_min;
        }
        _ if !positioned => match state.alignment.vertical {
            VAlign::Top => y = geometry.y_top(f64::from(margins.vertical), explicit) + first_asc,
            VAlign::Middle => {
                let scr_y = geometry.y_mid(f64::from(play_y) / 2.0, explicit);
                y = scr_y - (bbox.y_max + bbox.y_min) / 2.0;
            }
            VAlign::Bottom | VAlign::TopAndMiddle => {
                let line_pos = if explicit {
                    0.0
                } else {
                    geometry.canvas.settings.line_position
                };
                let bottom =
                    geometry.y_sub(f64::from(play_y.wrapping_sub(margins.vertical)), explicit);
                let top = geometry.y_top(0.0, explicit);
                y = bottom + (top - bottom) * line_pos / 100.0;
                y -= lines.height;
                y += first_asc;
                let y0 = top + first_asc;
                if y < y0 && line_pos > 0.0 {
                    y = y0;
                }
            }
        },
        _ => {}
    }
    (x, y)
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

/// The effects a glyph is drawn with, as its state sets them (libass's
/// filter flags, which it takes per glyph): its border and opaque box, and,
/// for the glyph that starts a run, the run's composite.
fn effects(style: &GlyphStyle, colours: &[Rgba; 4], fade: i32, effect: Option<Karaoke>) -> Effects {
    let stroked = style.border[0] != 0.0 || style.border[1] != 0.0;
    let alpha = |c: Rgba| c.0 & 0xff;
    let border = if style.border_style.0 == 3 {
        Border::OpaqueBox {
            size: style.border,
            shown: stroked,
        }
    } else if stroked {
        Border::Outline {
            size: style.border,
            keeps_fill: alpha(colours[0]) == 0 && alpha(colours[1]) == 0 && fade == 0,
        }
    } else {
        Border::None
    };
    Effects {
        border,
        shadow: Shadow {
            offset: style.shadow,
            keeps_fill: matches!(effect, Some(Karaoke::Fill | Karaoke::Outline))
                || alpha(colours[0]) != 0xff,
        },
        blur: style.blur,
        be: u32::from(style.be),
    }
}

/// A glyph's karaoke: libass times the first glyph of a cluster; shaping
/// copied the others before karaoke timing, so they keep their tags'.
fn timing(c: &Char, laid: &LaidChar, first: bool) -> Timing {
    if first {
        laid.karaoke
    } else {
        Timing::untimed(c.karaoke)
    }
}

/// What a placed glyph is drawn with.
#[derive(Debug)]
pub(super) struct GlyphDrawing<'a> {
    pub(super) outline: &'a Kept<OutlineValue>,
    placement: Placement,
    effects: Effects,
    /// It is timed by `\kf`, so its run tracks its leftmost x.
    karaoke: bool,
}

impl GlyphDrawing<'_> {
    /// What [`Run::place`](triergon_raster::Run::place) places.
    pub(super) fn request(&self, frame: &FrameScale) -> GlyphRequest<'_> {
        GlyphRequest {
            shape: &self.outline.shape,
            placement: self.placement,
            frame: *frame,
            effects: &self.effects,
            karaoke: self.karaoke,
        }
    }
}

/// What places an event's glyphs in the frame as they are drawn.
#[derive(Clone, Copy, Debug)]
pub(super) struct Placer<'a> {
    text: &'a Text,
    shaped: &'a Shaped,
    layout: &'a Layout,
    anchor: Anchor,
    /// The anchor in the frame, x stretched by the pixel aspect about the
    /// left margin, y in 26.6.
    x: f64,
    y: i32,
    par: f64,
    border_scale: [f64; 2],
}

impl<'a> Placer<'a> {
    pub(super) fn new(
        text: &'a Text,
        shaped: &'a Shaped,
        layout: &'a Layout,
        anchor: &Anchor,
        geometry: &Geometry<'_>,
        scales: &Scales,
    ) -> Self {
        let par = geometry.par;
        let left = f64::from(geometry.canvas.settings.margins.left);
        Self {
            text,
            shaped,
            layout,
            anchor: *anchor,
            x: (anchor.x - left) * par + left,
            y: pixels_26_6(anchor.y),
            par,
            border_scale: scales.border,
        }
    }

    /// What `placed` is drawn with: its outline, its position in the frame
    /// and offset from the rotation origin displaced by its shadow
    /// (`calculate_rotation_params`), and the effects its state gives it.
    pub(super) fn glyph(&self, placed: &PlacedGlyph) -> Option<GlyphDrawing<'a>> {
        let c = &self.text.chars()[placed.char];
        let style = self.text.style(c);
        let laid = &self.layout.chars[placed.char];
        let glyph = &self.layout.glyphs[placed.glyph];
        let outline = glyph.outline.as_ref()?;
        let first = placed.glyph == self.shaped.clusters[placed.char].glyphs.start;
        let timing = timing(c, laid, first);
        let colours = timing.colours(style.colours);
        let (anchor, par, border) = (&self.anchor, self.par, self.border_scale);
        let shift = Vector::new(
            glyph.pos.x.wrapping_add(pixels_26_6(
                anchor.x - anchor.origin.0 + style.shadow[0] * border[0] / par,
            )),
            glyph.pos.y.wrapping_add(pixels_26_6(
                anchor.y - anchor.origin.1 + style.shadow[1] * border[1],
            )),
        );
        let placement = Placement {
            rotation: style.rotation,
            shear: style.shear,
            scale: style.scale,
            shift,
            ascender: glyph.asc,
            descender: glyph.desc,
            advance: style.spacing_scaled.wrapping_add(glyph.advance.x),
            scale_fix: style.scale_fix,
            position: Vector::new(
                pixels_26_6(self.x + d6_to_double(glyph.pos.x) * par),
                self.y.wrapping_add(glyph.pos.y),
            ),
            outline_scale: outline.scale,
            outline_offset: [0.0, outline.offset_y],
        };
        Some(GlyphDrawing {
            outline: &outline.value,
            placement,
            effects: effects(style, &colours, c.fade, timing.effect),
            karaoke: timing.effect == Some(Karaoke::Fill),
        })
    }
}
