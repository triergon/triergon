//! The third stage: a shaped event laid out as libass does. Every glyph's
//! outline and metrics (`retrieve_glyphs`), the pen positions on one line
//! (`preliminary_layout`), the lines ([`super::wrap`]), karaoke timing,
//! the visual order (`reorder_text`), alignment (`align_lines`), the text
//! box (`compute_string_bbox`) and the baseline shear
//! (`apply_baseline_shear`).

use std::sync::Arc;

use triergon_core::{BudgetVec, Result, WorkBudget, round_ties_even_i32};
use triergon_raster::{Rect, Vector, pixels_26_6};

use super::fonts::Library;
use super::geometry::Scales;
use super::outlines::{GlyphKey, OutlineValue, Outlines};
use super::shaping::{Bidi, Shaped, Shaping};
use super::text::{Char, Text};
use super::wrap::{self, Lines, WrapChar, Wrapping, fmax};
use crate::{
    EventState, HAlign, Justify, Karaoke, KaraokeGlyph, KaraokeMark, Limits, Margins, Rgba,
    Syllables,
};

/// `d6_to_double`: 26.6 to pixels.
pub(super) fn d6_to_double(x: i32) -> f64 {
    f64::from(x) / 64.0
}

/// An outline and how it is scaled to the frame.
#[derive(Clone, Debug)]
pub(super) struct ScaledOutline {
    pub(super) value: Arc<OutlineValue>,
    /// From outline units to 26.6.
    pub(super) scale: [f64; 2],
    /// Added after the scale, 26.6: a drawing's baseline offset.
    pub(super) offset: [f64; 2],
}

/// A glyph laid out.
#[derive(Clone, Debug)]
pub(super) struct LaidGlyph {
    /// What it draws; `None` when its outline cannot be built.
    pub(super) outline: Option<ScaledOutline>,
    pub(super) bbox: Rect,
    pub(super) asc: i32,
    pub(super) desc: i32,
    /// Shaping's offset from the pen.
    pub(super) offset: Vector,
    /// Shaping's advance, or a drawing's width.
    pub(super) advance: Vector,
    /// Where it is drawn, 26.6 pre-aspect pixels from the first line's
    /// baseline at the text's left.
    pub(super) pos: Vector,
}

/// A character's karaoke at the event's time: libass's `effect_type` and
/// `effect_timing` after `ass_process_karaoke_effects`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Timing {
    pub(super) effect: Option<Karaoke>,
    /// For a syllable, its boundary from the character's pen, 26.6.
    pub(super) timing: i32,
    /// The primary and secondary colours trade places.
    pub(super) swap_colours: bool,
}

impl Timing {
    /// The timing of a character outside every syllable: its tags'.
    pub(super) fn untimed(mark: KaraokeMark) -> Self {
        Self {
            effect: mark.kind,
            timing: mark.timing,
            swap_colours: false,
        }
    }

    /// `colours`, the primary and secondary traded when they trade places.
    pub(super) fn colours(&self, mut colours: [Rgba; 4]) -> [Rgba; 4] {
        if self.swap_colours {
            colours.swap(0, 1);
        }
        colours
    }
}

/// A character laid out.
#[derive(Clone, Copy, Debug)]
pub(super) struct LaidChar {
    /// Its cluster's advance, with the italic correction and letter spacing.
    pub(super) advance: Vector,
    /// Shaping gave it a glyph and trimming kept it: it takes space on its
    /// line.
    pub(super) shown: bool,
    /// Shown, and not scaled to nothing: it is drawn.
    pub(super) drawn: bool,
    /// A run starts here: a style run, or after a line's leading whitespace.
    pub(super) starts_run: bool,
    pub(super) karaoke: Timing,
}

/// A laid-out event: what the next stage places in the frame.
#[derive(Debug)]
pub(super) struct Layout {
    /// Each character's layout, in text order.
    pub(super) chars: BudgetVec<LaidChar>,
    /// Each glyph's, as [`Shaped::glyphs`] holds them.
    pub(super) glyphs: BudgetVec<LaidGlyph>,
    pub(super) lines: Lines,
    /// The text box in pre-aspect pixels.
    pub(super) text_box: TextBox,
}

/// The text box in pre-aspect pixels.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct TextBox {
    pub(super) x_min: f64,
    pub(super) y_min: f64,
    pub(super) x_max: f64,
    pub(super) y_max: f64,
}

/// Where glyphs come from: fonts, outlines and the script's limits.
#[derive(Debug)]
pub(super) struct Sources<'a> {
    pub(super) library: &'a Library,
    pub(super) outlines: &'a mut Outlines,
    pub(super) limits: &'a Limits,
}

/// What an event is laid out in.
#[derive(Clone, Copy, Debug)]
pub(super) struct Room<'a> {
    pub(super) scales: &'a Scales,
    pub(super) par: f64,
    /// The event's margins, else its style's.
    pub(super) margins: Margins,
    /// The width lines wrap at and are aligned in, in pixels.
    pub(super) max_width: f64,
    pub(super) line_spacing: f64,
    /// Milliseconds since the event started.
    pub(super) elapsed: i64,
}

impl Layout {
    /// Lays out `text` as shaped, from its outlines to the baseline shear.
    pub(super) fn new(
        text: &Text<'_>,
        shaped: &Shaped,
        bidi: Bidi,
        shaping: &Shaping<'_>,
        sources: Sources<'_>,
        room: &Room<'_>,
        work: &mut WorkBudget,
    ) -> Result<Self> {
        let budget = text.chars.budget();
        let glyphs = measure_glyphs(text, shaped, sources, room, work)?;
        let advances = cluster_advances(text, shaped, &glyphs)?;
        let pens = pen_positions(&advances)?;
        let wrapping = Wrapping {
            max_width: room.max_width,
            style: text.state.wrap_style,
            line_spacing: room.line_spacing,
            scales: room.scales,
        };
        let lines = wrap::wrap(
            &wrap_chars(text, shaped, &glyphs, &pens)?,
            &wrapping,
            budget,
            work,
        )?;
        let chars = lay_out_chars(
            text,
            shaped,
            &glyphs,
            &lines,
            &advances,
            &pens,
            room.elapsed,
        )?;
        let order = bidi.visual_order(text, &lines.lines, |i| chars[i].starts_run, shaping)?;
        let mut layout = Self {
            chars,
            glyphs,
            lines,
            text_box: TextBox::default(),
        };
        layout.reorder(shaped, &order, room.line_spacing);
        layout.align(text, shaped, alignment(&text.state), room.max_width);
        layout.text_box = layout.measure_text_box(shaped);
        layout.shear(text, shaped, &order);
        Ok(layout)
    }

    /// `reorder_text`: the glyphs of shown characters placed in visual
    /// order, line by line.
    fn reorder(&mut self, shaped: &Shaped, order: &[usize], line_spacing: f64) {
        let mut pen = Vector::default();
        for (k, line) in self.lines.lines.iter().enumerate() {
            if k > 0 {
                let previous = &self.lines.lines[k - 1];
                pen.x = 0;
                pen.y = pen
                    .y
                    .wrapping_add(pixels_26_6(previous.desc))
                    .wrapping_add(pixels_26_6(line.asc))
                    .wrapping_add(pixels_26_6(line_spacing));
            }
            for &i in &order[line.chars.clone()] {
                let laid = &self.chars[i];
                if !laid.shown {
                    continue;
                }
                let mut cluster = pen;
                pen.x = pen.x.wrapping_add(laid.advance.x);
                pen.y = pen.y.wrapping_add(laid.advance.y);
                for glyph in &mut self.glyphs.as_mut_slice()[shaped.clusters[i].glyphs.clone()] {
                    glyph.pos = Vector::new(
                        glyph.offset.x.wrapping_add(cluster.x),
                        glyph.offset.y.wrapping_add(cluster.y),
                    );
                    cluster.x = cluster.x.wrapping_add(glyph.advance.x);
                    cluster.y = cluster.y.wrapping_add(glyph.advance.y);
                }
            }
        }
    }

    /// The width in pixels of the characters of `range` that alignment
    /// counts: shown ones other than `\n` and drawings' placeholders.
    fn line_width(&self, text: &Text<'_>, range: std::ops::Range<usize>) -> f64 {
        let mut width = 0.0;
        for i in range {
            let symbol = text.chars[i].symbol;
            if self.chars[i].shown && symbol != u32::from(b'\n') && symbol != 0 {
                width += d6_to_double(self.chars[i].advance.x);
            }
        }
        width
    }

    /// `align_lines`: each line shifted for its alignment and justification.
    fn align(
        &mut self,
        text: &Text<'_>,
        shaped: &Shaped,
        (halign, justify): (HAlign, Justify),
        max_text_width: f64,
    ) {
        let max_width = self.lines.lines.iter().fold(0.0, |max, line| {
            fmax(max, self.line_width(text, line.chars.clone()))
        });
        for line in self.lines.lines.iter() {
            let width = self.line_width(text, line.chars.clone());
            let shift = match halign {
                HAlign::Left => match justify {
                    Justify::Right => max_width - width,
                    Justify::Center => (max_width - width) / 2.0,
                    _ => 0.0,
                },
                HAlign::Right => match justify {
                    Justify::Left => max_text_width - max_width,
                    Justify::Center => max_text_width - max_width + (max_width - width) / 2.0,
                    _ => max_text_width - width,
                },
                HAlign::Center => match justify {
                    Justify::Left => (max_text_width - max_width) / 2.0,
                    Justify::Right => (max_text_width - max_width) / 2.0 + max_width - width,
                    _ => (max_text_width - width) / 2.0,
                },
                HAlign::None => 0.0,
            };
            let shift = pixels_26_6(shift);
            let first = shaped.clusters[line.chars.start].glyphs.start;
            let end = shaped.clusters[line.chars.end - 1].glyphs.end;
            for glyph in &mut self.glyphs.as_mut_slice()[first..end] {
                glyph.pos.x = glyph.pos.x.wrapping_add(shift);
            }
        }
    }

    /// `compute_string_bbox`: the shown characters' extent, and the lines'.
    fn measure_text_box(&self, shaped: &Shaped) -> TextBox {
        let y_min = -self.lines.first().asc;
        let mut b = TextBox {
            x_min: 32000.0,
            x_max: -32000.0,
            y_min,
            y_max: y_min + self.lines.height,
        };
        for (laid, cluster) in self.chars.iter().zip(shaped.clusters.iter()) {
            if laid.shown {
                let s = d6_to_double(self.glyphs[cluster.glyphs.start].pos.x);
                let e = s + d6_to_double(laid.advance.x);
                b.x_min = b.x_min.min(s);
                b.x_max = b.x_max.max(e);
            }
        }
        b
    }

    /// `apply_baseline_shear`: `\fay` raises each glyph by the advance before
    /// it on its line or run, in visual order. libass resets at a run only
    /// without the track's whole-text layout feature, which is never set
    /// here.
    fn shear(&mut self, text: &Text<'_>, shaped: &Shaped, order: &[usize]) {
        for line in self.lines.lines.iter() {
            let mut shear: i32 = 0;
            for i in line.chars.clone() {
                if self.chars[i].starts_run {
                    shear = 0;
                }
                let at = order[i];
                if !self.chars[at].drawn {
                    continue;
                }
                let style = &text.chars[at].style;
                let fay = style.shear[1] / style.scale[0] * style.scale[1];
                for glyph in &mut self.glyphs.as_mut_slice()[shaped.clusters[at].glyphs.clone()] {
                    // C's conversions of the double sums back to the int fields.
                    glyph.pos.y = (f64::from(glyph.pos.y)
                        + (f64::from(shear) + fay * f64::from(glyph.offset.x)))
                        as i32;
                    shear = (f64::from(shear) + fay * f64::from(glyph.advance.x)) as i32;
                }
            }
        }
    }
}

/// What wrapping reads of each character: its own glyph's extent at its pen
/// position on one unbroken line, its metrics and its border.
fn wrap_chars(
    text: &Text<'_>,
    shaped: &Shaped,
    glyphs: &[LaidGlyph],
    pens: &[i32],
) -> Result<BudgetVec<WrapChar>> {
    let mut chars = BudgetVec::new(text.chars.budget());
    chars.try_reserve_exact(text.chars.len())?;
    for ((c, cluster), &pen) in text.chars.iter().zip(&*shaped.clusters).zip(pens) {
        let own = &glyphs[cluster.glyphs.start];
        chars
            .try_push(WrapChar {
                symbol: c.symbol,
                left: own.bbox.x_min.wrapping_add(pen),
                right: own.bbox.x_max.wrapping_add(pen),
                asc: own.asc,
                desc: own.desc,
                border: c.style.border,
            })
            .map_err(|e| e.error)?;
    }
    Ok(chars)
}

/// Each character's place in its line (shown, drawn, starting a run), then
/// timed by `ass_process_karaoke_effects` over its pen position on one
/// unbroken line.
fn lay_out_chars(
    text: &Text<'_>,
    shaped: &Shaped,
    glyphs: &[LaidGlyph],
    lines: &Lines,
    advances: &[Vector],
    pens: &[i32],
    elapsed: i64,
) -> Result<BudgetVec<LaidChar>> {
    let budget = text.chars.budget();
    let mut chars = BudgetVec::new(budget);
    chars.try_reserve_exact(text.chars.len())?;
    // A run also starts where a line's leading whitespace ends: within the
    // line, or at the next one's start for a line of whitespace.
    let mut previous_shown = None;
    for line in lines.lines.iter() {
        for i in line.chars.clone() {
            let c = &text.chars[i];
            let shown = shaped.clusters[i].visible && line.shown.contains(&i);
            chars
                .try_push(LaidChar {
                    advance: advances[i],
                    shown,
                    drawn: shown && !c.style.is_flat(),
                    starts_run: c.starts_run || line.shown.start == i || previous_shown == Some(i),
                    karaoke: Timing::untimed(c.karaoke),
                })
                .map_err(|e| e.error)?;
        }
        previous_shown = Some(line.shown.start);
    }
    let mut karaoke = BudgetVec::new(budget);
    karaoke.try_reserve_exact(chars.len())?;
    for (i, (c, laid)) in text.chars.iter().zip(chars.iter()).enumerate() {
        karaoke
            .try_push(KaraokeGlyph {
                starts_new_run: laid.starts_run,
                mark: c.karaoke,
                skip: !laid.shown,
                x: pens[i],
                advance: glyphs[shaped.clusters[i].glyphs.start].advance.x,
                rotate_z: c.style.rotation[2],
            })
            .map_err(|e| e.error)?;
    }
    for syllable in Syllables::new(&karaoke, elapsed) {
        for i in syllable.glyphs.clone() {
            chars.as_mut_slice()[i].karaoke = Timing {
                effect: Some(syllable.effect),
                timing: syllable.timing(&karaoke[i]),
                swap_colours: syllable.swap_colours,
            };
        }
    }
    Ok(chars)
}

/// The banner of a horizontal scroll starts its lines at the left and
/// justifies them as aligned; other events align as their state says.
fn alignment(state: &EventState<'_>) -> (HAlign, Justify) {
    if state.scroll.is_some_and(|s| s.is_horizontal()) {
        let justify = match state.alignment.horizontal {
            HAlign::Left => Justify::Left,
            HAlign::Center => Justify::Center,
            HAlign::Right => Justify::Right,
            HAlign::None => Justify::Auto,
        };
        (HAlign::Left, justify)
    } else {
        (state.alignment.horizontal, state.justify)
    }
}

/// Each cluster's advance: as shaped, or a drawing's width; widened after
/// italic text that gives way to upright text to the box of the last glyph
/// with a width (`retrieve_glyphs`'s italic correction); and letter spacing.
fn cluster_advances(
    text: &Text<'_>,
    shaped: &Shaped,
    glyphs: &[LaidGlyph],
) -> Result<BudgetVec<Vector>> {
    let chars = text.chars.as_slice();
    let own = |i: usize| &glyphs[shaped.clusters[i].glyphs.start];
    let italic = |i: usize| chars[i].style.italic.0 != 0;
    let mut advances = BudgetVec::new(text.chars.budget());
    advances.try_reserve_exact(chars.len())?;
    for (i, (c, cluster)) in chars.iter().zip(shaped.clusters.iter()).enumerate() {
        let advance = if c.drawing.is_some() {
            own(i).advance
        } else {
            cluster.advance
        };
        advances.try_push(advance).map_err(|e| e.error)?;
    }
    let advances_mut = advances.as_mut_slice();
    for i in 0..chars.len() {
        if i > 0 && italic(i - 1) && !italic(i) {
            let mut back = i - 1;
            while back > 0 && own(back).bbox.x_max - own(back).bbox.x_min == 0 && italic(back) {
                back -= 1;
            }
            let x_max = own(back).bbox.x_max;
            if x_max > advances_mut[back].x {
                advances_mut[back].x = x_max;
            }
        }
        advances_mut[i].x = advances_mut[i]
            .x
            .wrapping_add(chars[i].style.spacing_scaled);
    }
    Ok(advances)
}

/// `preliminary_layout`: each character's pen position on one unbroken
/// line, 26.6.
fn pen_positions(advances: &BudgetVec<Vector>) -> Result<BudgetVec<i32>> {
    let mut pens = BudgetVec::new(advances.budget());
    pens.try_reserve_exact(advances.len())?;
    let mut pen: i32 = 0;
    for advance in advances.iter() {
        pens.try_push(pen).map_err(|e| e.error)?;
        pen = pen.wrapping_add(advance.x);
    }
    Ok(pens)
}

/// `retrieve_glyphs`: every glyph's outline, box, advance (for drawings) and
/// ascent and descent at its scale, cluster by cluster.
fn measure_glyphs(
    text: &Text<'_>,
    shaped: &Shaped,
    mut sources: Sources<'_>,
    room: &Room<'_>,
    work: &mut WorkBudget,
) -> Result<BudgetVec<LaidGlyph>> {
    let mut glyphs = BudgetVec::new(text.chars.budget());
    glyphs.try_reserve_exact(shaped.glyphs.len())?;
    for (c, cluster) in text.chars.iter().zip(shaped.clusters.iter()) {
        for glyph in shaped.glyphs(cluster) {
            let mut laid = LaidGlyph {
                outline: None,
                bbox: Rect::new(0, 0, 0, 0),
                asc: 0,
                desc: 0,
                offset: glyph.offset,
                advance: glyph.advance,
                pos: Vector::default(),
            };
            if let Some((outline, asc, desc)) =
                outline(c, cluster.face, glyph.id, &mut sources, room, work)?
            {
                let cbox = *outline.value.shape.cbox();
                let ([sx, sy], offset_y) = (outline.scale, outline.offset[1]);
                laid.bbox = Rect::new(
                    round_ties_even_i32(f64::from(cbox.x_min) * sx),
                    round_ties_even_i32(f64::from(cbox.y_min) * sy + offset_y),
                    round_ties_even_i32(f64::from(cbox.x_max) * sx),
                    round_ties_even_i32(f64::from(cbox.y_max) * sy + offset_y),
                );
                if c.drawing.is_some() {
                    laid.advance = Vector::new(
                        round_ties_even_i32(f64::from(outline.value.advance) * sx),
                        0,
                    );
                }
                laid.asc = round_ties_even_i32(f64::from(asc) * sy);
                laid.desc = round_ties_even_i32(f64::from(desc) * sy);
                laid.outline = Some(outline);
            }
            glyphs.try_push(laid).map_err(|e| e.error)?;
        }
    }
    Ok(glyphs)
}

/// The outline of glyph `id` of `c`, or of its drawing, scaled, with the
/// unscaled ascent and descent; `None` when it cannot be built.
fn outline(
    c: &Char<'_>,
    face: usize,
    id: triergon_font::GlyphId,
    sources: &mut Sources<'_>,
    room: &Room<'_>,
    work: &mut WorkBudget,
) -> Result<Option<(ScaledOutline, i32, i32)>> {
    let style = &c.style;
    if let Some(drawing) = c.drawing {
        let Some(value) = sources.outlines.drawing(&drawing, sources.limits, work)? else {
            return Ok(None);
        };
        let base = 1_i32.wrapping_shl(drawing.scale.wrapping_sub(1) as u32);
        let w = if base > 0 { 1.0 / f64::from(base) } else { 0.0 };
        let scale = [
            style.scale[0] * w * room.scales.screen[0] / room.par,
            style.scale[1] * w * room.scales.screen[1],
        ];
        let desc = 64_i32.wrapping_mul(drawing.baseline);
        let asc = value.asc.wrapping_sub(desc);
        let offset = [0.0, -f64::from(asc) * scale[1]];
        return Ok(Some((
            ScaledOutline {
                value,
                scale,
                offset,
            },
            asc,
            desc,
        )));
    }
    let key = GlyphKey {
        font: c.font,
        face,
        size: style.size.to_bits(),
        glyph: id,
        bold: style.bold,
        italic: style.italic,
        decorations: style.decorations,
    };
    let font = sources.library.get(c.font);
    Ok(sources.outlines.glyph(key, font, work)?.map(|value| {
        let (asc, desc) = (value.asc, value.desc);
        (
            ScaledOutline {
                value,
                scale: style.scale,
                offset: [0.0; 2],
            },
            asc,
            desc,
        )
    }))
}
