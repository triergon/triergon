//! The first stage: an event's text evaluated at a time (libass's
//! `parse_events`), each character with the font it asks for and its glyph
//! state scaled to the frame, and where its style runs start
//! (`split_style_runs`).

use std::ops::Range;

use triergon_core::{Budget, BudgetVec, Result, WorkBudget};
use triergon_raster::pixels_26_6;

use super::fonts::{FontId, Library};
use super::geometry::Scales;
use crate::{
    Bold, BorderStyle, Context, EventState, Glyph, GlyphState, Italic, KaraokeMark, Rgba, Style,
    Symbol,
};

/// Code points from which an `@` font draws glyphs rotated
/// (`VERTICAL_LOWER_BOUND`).
pub(super) const VERTICAL_LOWER_BOUND: u32 = 0x2f1;

/// The font size libass draws and shapes every glyph at with hinting off,
/// unless its size is 0: the glyph is then scaled to its size.
pub(super) const DRAWN_SIZE: f64 = 256.0;

/// What decorates a glyph's outline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) struct Decorations {
    pub(super) underline: bool,
    pub(super) strike_out: bool,
    /// The glyph of an `@` font, drawn rotated by 90°.
    pub(super) rotate: bool,
}

/// A drawing's commands and how they are scaled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct DrawingText<'s> {
    pub(super) text: &'s [u8],
    /// The `\p` level.
    pub(super) scale: i32,
    pub(super) baseline: i32,
}

impl DrawingText<'_> {
    /// Pixels per drawing unit at its `\p` level: 1 / 2^(level - 1), and 0
    /// where libass's shift gives no positive power of two.
    pub(super) fn pixel_scale(&self) -> f64 {
        let base = 1_i32.wrapping_shl(self.scale.wrapping_sub(1) as u32);
        if base > 0 { 1.0 / f64::from(base) } else { 0.0 }
    }
}

/// A drawing character's commands, as a range of the event text.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Drawing {
    text: Range<usize>,
    scale: i32,
    baseline: i32,
}

/// How a character is drawn: the glyph state its tags gave it, in frame
/// units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct GlyphStyle {
    pub(super) colours: [Rgba; 4],
    /// The size the outline is loaded and its run shaped at: [`DRAWN_SIZE`]
    /// for text whose size is not 0, where FreeType loads it unscaled; the
    /// frame size of a drawing.
    pub(super) size: f64,
    /// Horizontal and vertical scale, with the size's correction.
    pub(super) scale: [f64; 2],
    /// The scale the size correction divided out.
    pub(super) scale_fix: f64,
    pub(super) spacing: f64,
    /// `\fsp` in frame 26.6 units, 0 for a drawing.
    pub(super) spacing_scaled: i32,
    pub(super) bold: Bold,
    pub(super) italic: Italic,
    pub(super) decorations: Decorations,
    pub(super) border_style: BorderStyle,
    pub(super) border: [f64; 2],
    pub(super) shadow: [f64; 2],
    pub(super) blur: f64,
    pub(super) be: u8,
    pub(super) rotation: [f64; 3],
    pub(super) shear: [f64; 2],
}

impl GlyphStyle {
    fn new(
        state: &GlyphState<'_>,
        decorations: Decorations,
        drawing: bool,
        scales: &Scales,
        par: f64,
    ) -> Self {
        let colours = state.colours;
        let mut style = Self {
            colours: [
                colours.primary,
                colours.secondary,
                colours.outline,
                colours.back,
            ],
            // VSFilter scales fonts by the play resolution's height in both
            // directions.
            size: (state.font_size * scales.screen[1]).abs(),
            scale: [state.scale_x, state.scale_y],
            scale_fix: 1.0,
            spacing: state.spacing,
            spacing_scaled: 0,
            bold: state.bold,
            italic: state.italic,
            decorations,
            border_style: state.border_style,
            border: [state.border_x, state.border_y],
            shadow: [state.shadow_x, state.shadow_y],
            blur: state.blur,
            be: state.blur_edges,
            rotation: [state.rotate_x, state.rotate_y, state.rotate_z],
            shear: [state.shear_x, state.shear_y],
        };
        if !drawing {
            style.spacing_scaled =
                pixels_26_6(style.spacing * scales.screen[0] / par * style.scale[0]);
            style.fix_scaling();
        }
        style
    }

    /// `fix_glyph_scaling`: the glyph is drawn at [`DRAWN_SIZE`] and scaled to
    /// its size, so FreeType's grid fitting cannot move it.
    fn fix_scaling(&mut self) {
        if self.size == 0.0 {
            return;
        }
        let mul = self.size / DRAWN_SIZE;
        self.scale_fix = 1.0 / mul;
        self.scale[0] *= mul;
        self.scale[1] *= mul;
        self.size = DRAWN_SIZE;
    }

    /// Whether the glyph is scaled to nothing, which libass hides once its
    /// lines are laid out.
    pub(super) fn is_flat(&self) -> bool {
        self.scale[0] == 0.0 || self.scale[1] == 0.0
    }

    /// Whether `self` and `other` differ in a way that starts a style run;
    /// libass compares the values exactly.
    #[allow(clippy::float_cmp)]
    fn differs(&self, other: &Self) -> bool {
        self.size != other.size
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

/// One character of an evaluated event.
#[derive(Clone, Debug)]
pub(super) struct Char {
    /// The code point; U+FFFC for a drawing.
    pub(super) symbol: u32,
    pub(super) drawing: Option<Drawing>,
    pub(super) font: FontId,
    /// Its style in [`Text`]'s, which consecutive characters of one glyph
    /// state share.
    style: usize,
    /// The karaoke tags just before it.
    pub(super) karaoke: KaraokeMark,
    /// The event's fade alpha when it was read.
    pub(super) fade: i32,
    /// A style run starts here.
    pub(super) starts_run: bool,
}

/// An event's characters evaluated at a time: what the next stage shapes.
/// Read again for each event, into the same storage.
#[derive(Debug)]
pub(super) struct Text {
    chars: BudgetVec<Char>,
    /// The glyph states of the characters, each once per change.
    styles: BudgetVec<GlyphStyle>,
}

/// What an event's evaluation leaves besides its characters.
#[derive(Clone, Copy, Debug)]
pub(super) struct Evaluated<'s> {
    /// The event text, which drawings are ranges of.
    source: &'s [u8],
    /// The event-wide state after the text.
    pub(super) state: EventState<'s>,
    /// The glyph state after the text: its last `\fe`, border style,
    /// shadow and back colour apply to the whole event.
    pub(super) last: GlyphState<'s>,
    /// The style in force after the text: the event's, or its last `\r`
    /// style, whose margins apply where the event's are 0.
    pub(super) style: &'s Style,
}

impl<'s> Evaluated<'s> {
    /// The commands of `c`, a drawing character.
    pub(super) fn drawing(&self, c: &Char) -> Option<DrawingText<'s>> {
        let drawing = c.drawing.as_ref()?;
        Some(DrawingText {
            text: self.source.get(drawing.text.clone())?,
            scale: drawing.scale,
            baseline: drawing.baseline,
        })
    }
}

impl Text {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            chars: BudgetVec::new(budget),
            styles: BudgetVec::new(budget),
        }
    }

    pub(super) fn chars(&self) -> &[Char] {
        &self.chars
    }

    /// The glyph state of `c`.
    pub(super) fn style(&self, c: &Char) -> &GlyphStyle {
        &self.styles[c.style]
    }

    /// Reads the characters of `context`'s event, whose text is `source`
    /// (`parse_events`). `None` when a character's font has no face, which
    /// libass treats as a failed event.
    pub(super) fn read<'s>(
        &mut self,
        context: &mut Context<'s>,
        source: &'s [u8],
        library: &mut Library,
        scales: &Scales,
        par: f64,
        work: &mut WorkBudget,
    ) -> Result<Option<Evaluated<'s>>> {
        self.chars.clear();
        self.styles.clear();
        let mut last_effect = None;
        while let Some(glyph) = context.next_glyph()? {
            let desc = glyph.state.font();
            let Some(font) = library.font(&desc, work)? else {
                return Ok(None);
            };
            let (symbol, drawing) = drawing(&glyph);
            let decorations = Decorations {
                underline: glyph.state.underline,
                strike_out: glyph.state.strike_out,
                rotate: desc.vertical && symbol >= VERTICAL_LOWER_BOUND,
            };
            let style = GlyphStyle::new(&glyph.state, decorations, drawing.is_some(), scales, par);
            // The previous character's style is the last one kept: an equal
            // style, or the same font, cannot start a run.
            let same_style = self.styles.last() == Some(&style);
            let starts_run = self.chars.last().is_none_or(|previous| {
                let effect = glyph.karaoke.kind;
                let other_font = || {
                    let (a, b) = (library.get(previous.font), library.get(font));
                    a.family() != b.family() || a.vertical() != b.vertical()
                };
                glyph.karaoke.timing != 0
                    || (effect.is_some() && effect != last_effect)
                    || drawing.is_some()
                    || previous.drawing.is_some()
                    || (previous.font != font && other_font())
                    || (!same_style && self.styles[previous.style].differs(&style))
            });
            if glyph.karaoke.kind.is_some() {
                last_effect = glyph.karaoke.kind;
            }
            if !same_style {
                self.styles.try_push(style).map_err(|e| e.error)?;
            }
            let c = Char {
                symbol,
                drawing,
                font,
                style: self.styles.len() - 1,
                karaoke: glyph.karaoke,
                fade: glyph.fade,
                starts_run,
            };
            self.chars.try_push(c).map_err(|e| e.error)?;
        }
        super::fit(&mut self.chars)?;
        super::fit(&mut self.styles)?;
        Ok(Some(Evaluated {
            source,
            state: *context.state(),
            last: *context.glyph_state(),
            style: context.style(),
        }))
    }
}

/// The code point a glyph shapes as and, for a drawing, its commands as a
/// range of the event text, where the walk found them.
fn drawing(glyph: &Glyph<'_>) -> (u32, Option<Drawing>) {
    match glyph.symbol {
        Symbol::Char(code) => (code, None),
        Symbol::Drawing {
            text,
            scale,
            baseline,
        } => (
            0xfffc,
            Some(Drawing {
                text: glyph.offset..glyph.offset + text.len(),
                scale,
                baseline,
            }),
        ),
    }
}
