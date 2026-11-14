//! The third stage: a shaped event laid out as libass does. Every glyph's
//! outline and metrics (`retrieve_glyphs`), the pen positions on one line
//! (`preliminary_layout`), the lines ([`super::wrap`]), karaoke timing, the
//! visual order (`reorder_text`), alignment (`align_lines`), the text box
//! (`compute_string_bbox`) and the baseline shear (`apply_baseline_shear`).

use triergon_core::{Budget, BudgetVec, Result, WorkBudget, round_ties_even_i32};
use triergon_raster::{Vector, pixels_26_6};
use triergon_shape::bidi::Level;

use super::fonts::Library;
use super::geometry::{Geometry, Scales};
use super::outlines::{GlyphKey, KeptOutline, Outlines};
use super::shaping::{Shaped, ShapedGlyph, Shaping};
use super::text::{Char, Evaluated, Text};
use super::units::{d6_to_double, fmax};
use super::wrap::{Lines, WrapText, Wrapping};
use crate::{
    EventState, HAlign, Justify, Karaoke, KaraokeGlyph, KaraokeMark, Limits, Margins, Rgba,
    Syllables,
};

/// An outline and how it is scaled to the frame.
#[derive(Clone, Debug)]
pub(super) struct ScaledOutline {
    pub(super) value: KeptOutline,
    /// From outline units to 26.6.
    pub(super) scale: [f64; 2],
    /// Added to y after the scale, 26.6: a drawing's baseline offset.
    pub(super) offset_y: f64,
}

/// A glyph laid out.
#[derive(Clone, Debug)]
pub(super) struct LaidGlyph {
    /// What it draws; `None` when its outline cannot be built.
    pub(super) outline: Option<ScaledOutline>,
    /// The left and right of its box, 26.6.
    pub(super) x_min: i32,
    pub(super) x_max: i32,
    pub(super) asc: i32,
    pub(super) desc: i32,
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
    /// Shaping gave it a glyph and trimming kept it: it takes space on its
    /// line.
    pub(super) shown: bool,
    /// Shown, and not scaled to nothing: it is drawn.
    pub(super) drawn: bool,
    /// A run starts here: a style run, or after a line's leading whitespace.
    pub(super) starts_run: bool,
    pub(super) karaoke: Timing,
}

/// The text box in pre-aspect pixels.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct TextBox {
    pub(super) x_min: f64,
    pub(super) y_min: f64,
    pub(super) x_max: f64,
    pub(super) y_max: f64,
}

/// What layout reads: an evaluated event and its shaping.
#[derive(Clone, Copy, Debug)]
pub(super) struct Input<'a, 's> {
    pub(super) text: &'a Text,
    pub(super) evaluated: &'a Evaluated<'s>,
    pub(super) shaped: &'a Shaped,
    pub(super) shaping: &'a Shaping<'s>,
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
    pub(super) geometry: &'a Geometry<'a>,
    pub(super) scales: &'a Scales,
    /// The event's margins, else its style's.
    pub(super) margins: Margins,
    /// Milliseconds since the event started.
    pub(super) elapsed: i64,
}

/// A laid-out event: what the next stage places in the frame. Laid out
/// again for each event, into the same storage.
#[derive(Debug)]
pub(super) struct Layout {
    /// Each character's layout, in text order.
    pub(super) chars: BudgetVec<LaidChar>,
    /// Each glyph's, as [`Shaped::glyphs`] holds them.
    pub(super) glyphs: BudgetVec<LaidGlyph>,
    pub(super) lines: Lines,
    /// The text box in pre-aspect pixels.
    pub(super) text_box: TextBox,
    /// Each cluster's advance, with the italic correction and letter
    /// spacing.
    advances: BudgetVec<Vector>,
    /// Each character's pen position on one unbroken line, 26.6.
    pens: BudgetVec<i32>,
    karaoke: BudgetVec<KaraokeGlyph>,
    levels: BudgetVec<Level>,
    /// The character shown at each position.
    order: BudgetVec<u32>,
}

impl Layout {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            chars: BudgetVec::new(budget),
            glyphs: BudgetVec::new(budget),
            lines: Lines::new(budget),
            text_box: TextBox::default(),
            advances: BudgetVec::new(budget),
            pens: BudgetVec::new(budget),
            karaoke: BudgetVec::new(budget),
            levels: BudgetVec::new(budget),
            order: BudgetVec::new(budget),
        }
    }

    /// Lets go of the last event's outlines.
    pub(super) fn release(&mut self) {
        self.glyphs.clear();
    }

    /// Lays out `input`, from its outlines to the baseline shear.
    pub(super) fn lay_out(
        &mut self,
        input: &Input<'_, '_>,
        sources: Sources<'_>,
        room: &Room<'_>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let Input {
            text,
            evaluated,
            shaped,
            shaping,
        } = *input;
        self.measure_glyphs(input, sources, room, work)?;
        self.measure_clusters(text, shaped)?;
        let state = &evaluated.state;
        let max_width = room.geometry.text_width(room.margins, state.explicit);
        let line_spacing = room.geometry.canvas.settings.line_spacing;
        let wrapping = Wrapping {
            max_width,
            style: state.wrap_style,
            line_spacing,
            scales: room.scales,
        };
        let unbroken = Unbroken {
            text,
            shaped,
            glyphs: &self.glyphs,
            pens: &self.pens,
        };
        self.lines.wrap(&unbroken, &wrapping, work)?;
        self.lay_out_chars(text, shaped, room.elapsed)?;
        let chars = &self.chars;
        shaped.bidi.visual_order(
            text,
            &self.lines.lines,
            |i| chars[i].starts_run,
            shaping,
            &mut self.levels,
            &mut self.order,
        )?;
        self.reorder(shaped, line_spacing);
        self.align(text, shaped, alignment(state), max_width);
        self.text_box = self.measure_text_box(shaped);
        self.shear(text, shaped);
        self.fit()
    }

    /// Gives back what the stores hold beyond this event's needs.
    fn fit(&mut self) -> Result<()> {
        super::fit(&mut self.chars)?;
        super::fit(&mut self.glyphs)?;
        super::fit(&mut self.advances)?;
        super::fit(&mut self.pens)?;
        super::fit(&mut self.karaoke)?;
        super::fit(&mut self.levels)?;
        super::fit(&mut self.order)?;
        self.lines.fit()
    }

    /// `retrieve_glyphs`: every glyph's outline, box, advance (for drawings)
    /// and ascent and descent at its scale, cluster by cluster.
    fn measure_glyphs(
        &mut self,
        input: &Input<'_, '_>,
        mut sources: Sources<'_>,
        room: &Room<'_>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let (text, shaped) = (input.text, input.shaped);
        self.glyphs.clear();
        self.glyphs.try_reserve_exact(shaped.glyphs.len())?;
        for (c, cluster) in text.chars().iter().zip(&*shaped.clusters) {
            for glyph in shaped.glyphs(cluster) {
                let face = usize::from(cluster.face);
                let laid = measure_glyph(input, c, face, glyph, &mut sources, room, work)?;
                self.glyphs.try_push(laid).map_err(|e| e.error)?;
            }
        }
        Ok(())
    }

    /// Each cluster's advance: as shaped, or a drawing's width; widened
    /// after italic text that gives way to upright text to the box of the
    /// last glyph with a width (`retrieve_glyphs`'s italic correction); and
    /// letter spacing. Then each character's pen position on one unbroken
    /// line (`preliminary_layout`).
    fn measure_clusters(&mut self, text: &Text, shaped: &Shaped) -> Result<()> {
        let chars = text.chars();
        let own = |i: usize| &self.glyphs[shaped.clusters[i].glyphs.start];
        let italic = |i: usize| text.style(&chars[i]).italic.0 != 0;
        self.advances.clear();
        self.advances.try_reserve_exact(chars.len())?;
        for (i, (c, cluster)) in chars.iter().zip(&*shaped.clusters).enumerate() {
            let advance = if c.drawing.is_some() {
                own(i).advance
            } else {
                cluster.advance
            };
            self.advances.try_push(advance).map_err(|e| e.error)?;
        }
        let advances = self.advances.as_mut_slice();
        for i in 0..chars.len() {
            if i > 0 && italic(i - 1) && !italic(i) {
                let mut back = i - 1;
                while back > 0 && own(back).x_max - own(back).x_min == 0 && italic(back) {
                    back -= 1;
                }
                let x_max = own(back).x_max;
                if x_max > advances[back].x {
                    advances[back].x = x_max;
                }
            }
            advances[i].x = advances[i]
                .x
                .wrapping_add(text.style(&chars[i]).spacing_scaled);
        }
        self.pens.clear();
        self.pens.try_reserve_exact(chars.len())?;
        let mut pen: i32 = 0;
        for advance in self.advances.iter() {
            self.pens.try_push(pen).map_err(|e| e.error)?;
            pen = pen.wrapping_add(advance.x);
        }
        Ok(())
    }

    /// Each character's place in its line (shown, drawn, starting a run)
    /// and its karaoke, timed by `ass_process_karaoke_effects` over its pen
    /// position on one unbroken line.
    fn lay_out_chars(&mut self, text: &Text, shaped: &Shaped, elapsed: i64) -> Result<()> {
        let chars = text.chars();
        self.karaoke.clear();
        self.karaoke.try_reserve_exact(chars.len())?;
        // A run also starts where a line's leading whitespace ends: within
        // the line, or at the next one's start for a line of whitespace.
        let mut previous_shown = None;
        for line in self.lines.lines.iter() {
            for i in line.chars.clone() {
                let c = &chars[i];
                self.karaoke
                    .try_push(KaraokeGlyph {
                        starts_new_run: c.starts_run
                            || line.shown.start == i
                            || previous_shown == Some(i),
                        mark: c.karaoke,
                        skip: !(shaped.clusters[i].visible && line.shown.contains(&i)),
                        x: self.pens[i],
                        advance: self.glyphs[shaped.clusters[i].glyphs.start].advance.x,
                        rotate_z: text.style(c).rotation[2],
                    })
                    .map_err(|e| e.error)?;
            }
            previous_shown = Some(line.shown.start);
        }
        self.chars.clear();
        self.chars.try_reserve_exact(chars.len())?;
        let mut syllables = Syllables::new(&self.karaoke, elapsed).peekable();
        for (i, (c, k)) in chars.iter().zip(self.karaoke.iter()).enumerate() {
            while syllables.next_if(|s| s.glyphs.end <= i).is_some() {}
            let karaoke = match syllables.peek() {
                Some(s) if s.glyphs.contains(&i) => Timing {
                    effect: Some(s.effect),
                    timing: s.timing(k),
                    swap_colours: s.swap_colours,
                },
                _ => Timing::untimed(c.karaoke),
            };
            let shown = !k.skip;
            self.chars
                .try_push(LaidChar {
                    shown,
                    drawn: shown && !text.style(c).is_flat(),
                    starts_run: k.starts_new_run,
                    karaoke,
                })
                .map_err(|e| e.error)?;
        }
        Ok(())
    }

    /// `reorder_text`: the glyphs of shown characters placed in visual
    /// order, line by line.
    fn reorder(&mut self, shaped: &Shaped, line_spacing: f64) {
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
            for &i in &self.order[line.chars.clone()] {
                let i = i as usize;
                if !self.chars[i].shown {
                    continue;
                }
                let mut cluster = pen;
                let advance = self.advances[i];
                pen.x = pen.x.wrapping_add(advance.x);
                pen.y = pen.y.wrapping_add(advance.y);
                let range = shaped.clusters[i].glyphs.clone();
                for (laid, shaped) in self.glyphs[range.clone()]
                    .iter_mut()
                    .zip(&shaped.glyphs[range])
                {
                    laid.pos = Vector::new(
                        shaped.offset.x.wrapping_add(cluster.x),
                        shaped.offset.y.wrapping_add(cluster.y),
                    );
                    cluster.x = cluster.x.wrapping_add(laid.advance.x);
                    cluster.y = cluster.y.wrapping_add(laid.advance.y);
                }
            }
        }
    }

    /// The width in pixels of the characters of `range` that alignment
    /// counts: shown ones other than `\n` and drawings' placeholders.
    fn line_width(&self, text: &Text, range: std::ops::Range<usize>) -> f64 {
        let mut width = 0.0;
        for i in range {
            let symbol = text.chars()[i].symbol;
            if self.chars[i].shown && symbol != u32::from(b'\n') && symbol != 0 {
                width += d6_to_double(self.advances[i].x);
            }
        }
        width
    }

    /// `align_lines`: each line shifted for its alignment and justification.
    fn align(
        &mut self,
        text: &Text,
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
        let (mut x_min, mut x_max) = (32000.0_f64, -32000.0_f64);
        for ((laid, cluster), advance) in self
            .chars
            .iter()
            .zip(&*shaped.clusters)
            .zip(&*self.advances)
        {
            if laid.shown {
                let s = d6_to_double(self.glyphs[cluster.glyphs.start].pos.x);
                x_min = x_min.min(s);
                x_max = x_max.max(s + d6_to_double(advance.x));
            }
        }
        TextBox {
            x_min,
            y_min,
            x_max,
            y_max: y_min + self.lines.height,
        }
    }

    /// `apply_baseline_shear`: `\fay` raises each glyph by the advance before
    /// it on its line or run, in visual order. libass resets at a run only
    /// without the track's whole-text layout feature, which is never set
    /// here.
    fn shear(&mut self, text: &Text, shaped: &Shaped) {
        for line in self.lines.lines.iter() {
            let mut shear: i32 = 0;
            for i in line.chars.clone() {
                if self.chars[i].starts_run {
                    shear = 0;
                }
                let at = self.order[i] as usize;
                if !self.chars[at].drawn {
                    continue;
                }
                let style = text.style(&text.chars()[at]);
                let fay = style.shear[1] / style.scale[0] * style.scale[1];
                let range = shaped.clusters[at].glyphs.clone();
                for (laid, shaped) in self.glyphs[range.clone()]
                    .iter_mut()
                    .zip(&shaped.glyphs[range])
                {
                    // C's conversions of the double sums back to the int fields.
                    laid.pos.y = (f64::from(laid.pos.y)
                        + (f64::from(shear) + fay * f64::from(shaped.offset.x)))
                        as i32;
                    shear = (f64::from(shear) + fay * f64::from(laid.advance.x)) as i32;
                }
            }
        }
    }
}

/// An event's characters on one unbroken line, as wrapping reads them.
struct Unbroken<'a> {
    text: &'a Text,
    shaped: &'a Shaped,
    glyphs: &'a [LaidGlyph],
    pens: &'a [i32],
}

impl Unbroken<'_> {
    /// Character `i`'s own glyph: the first of its cluster.
    fn own(&self, i: usize) -> &LaidGlyph {
        &self.glyphs[self.shaped.clusters[i].glyphs.start]
    }
}

impl WrapText for Unbroken<'_> {
    fn len(&self) -> usize {
        self.pens.len()
    }

    fn symbol(&self, i: usize) -> u32 {
        self.text.chars()[i].symbol
    }

    fn extent(&self, i: usize) -> (i32, i32) {
        let (own, pen) = (self.own(i), self.pens[i]);
        (own.x_min.wrapping_add(pen), own.x_max.wrapping_add(pen))
    }

    fn metrics(&self, i: usize) -> (i32, i32, [f64; 2]) {
        let own = self.own(i);
        let border = self.text.style(&self.text.chars()[i]).border;
        (own.asc, own.desc, border)
    }
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

/// `glyph` of `c`, or its drawing, with its outline scaled and its box,
/// ascent and descent at that scale; a drawing's advance is its width.
/// Without an outline it has neither, and shaping's advance.
fn measure_glyph(
    input: &Input<'_, '_>,
    c: &Char,
    face: usize,
    glyph: &ShapedGlyph,
    sources: &mut Sources<'_>,
    room: &Room<'_>,
    work: &mut WorkBudget,
) -> Result<LaidGlyph> {
    let style = input.text.style(c);
    let unmeasured = LaidGlyph {
        outline: None,
        x_min: 0,
        x_max: 0,
        asc: 0,
        desc: 0,
        advance: glyph.advance,
        pos: Vector::default(),
    };
    let (outline, asc, desc, advance) = if let Some(drawing) = input.evaluated.drawing(c) {
        let Some(value) = sources.outlines.drawing(&drawing, sources.limits, work)? else {
            return Ok(unmeasured);
        };
        let w = drawing.pixel_scale();
        let scale = [
            style.scale[0] * w * room.scales.screen[0] / room.geometry.par,
            style.scale[1] * w * room.scales.screen[1],
        ];
        let desc = 64_i32.wrapping_mul(drawing.baseline);
        let asc = value.asc.wrapping_sub(desc);
        let advance = Vector::new(round_ties_even_i32(f64::from(value.advance) * scale[0]), 0);
        let offset_y = -f64::from(asc) * scale[1];
        let outline = ScaledOutline {
            value,
            scale,
            offset_y,
        };
        (outline, asc, desc, advance)
    } else {
        let key = GlyphKey {
            font: c.font,
            face,
            size: style.size.to_bits(),
            glyph: glyph.id,
            bold: style.bold,
            italic: style.italic,
            decorations: style.decorations,
        };
        let font = sources.library.get(c.font);
        let Some(value) = sources.outlines.glyph(key, font, work)? else {
            return Ok(unmeasured);
        };
        let (asc, desc) = (value.asc, value.desc);
        let outline = ScaledOutline {
            value,
            scale: style.scale,
            offset_y: 0.0,
        };
        (outline, asc, desc, glyph.advance)
    };
    let cbox = *outline.value.shape.cbox();
    let [sx, sy] = outline.scale;
    Ok(LaidGlyph {
        x_min: round_ties_even_i32(f64::from(cbox.x_min) * sx),
        x_max: round_ties_even_i32(f64::from(cbox.x_max) * sx),
        asc: round_ties_even_i32(f64::from(asc) * sy),
        desc: round_ties_even_i32(f64::from(desc) * sy),
        advance,
        pos: Vector::default(),
        outline: Some(outline),
    })
}
