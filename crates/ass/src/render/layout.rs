//! Laying out an event's glyphs as libass does: outlines and metrics
//! (`retrieve_glyphs`), the pen positions before wrapping
//! (`preliminary_layout`), line wrapping (`wrap_lines_smart`), karaoke, the
//! visual order (`reorder_text`), alignment (`align_lines`), the text box
//! (`compute_string_bbox`) and the baseline shear (`apply_baseline_shear`).

use triergon_core::{BudgetVec, Result, WorkBudget, round_ties_even_i32};
use triergon_raster::{Vector, pixels_26_6};

use super::fonts::Library;
use super::geometry::Scales;
use super::math::d6_to_double;
use super::outlines::{GlyphKey, Outlines};
use super::text::{Break, Glyphs};
use crate::{HAlign, Justify, KaraokeGlyph, Limits, Syllables, WrapStyle, charge};

/// One line's ascent and descent in pixels.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Line {
    pub asc: f64,
    pub desc: f64,
}

/// What measuring the lines gives.
#[derive(Debug)]
pub(super) struct Lines {
    pub lines: BudgetVec<Line>,
    /// The text's height in pixels.
    pub height: f64,
    /// Border extents in whole pixels, rounded as VSFilter rounds them.
    pub border_top: i32,
    pub border_bottom: i32,
    pub border_x: i32,
}

/// The text box in pre-aspect pixels.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct TextBox {
    pub x_min: f64,
    pub y_min: f64,
    pub x_max: f64,
    pub y_max: f64,
}

/// `retrieve_glyphs`: every glyph's outline, box, advance (for drawings) and
/// ascent and descent at its scale, the italic correction and letter
/// spacing.
pub(super) fn retrieve(
    glyphs: &mut Glyphs<'_>,
    library: &Library,
    outlines: &mut Outlines,
    limits: &Limits,
    scales: &Scales,
    par: f64,
) -> Result<()> {
    for i in 0..glyphs.len {
        let mut member = Some(i);
        while let Some(g) = member {
            member = glyphs.get(g).next;
            let info = glyphs.get(g);
            let (value, scale, offset_y, asc, desc) = if let Some(drawing) = info.drawing {
                let Some(value) = outlines.drawing(&drawing, limits)? else {
                    continue;
                };
                let base = 1_i32.wrapping_shl(drawing.scale.wrapping_sub(1) as u32);
                let w = if base > 0 { 1.0 / f64::from(base) } else { 0.0 };
                let scale = [
                    info.scale[0] * w * scales.screen[0] / par,
                    info.scale[1] * w * scales.screen[1],
                ];
                let desc = 64_i32.wrapping_mul(drawing.baseline);
                let asc = value.asc.wrapping_sub(desc);
                let offset_y = -f64::from(asc) * scale[1];
                (value, scale, offset_y, asc, desc)
            } else {
                let font = library.get(info.font);
                let key = GlyphKey {
                    font: info.font,
                    face: info.face,
                    size: info.font_size.to_bits(),
                    glyph: info.glyph,
                    bold: info.bold,
                    italic: info.italic,
                    decorations: info.decorations,
                };
                let Some(value) = outlines.glyph(key, font)? else {
                    continue;
                };
                let (asc, desc) = (value.asc, value.desc);
                (value, info.scale, 0.0, asc, desc)
            };
            let cbox = *value.shape.cbox();
            let advance = value.advance;
            let info = glyphs.get_mut(g);
            info.outline_scale = scale;
            info.outline_offset = [0.0, offset_y];
            info.bbox.x_min = round_ties_even_i32(f64::from(cbox.x_min) * scale[0]);
            info.bbox.y_min = round_ties_even_i32(f64::from(cbox.y_min) * scale[1] + offset_y);
            info.bbox.x_max = round_ties_even_i32(f64::from(cbox.x_max) * scale[0]);
            info.bbox.y_max = round_ties_even_i32(f64::from(cbox.y_max) * scale[1] + offset_y);
            if info.drawing.is_some() {
                info.advance = Vector::new(round_ties_even_i32(f64::from(advance) * scale[0]), 0);
                info.cluster_advance = info.advance;
            }
            info.asc = round_ties_even_i32(f64::from(asc) * scale[1]);
            info.desc = round_ties_even_i32(f64::from(desc) * scale[1]);
            info.outline = Some(value);
        }
        // Space after italic text that gives way to upright text.
        if i > 0 && glyphs.get(i - 1).italic.0 != 0 && glyphs.get(i).italic.0 == 0 {
            let mut back = i - 1;
            while back > 0
                && glyphs.get(back).bbox.x_max - glyphs.get(back).bbox.x_min == 0
                && glyphs.get(back).italic.0 != 0
            {
                back -= 1;
            }
            let og = glyphs.get_mut(back);
            if og.bbox.x_max > og.cluster_advance.x {
                og.cluster_advance.x = og.bbox.x_max;
            }
        }
        let info = glyphs.get_mut(i);
        info.cluster_advance.x = info.cluster_advance.x.wrapping_add(info.spacing_scaled);
    }
    Ok(())
}

/// `preliminary_layout`: pen positions along one line.
pub(super) fn preliminary(glyphs: &mut Glyphs<'_>) {
    let mut pen = Vector::default();
    for i in 0..glyphs.len {
        let mut cluster = pen;
        glyphs.each_in_cluster(i, |info| {
            info.pos = cluster;
            cluster.x = cluster.x.wrapping_add(info.advance.x);
            cluster.y = cluster.y.wrapping_add(info.advance.y);
        });
        let root = glyphs.get(i);
        pen.x = pen.x.wrapping_add(root.cluster_advance.x);
        pen.y = pen.y.wrapping_add(root.cluster_advance.y);
    }
}

/// The right end of glyph `g` minus the left end of glyph `s`, in pixels.
fn width(glyphs: &Glyphs<'_>, s: usize, e: usize) -> f64 {
    let (s, e) = (glyphs.get(s), glyphs.get(e));
    d6_to_double(
        e.bbox
            .x_max
            .wrapping_add(e.pos.x)
            .wrapping_sub(s.bbox.x_min.wrapping_add(s.pos.x)),
    )
}

/// `wrap_lines_smart`: breaks, balanced unless `\q1`, then the whitespace
/// trimmed and the lines measured.
pub(super) fn wrap(
    glyphs: &mut Glyphs<'_>,
    max_width: f64,
    wrap_style: WrapStyle,
    line_spacing: f64,
    scales: &Scales,
    lines: &mut Lines,
    work: &mut WorkBudget,
) -> Result<()> {
    let count = wrap_naive(glyphs, max_width, wrap_style);
    if wrap_style != WrapStyle::EndOfLine {
        rebalance(glyphs, work)?;
    }
    trim_whitespace(glyphs);
    measure(glyphs, count, line_spacing, scales, lines)
}

/// `wrap_lines_naive`: a hard break at every `\N`, a soft one at the last
/// space before a glyph that overflows. The number of lines.
fn wrap_naive(glyphs: &mut Glyphs<'_>, max_width: f64, wrap_style: WrapStyle) -> usize {
    let len = glyphs.len;
    let mut lines = 1;
    let mut start = 0;
    let mut last_breakable: Option<usize> = None;
    for i in 0..len {
        let symbol = glyphs.get(i).symbol;
        let length = width(glyphs, start, i);
        let mut break_at = None;
        let mut kind = Break::None;
        if symbol == u32::from(b'\n') {
            kind = Break::Hard;
            break_at = Some(i);
        } else if length >= max_width
            && symbol != u32::from(b' ')
            && wrap_style != WrapStyle::Manual
        {
            kind = Break::Soft;
            break_at = last_breakable;
        }
        if symbol == u32::from(b' ') {
            last_breakable = Some(i);
        }
        if let Some(at) = break_at
            && at + 1 < len
        {
            glyphs.get_mut(at + 1).linebreak = kind;
            last_breakable = None;
            start = at + 1;
            lines += 1;
        }
    }
    lines
}

/// The last glyph before `end` that is not a space, or `start`.
fn rewind_spaces(glyphs: &Glyphs<'_>, start: usize, end: usize) -> usize {
    let mut g = end;
    loop {
        g -= 1;
        if !(g > start && glyphs.get(g).symbol == u32::from(b' ')) {
            return g;
        }
    }
}

/// `wrap_lines_rebalance`: moves soft breaks back by a word while that
/// evens out the two lines around them; each pass over the text is charged,
/// as breaks only move back and a pass may move each by one word.
fn rebalance(glyphs: &mut Glyphs<'_>, work: &mut WorkBudget) -> Result<()> {
    let len = glyphs.len;
    let space = u32::from(b' ');
    loop {
        charge(work, len)?;
        let mut changed = false;
        let (mut s2, mut s3): (Option<usize>, usize) = (None, 0);
        for i in 0..=len {
            if i < len && glyphs.get(i).linebreak == Break::None {
                continue;
            }
            let s1 = s2;
            (s2, s3) = (Some(s3), i);
            let (Some(a), Some(mut b)) = (s1, s2) else {
                continue;
            };
            if glyphs.get(b).linebreak != Break::Soft {
                continue;
            }
            let mut w = rewind_spaces(glyphs, a, b);
            let e1_old = w;
            while w > a && glyphs.get(w).symbol != space {
                w -= 1;
            }
            let mut e1 = w;
            while e1 > a && glyphs.get(e1).symbol == space {
                e1 -= 1;
            }
            if glyphs.get(w).symbol == space {
                w += 1;
            }
            if w == a {
                continue;
            }
            let e2 = rewind_spaces(glyphs, b, s3);
            let l1 = width(glyphs, a, e1_old);
            let l2 = width(glyphs, b, e2);
            let l1_new = width(glyphs, a, e1);
            let l2_new = width(glyphs, w, e2);
            let diff = |x: f64, y: f64| if x < y { y - x } else { x - y };
            if diff(l1_new, l2_new) < diff(l1, l2) {
                glyphs.get_mut(w).linebreak = Break::Soft;
                glyphs.get_mut(b).linebreak = Break::None;
                b = w;
                s2 = Some(b);
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
    }
}

/// `trim_whitespace`: spaces and `\n` at the ends of lines are hidden and
/// left out of the line metrics.
fn trim_whitespace(glyphs: &mut Glyphs<'_>) {
    let len = glyphs.len;
    if len == 0 {
        return;
    }
    let is_space = |glyphs: &Glyphs<'_>, i: usize| {
        let g = glyphs.get(i);
        g.symbol == u32::from(b' ') || g.symbol == u32::from(b'\n')
    };
    let whitespace = |glyphs: &Glyphs<'_>, i: usize| {
        is_space(glyphs, i) && glyphs.get(i).linebreak == Break::None
    };
    let mark = |glyphs: &mut Glyphs<'_>, i: usize| {
        let g = glyphs.get_mut(i);
        g.skip = true;
        g.trimmed = true;
    };
    let mut i = len - 1;
    while i > 0 && whitespace(glyphs, i) {
        mark(glyphs, i);
        i -= 1;
    }
    let mut i = 0;
    while i < len && whitespace(glyphs, i) {
        mark(glyphs, i);
        i += 1;
    }
    if i < len {
        glyphs.get_mut(i).starts_new_run = true;
    }
    let mut i = 0;
    while i < len {
        if glyphs.get(i).linebreak != Break::None {
            let mut j = i.wrapping_sub(1);
            while j > 0 && j < len && whitespace(glyphs, j) {
                mark(glyphs, j);
                j -= 1;
            }
            let mut next = i;
            if is_space(glyphs, i) {
                mark(glyphs, i);
                let mut j = i + 1;
                while j < len && whitespace(glyphs, j) {
                    mark(glyphs, j);
                    j += 1;
                }
                next = j;
                i = j - 1;
            }
            if next < len {
                glyphs.get_mut(next).starts_new_run = true;
            }
        }
        i += 1;
    }
}

/// `measure_text`: each line's ascent and descent, the height and the border
/// extents. A line of only a `\n` is half height; trimmed whitespace counts
/// only for a line that has nothing else.
fn measure(
    glyphs: &Glyphs<'_>,
    count: usize,
    line_spacing: f64,
    scales: &Scales,
    out: &mut Lines,
) -> Result<()> {
    out.lines.clear();
    out.lines.try_reserve_exact(count)?;
    out.height = 0.0;
    out.border_x = 0;
    let mut state = Measure::default();
    let mut line = 0;
    for i in 0..glyphs.len {
        let cur = glyphs.get(i);
        if cur.linebreak != Break::None {
            state.finish(line, scales, out)?;
            state = Measure::default();
            line += 1;
        }
        if state.empty && !cur.trimmed {
            state.empty = false;
            state.reset_maxima();
        } else if !state.empty && cur.trimmed {
            continue;
        }
        state.asc = state.asc.max(cur.asc);
        state.desc = state.desc.max(cur.desc);
        state.border_y = fmax(state.border_y, cur.border[1]);
        state.border_x = fmax(state.border_x, cur.border[0]);
        if cur.symbol != u32::from(b'\n') {
            state.scale = 1.0 / 64.0;
        }
    }
    state.finish(line, scales, out)?;
    out.height += line as f64 * line_spacing;
    Ok(())
}

/// libass's `FFMAX` on doubles: `a > b ? a : b`.
fn fmax(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

/// The maxima of the line being measured.
#[derive(Debug)]
struct Measure {
    scale: f64,
    asc: i32,
    desc: i32,
    border_x: f64,
    border_y: f64,
    empty: bool,
}

impl Default for Measure {
    fn default() -> Self {
        Self {
            scale: 0.5 / 64.0,
            asc: 0,
            desc: 0,
            border_x: 0.0,
            border_y: 0.0,
            empty: true,
        }
    }
}

impl Measure {
    fn reset_maxima(&mut self) {
        self.asc = 0;
        self.desc = 0;
        self.border_x = 0.0;
        self.border_y = 0.0;
    }

    fn finish(&self, line: usize, scales: &Scales, out: &mut Lines) -> Result<()> {
        let asc = self.scale * f64::from(self.asc);
        let desc = self.scale * f64::from(self.desc);
        out.lines
            .try_push(Line { asc, desc })
            .map_err(|e| e.error)?;
        out.height += asc + desc;
        out.border_bottom = (scales.border[1] * self.border_y + 0.5) as i32;
        if line == 0 {
            out.border_top = out.border_bottom;
        }
        out.border_x = out
            .border_x
            .max((scales.border[0] * self.border_x + 0.5) as i32);
        Ok(())
    }
}

/// `ass_process_karaoke_effects` over the pen positions before reordering.
pub(super) fn karaoke(
    glyphs: &mut Glyphs<'_>,
    elapsed: i64,
    scratch: &mut BudgetVec<KaraokeGlyph>,
) -> Result<()> {
    scratch.clear();
    scratch.try_reserve_exact(glyphs.len)?;
    for g in glyphs.roots() {
        scratch
            .try_push(KaraokeGlyph {
                starts_new_run: g.starts_new_run,
                mark: g.karaoke,
                skip: g.skip,
                x: g.pos.x,
                advance: g.advance.x,
                rotate_z: g.rotation[2],
            })
            .map_err(|e| e.error)?;
    }
    let laid_out = scratch.as_slice();
    for syllable in Syllables::new(laid_out, elapsed) {
        for i in syllable.glyphs.clone() {
            let timing = syllable.timing(&laid_out[i]);
            let g = glyphs.get_mut(i);
            g.effect = Some(syllable.effect);
            g.effect_timing = timing;
            if syllable.swap_colours {
                g.colours.swap(0, 1);
            }
        }
    }
    Ok(())
}

/// `reorder_text`: positions in visual order, line by line.
pub(super) fn reorder(glyphs: &mut Glyphs<'_>, map: &[usize], lines: &Lines, line_spacing: f64) {
    let mut pen = Vector::default();
    let mut line = 1;
    for i in 0..glyphs.len {
        if glyphs.get(i).linebreak != Break::None {
            pen.x = 0;
            let (prev, next) = (
                lines.lines.as_slice()[line - 1],
                lines.lines.as_slice()[line],
            );
            pen.y = pen
                .y
                .wrapping_add(pixels_26_6(prev.desc))
                .wrapping_add(pixels_26_6(next.asc))
                .wrapping_add(pixels_26_6(line_spacing));
            line += 1;
        }
        let root = map[i];
        if glyphs.get(root).skip {
            continue;
        }
        let mut cluster = pen;
        let advance = glyphs.get(root).cluster_advance;
        pen.x = pen.x.wrapping_add(advance.x);
        pen.y = pen.y.wrapping_add(advance.y);
        glyphs.each_in_cluster(root, |info| {
            info.pos = Vector::new(
                info.offset.x.wrapping_add(cluster.x),
                info.offset.y.wrapping_add(cluster.y),
            );
            cluster.x = cluster.x.wrapping_add(info.advance.x);
            cluster.y = cluster.y.wrapping_add(info.advance.y);
        });
    }
}

/// `align_lines`: each line shifted for its alignment and justification.
pub(super) fn align(
    glyphs: &mut Glyphs<'_>,
    halign: HAlign,
    justify: Justify,
    max_text_width: f64,
) {
    let len = glyphs.len;
    let counts =
        |g: &super::text::GlyphInfo<'_>| !g.skip && g.symbol != u32::from(b'\n') && g.symbol != 0;
    let mut width = 0.0;
    let mut max_width: f64 = 0.0;
    for i in 0..=len {
        if i == len || glyphs.get(i).linebreak != Break::None {
            max_width = fmax(max_width, width);
            width = 0.0;
        }
        if i < len && counts(glyphs.get(i)) {
            width += d6_to_double(glyphs.get(i).cluster_advance.x);
        }
    }
    let mut last_break: usize = 0;
    width = 0.0;
    for i in 0..=len {
        if i == len || glyphs.get(i).linebreak != Break::None {
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
            for j in last_break..i {
                glyphs.each_in_cluster(j, |info| info.pos.x = info.pos.x.wrapping_add(shift));
            }
            last_break = i;
            width = 0.0;
        }
        if i < len && counts(glyphs.get(i)) {
            width += d6_to_double(glyphs.get(i).cluster_advance.x);
        }
    }
}

/// `compute_string_bbox`.
pub(super) fn text_box(glyphs: &Glyphs<'_>, lines: &Lines) -> TextBox {
    if glyphs.len == 0 {
        return TextBox::default();
    }
    let y_min = -lines.lines.as_slice()[0].asc;
    let mut b = TextBox {
        x_min: 32000.0,
        x_max: -32000.0,
        y_min,
        y_max: y_min + lines.height,
    };
    for g in glyphs.roots().iter().filter(|g| !g.skip) {
        let s = d6_to_double(g.pos.x);
        let e = s + d6_to_double(g.cluster_advance.x);
        b.x_min = b.x_min.min(s);
        b.x_max = b.x_max.max(e);
    }
    b
}

/// `apply_baseline_shear`: `\fay` raises each glyph by the advance before it
/// on its line or run. Glyphs scaled to nothing are hidden here.
pub(super) fn shear(glyphs: &mut Glyphs<'_>, map: &[usize], whole_text: bool) {
    let mut shear: i32 = 0;
    for i in 0..glyphs.len {
        let logical = glyphs.get(i);
        if logical.linebreak != Break::None || (!whole_text && logical.starts_new_run) {
            shear = 0;
        }
        let root = map[i];
        let info = glyphs.get_mut(root);
        if info.scale[0] == 0.0 || info.scale[1] == 0.0 {
            info.skip = true;
        }
        if info.skip {
            continue;
        }
        let fay = info.shear[1] / info.scale[0] * info.scale[1];
        glyphs.each_in_cluster(root, |cur| {
            // C's conversions of the double sums back to the int fields.
            cur.pos.y =
                (f64::from(cur.pos.y) + (f64::from(shear) + fay * f64::from(cur.offset.x))) as i32;
            shear = (f64::from(shear) + fay * f64::from(cur.advance.x)) as i32;
        });
    }
}
