//! An event's glyphs as the renderer lays them out (libass's `GlyphInfo`):
//! read from the event's text with the state each one was given
//! (`parse_events`), split into runs of equal style (`split_style_runs`), then
//! filled in by shaping, layout and drawing.

use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Result};
use triergon_font::GlyphId;
use triergon_raster::{Rect, Vector, pixels_26_6};
use triergon_shape::Script as WritingScript;

use super::fonts::{FontId, Library};
use super::geometry::Scales;
use super::outlines::OutlineValue;
use crate::{Bold, BorderStyle, Context, Italic, Karaoke, KaraokeMark, Rgba, Symbol};

/// Code points from which an `@` font draws glyphs rotated
/// (`VERTICAL_LOWER_BOUND`).
const VERTICAL_LOWER_BOUND: u32 = 0x2f1;

/// The font size libass draws every glyph at with hinting off: the glyph is
/// then scaled to its size.
pub(super) const DRAWN_SIZE: f64 = 256.0;

/// What decorates a glyph's outline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) struct Decorations {
    pub(super) underline: bool,
    pub(super) strike_out: bool,
    /// The glyph of an `@` font, drawn rotated by 90°.
    pub(super) rotate: bool,
}

/// A line break before a glyph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Break {
    /// The glyph continues its line.
    #[default]
    None,
    /// Wrapping starts a line here; rebalancing may move the break.
    Soft,
    /// `\N`, or `\n` with `\q2`, starts a line after it.
    Hard,
}

/// A drawing glyph's commands and how they are scaled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct DrawingText<'s> {
    pub(super) text: &'s [u8],
    pub(super) scale: i32,
    pub(super) baseline: i32,
}

/// One glyph of an event.
#[derive(Clone, Debug)]
pub(super) struct GlyphInfo<'s> {
    pub(super) symbol: u32,
    pub(super) drawing: Option<DrawingText<'s>>,
    pub(super) font: FontId,
    pub(super) colours: [Rgba; 4],
    pub(super) karaoke: KaraokeMark,
    /// The size the glyph is drawn at, [`DRAWN_SIZE`] once scaled.
    pub(super) font_size: f64,
    pub(super) be: u8,
    pub(super) blur: f64,
    pub(super) shadow: [f64; 2],
    /// Horizontal and vertical scale, with the size's correction.
    pub(super) scale: [f64; 2],
    pub(super) border_style: BorderStyle,
    pub(super) border: [f64; 2],
    pub(super) spacing: f64,
    pub(super) bold: Bold,
    pub(super) italic: Italic,
    pub(super) decorations: Decorations,
    pub(super) rotation: [f64; 3],
    pub(super) shear: [f64; 2],
    pub(super) fade: i32,
    /// `\fsp` in frame 26.6 units.
    pub(super) spacing_scaled: i32,
    /// The scale the size correction divided out.
    pub(super) scale_fix: f64,

    pub(super) starts_new_run: bool,
    pub(super) script: WritingScript,
    pub(super) skip: bool,
    pub(super) shape_run: u32,
    pub(super) face: usize,
    pub(super) glyph: GlyphId,
    /// The line break before the glyph.
    pub(super) linebreak: Break,
    pub(super) trimmed: bool,

    pub(super) outline: Option<Arc<OutlineValue>>,
    pub(super) outline_scale: [f64; 2],
    pub(super) outline_offset: [f64; 2],
    pub(super) bbox: Rect,
    pub(super) asc: i32,
    pub(super) desc: i32,
    pub(super) advance: Vector,
    pub(super) cluster_advance: Vector,
    pub(super) offset: Vector,
    pub(super) pos: Vector,
    pub(super) shift: Vector,
    /// The karaoke effect and boundary after karaoke timing.
    pub(super) effect: Option<Karaoke>,
    pub(super) effect_timing: i32,
    /// The next glyph of the cluster, after the event's root glyphs.
    pub(super) next: Option<usize>,
}

impl GlyphInfo<'_> {
    pub(super) fn is_drawing(&self) -> bool {
        self.drawing.is_some()
    }

    /// Whether the glyph and `other` differ in a way that starts a new run;
    /// libass compares the values exactly.
    #[allow(clippy::float_cmp)]
    fn differs(&self, other: &Self, library: &Library) -> bool {
        let (a, b) = (library.get(self.font), library.get(other.font));
        a.family() != b.family()
            || a.vertical() != b.vertical()
            || self.font_size != other.font_size
            || self.colours != other.colours
            || self.be != other.be
            || self.blur != other.blur
            || self.shadow != other.shadow
            || self.rotation != other.rotation
            || self.shear != other.shear
            || self.scale != other.scale
            || self.border_style != other.border_style
            || self.border != other.border
            || self.spacing != other.spacing
            || self.italic != other.italic
            || self.bold != other.bold
            || self.decorations.underline != other.decorations.underline
            || self.decorations.strike_out != other.decorations.strike_out
    }
}

/// An event's glyphs: the roots in text order, then the extra glyphs of
/// clusters that shape to several, each chained from its cluster's last.
#[derive(Debug)]
pub(super) struct Glyphs<'s> {
    pub(super) list: BudgetVec<GlyphInfo<'s>>,
    /// The number of root glyphs.
    pub(super) len: usize,
}

impl<'s> Glyphs<'s> {
    /// Reads the glyphs of `context`'s event (`parse_events`). `None` when a
    /// glyph's font has no face, which libass treats as a failed event.
    pub(super) fn read(
        budget: &Budget,
        context: &mut Context<'s>,
        library: &mut Library,
        scales: &Scales,
        par: f64,
    ) -> Result<Option<Self>> {
        let mut list = BudgetVec::new(budget);
        while let Some(glyph) = context.next_glyph()? {
            let state = glyph.state;
            let desc = state.font();
            let Some(font) = library.font(&desc)? else {
                return Ok(None);
            };
            let (symbol, drawing) = match glyph.symbol {
                Symbol::Char(code) => (code, None),
                Symbol::Drawing {
                    text,
                    scale,
                    baseline,
                } => (
                    0xfffc,
                    Some(DrawingText {
                        text,
                        scale,
                        baseline,
                    }),
                ),
            };
            let colours = state.colours;
            let mut info = GlyphInfo {
                symbol,
                drawing,
                font,
                colours: [
                    colours.primary,
                    colours.secondary,
                    colours.outline,
                    colours.back,
                ],
                karaoke: glyph.karaoke,
                // VSFilter scales fonts by the play resolution's height in
                // both directions.
                font_size: (state.font_size * scales.screen[1]).abs(),
                be: state.blur_edges,
                blur: state.blur,
                shadow: [state.shadow_x, state.shadow_y],
                scale: [state.scale_x, state.scale_y],
                border_style: state.border_style,
                border: [state.border_x, state.border_y],
                spacing: state.spacing,
                bold: state.bold,
                italic: state.italic,
                decorations: Decorations {
                    underline: state.underline,
                    strike_out: state.strike_out,
                    rotate: desc.vertical && symbol >= VERTICAL_LOWER_BOUND,
                },
                rotation: [state.rotate_x, state.rotate_y, state.rotate_z],
                shear: [state.shear_x, state.shear_y],
                fade: glyph.fade,
                spacing_scaled: 0,
                scale_fix: 1.0,
                starts_new_run: false,
                script: WritingScript::UNKNOWN,
                skip: false,
                shape_run: 0,
                face: 0,
                glyph: GlyphId(0),
                linebreak: Break::None,
                trimmed: false,
                outline: None,
                outline_scale: [0.0; 2],
                outline_offset: [0.0; 2],
                bbox: Rect::new(0, 0, 0, 0),
                asc: 0,
                desc: 0,
                advance: Vector::default(),
                cluster_advance: Vector::default(),
                offset: Vector::default(),
                pos: Vector::default(),
                shift: Vector::default(),
                effect: glyph.karaoke.kind,
                effect_timing: glyph.karaoke.timing,
                next: None,
            };
            if drawing.is_none() {
                info.spacing_scaled =
                    pixels_26_6(info.spacing * scales.screen[0] / par * info.scale[0]);
                info.fix_scaling();
            }
            list.try_push(info).map_err(|e| e.error)?;
        }
        let len = list.len();
        Ok(Some(Self { list, len }))
    }

    /// `split_style_runs`: a run starts at the first glyph, at karaoke timing,
    /// a change of karaoke effect, around drawings and wherever the style
    /// changes.
    pub(super) fn split_style_runs(&mut self, library: &Library) {
        let glyphs = &mut self.list.as_mut_slice()[..self.len];
        let Some(first) = glyphs.first_mut() else {
            return;
        };
        first.starts_new_run = true;
        let mut last_effect = first.karaoke.kind;
        for i in 1..glyphs.len() {
            let (last, info) = (&glyphs[i - 1], &glyphs[i]);
            let effect = info.karaoke.kind;
            let starts = info.karaoke.timing != 0
                || (effect.is_some() && effect != last_effect)
                || info.is_drawing()
                || last.is_drawing()
                || last.differs(info, library);
            glyphs[i].starts_new_run = starts;
            if effect.is_some() {
                last_effect = effect;
            }
        }
    }

    /// The root glyphs.
    pub(super) fn roots(&self) -> &[GlyphInfo<'s>] {
        &self.list.as_slice()[..self.len]
    }

    pub(super) fn roots_mut(&mut self) -> &mut [GlyphInfo<'s>] {
        &mut self.list.as_mut_slice()[..self.len]
    }

    /// The indices of root `i` and the rest of its cluster.
    pub(super) fn cluster(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(Some(i), |&g| self.list.as_slice()[g].next)
    }

    /// Appends a copy of `from` to the end of `root`'s cluster; its index.
    pub(super) fn extend_cluster(&mut self, root: usize) -> Result<usize> {
        let last = self.cluster(root).last().unwrap_or(root);
        let mut copy = self.list.as_slice()[last].clone();
        copy.next = None;
        let index = self.list.len();
        self.list.try_push(copy).map_err(|e| e.error)?;
        self.list.as_mut_slice()[last].next = Some(index);
        Ok(index)
    }

    pub(super) fn get(&self, i: usize) -> &GlyphInfo<'s> {
        &self.list.as_slice()[i]
    }

    pub(super) fn get_mut(&mut self, i: usize) -> &mut GlyphInfo<'s> {
        &mut self.list.as_mut_slice()[i]
    }

    /// Applies `f` to root `i` and the rest of its cluster, in order.
    pub(super) fn each_in_cluster(&mut self, i: usize, mut f: impl FnMut(&mut GlyphInfo<'s>)) {
        let mut at = Some(i);
        while let Some(g) = at {
            let info = &mut self.list.as_mut_slice()[g];
            f(info);
            at = info.next;
        }
    }
}

impl GlyphInfo<'_> {
    /// `fix_glyph_scaling`: the glyph is drawn at [`DRAWN_SIZE`] and scaled to
    /// its size, so FreeType's grid fitting cannot move it.
    fn fix_scaling(&mut self) {
        if self.font_size == 0.0 {
            return;
        }
        let mul = self.font_size / DRAWN_SIZE;
        self.scale_fix = 1.0 / mul;
        self.scale[0] *= mul;
        self.scale[1] *= mul;
        self.font_size = DRAWN_SIZE;
    }
}
