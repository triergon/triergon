//! The shaping pipeline: HarfBuzz's `hb_ot_shape` from characters to positioned
//! glyphs.

use crate::buffer::{Buffer, GlyphClass, GlyphProps, Info, LigProps};
use crate::face::Face;
use crate::metrics::{Metrics, Scaled};
use crate::normalize::{self, normalize};
use crate::ot::apply::{Apply, Stage};
use crate::ot::digest::Digest;
use crate::ot::layout::{LayoutTable, Lookup};
use crate::plan::{Plan, StageMap};
use crate::shapers::{Shaper, ZeroMarks};
use crate::tag::Direction;
use crate::unicode::{GeneralCategory, code_point};
use std::ops::Range;
use triergon_core::Result;

/// Shapes `text[item]` with `face` as planned, with the characters around the
/// item as context; clusters are indices into `text`. The glyphs and positions
/// replace the buffer's contents, in visual order (right to left reversed), with
/// positions in the units of the metrics' [`Scale`](crate::Scale).
///
/// # Errors
///
/// `Invalid` for an item outside `text`; `Unsupported` for script features this
/// crate does not implement (the plan reports most at creation); `Policy` when
/// the work exceeds the buffer's [`Limits`](crate::Limits) or lookups nest
/// deeper than HarfBuzz's 64 levels; the buffer budget's errors; the first error
/// a [`Metrics`] function returns.
pub fn shape(
    face: &Face<'_>,
    metrics: &dyn Metrics,
    plan: &Plan,
    text: &[char],
    item: Range<usize>,
    buffer: &mut Buffer,
) -> Result<()> {
    buffer.load(text, item)?;
    let font = Scaled::new(metrics, face.font.units_per_em());
    let mut run = Run {
        face,
        plan,
        font: &font,
        direction: plan.direction,
    };
    let shaped = run.shape(buffer);
    // A font function's error comes first: the run went on without its answer.
    font.finish().and(shaped)
}

struct Run<'a, 'b> {
    face: &'a Face<'b>,
    plan: &'a Plan,
    font: &'a Scaled<'a>,
    /// The direction the glyphs are processed in: the script's own.
    direction: Direction,
}

impl Run<'_, '_> {
    fn shape(&mut self, buffer: &mut Buffer) -> Result<()> {
        let mut scratch = buffer.scratch;
        crate::props::set_all(buffer.infos_mut(), &mut scratch, self.plan.global_mask);
        buffer.scratch = scratch;
        if buffer.scratch.continuations {
            form_clusters(buffer)?;
        }
        self.ensure_native_direction(buffer);
        self.plan
            .shaper
            .preprocess_text(self.plan, buffer, self.font)?;
        self.substitute(buffer)?;
        self.position(buffer)?;
        self.hide_default_ignorables(buffer)?;
        self.plan
            .shaper
            .postprocess_glyphs(self.plan, buffer, self.font)
    }

    /// Shapes in the script's own direction: left-to-right text in a
    /// right-to-left script is reversed grapheme by grapheme, unless it is only
    /// numbers.
    fn ensure_native_direction(&mut self, buffer: &mut Buffer) {
        let Some(mut native) = self.plan.script.horizontal_direction() else {
            return;
        };
        if native == Direction::RightToLeft && self.direction == Direction::LeftToRight {
            let mut number = false;
            let mut letter = false;
            for info in buffer.infos() {
                let category = info.category();
                if category.is_letter() {
                    letter = true;
                    break;
                }
                if category == GeneralCategory::DecimalNumber
                    || matches!(info.glyph, 0x1F1E6..=0x1F1FF)
                {
                    number = true;
                }
            }
            if number && !letter {
                native = Direction::LeftToRight;
            }
        }
        if self.direction != native {
            buffer.reverse_groups(|_, b| b.is_continuation());
            self.direction = native;
        }
    }

    fn substitute(&mut self, buffer: &mut Buffer) -> Result<()> {
        self.rotate_chars(buffer);
        normalize(self.plan, buffer, self.font)?;
        self.setup_masks(buffer);
        if self.plan.fallback_mark_positioning {
            crate::fallback::recategorize_marks(buffer);
        }
        for info in buffer.infos_mut() {
            info.glyph = info.normalized;
        }
        let gdef = &self.face.gdef;
        for info in buffer.infos_mut() {
            info.glyph_props = gdef.glyph_props(info.glyph);
            info.lig = LigProps::default();
        }
        if self.plan.fallback_glyph_classes {
            for info in buffer.infos_mut() {
                let mark = info.category() == GeneralCategory::NonSpacingMark
                    && !info.is_default_ignorable();
                let class = if mark {
                    GlyphClass::Mark
                } else {
                    GlyphClass::Base
                };
                info.glyph_props = GlyphProps::new(class, 0);
            }
        }
        self.apply_stages(Stage::Substitute, buffer)
    }

    /// Mirrors characters of right-to-left text that the font has mirrored glyphs
    /// for; the others get `rtlm`.
    fn rotate_chars(&self, buffer: &mut Buffer) {
        if !self.plan.direction.is_backward() {
            return;
        }
        let rtlm = self.plan.rtlm_mask;
        for info in buffer.infos_mut() {
            let mirrored = code_point::mirroring(info.glyph);
            if mirrored != info.glyph && self.font.glyph(mirrored).is_some() {
                info.glyph = mirrored;
            } else {
                info.mask |= rtlm;
            }
        }
    }

    fn setup_masks(&self, buffer: &mut Buffer) {
        if buffer.scratch.fraction_slash && self.plan.has_frac {
            self.setup_fraction_masks(buffer);
        }
        self.plan.shaper.setup_masks(self.plan, buffer);
    }

    /// Digits around a fraction slash get `numr`, `frac` and `dnom`.
    fn setup_fraction_masks(&self, buffer: &mut Buffer) {
        let plan = self.plan;
        let (pre, post) = if self.direction.is_backward() {
            (
                plan.frac_mask | plan.dnom_mask,
                plan.numr_mask | plan.frac_mask,
            )
        } else {
            (
                plan.numr_mask | plan.frac_mask,
                plan.frac_mask | plan.dnom_mask,
            )
        };
        let infos = buffer.infos_mut();
        let digit = |info: &Info| info.category() == GeneralCategory::DecimalNumber;
        let mut i = 0;
        while i < infos.len() {
            if infos[i].glyph != 0x2044 {
                i += 1;
                continue;
            }
            let mut start = i;
            while start > 0 && digit(&infos[start - 1]) {
                start -= 1;
            }
            let mut end = i + 1;
            while end < infos.len() && digit(&infos[end]) {
                end += 1;
            }
            if start == i || end == i + 1 {
                i += 1;
                continue;
            }
            for info in &mut infos[start..i] {
                info.mask |= pre;
            }
            infos[i].mask |= plan.frac_mask;
            for info in &mut infos[i + 1..end] {
                info.mask |= post;
            }
            i = end;
        }
    }

    fn apply_stages(&self, stage: Stage, buffer: &mut Buffer) -> Result<()> {
        let t = stage as usize;
        let table: LayoutTable<'_> = if stage == Stage::Substitute {
            self.face.gsub
        } else {
            self.face.gpos
        };
        let lookups = self.plan.lookups[t].as_slice();
        let mut i = 0;
        buffer.update_digest();
        for &StageMap { last_lookup, pause } in self.plan.stages[t].iter() {
            for lookup in &lookups[i..last_lookup] {
                // A lookup none of whose glyphs is in the buffer changes nothing.
                if !lookup.digest.intersects(&buffer.digest) {
                    continue;
                }
                let mut ctx = Apply::new(
                    stage,
                    table,
                    &self.face.gdef,
                    buffer,
                    self.font,
                    self.direction,
                );
                apply_lookup(&mut ctx, lookup)?;
            }
            i = last_lookup;
            if let Some(pause) = pause {
                Shaper::pause(pause, self.plan, self.face, buffer, self.font)?;
                buffer.update_digest();
            }
        }
        Ok(())
    }

    fn position(&mut self, buffer: &mut Buffer) -> Result<()> {
        buffer.clear_positions()?;
        for (info, pos) in buffer.info.as_slice().iter().zip(buffer.pos.as_mut_slice()) {
            pos.position.x_advance = self.font.advance(info.glyph);
        }
        if buffer.scratch.space_fallback {
            self.fallback_spaces(buffer);
        }
        let plan = self.plan;
        let adjust_offsets =
            plan.adjust_mark_positioning_when_zeroing && !self.direction.is_backward();
        crate::ot::gpos::start(buffer);
        let zero = plan
            .zero_marks
            .then(|| plan.shaper.zero_width_marks())
            .flatten();
        if zero == Some(ZeroMarks::Early) {
            zero_mark_widths(buffer, adjust_offsets);
        }
        if plan.apply_gpos {
            self.apply_stages(Stage::Position, buffer)?;
        }
        if plan.apply_kern {
            let mut ctx = Apply::new(
                Stage::Position,
                self.face.gpos,
                &self.face.gdef,
                buffer,
                self.font,
                self.direction,
            );
            self.face.kern.apply(&mut ctx, plan.kern_mask)?;
        } else if plan.apply_fallback_kern && self.font.has_h_kerning() {
            // The font functions' kerning, in visual order and already scaled.
            let backward = self.direction.is_backward();
            if backward {
                buffer.reverse();
            }
            let mut ctx = Apply::new(
                Stage::Position,
                self.face.gpos,
                &self.face.gdef,
                buffer,
                self.font,
                self.direction,
            );
            crate::ot::kern::kern_machine(&mut ctx, plan.kern_mask, false, |a, b| {
                self.font.h_kerning(a, b)
            })?;
            if backward {
                buffer.reverse();
            }
        }
        if zero == Some(ZeroMarks::Late) {
            zero_mark_widths(buffer, adjust_offsets);
        }
        if buffer.scratch.default_ignorables {
            for (info, pos) in buffer.info.as_slice().iter().zip(buffer.pos.as_mut_slice()) {
                if info.is_default_ignorable() {
                    pos.position.x_advance = 0;
                    pos.position.y_advance = 0;
                    pos.position.x_offset = 0;
                }
            }
        }
        crate::ot::gpos::finish_offsets(buffer, self.direction)?;
        if plan.fallback_mark_positioning {
            crate::fallback::position_marks(
                buffer,
                self.font,
                self.direction,
                plan.direction,
                adjust_offsets,
            );
        }
        if self.direction.is_backward() {
            buffer.reverse();
        }
        Ok(())
    }

    /// Space characters drawn with the font's space glyph get the width their
    /// kind calls for.
    fn fallback_spaces(&self, buffer: &mut Buffer) {
        let scale = self.font.scale.x;
        let metrics = self.font;
        for (info, pos) in buffer.info.as_slice().iter().zip(buffer.pos.as_mut_slice()) {
            if info.category() != GeneralCategory::SpaceSeparator || info.ligated() {
                continue;
            }
            let advance = &mut pos.position.x_advance;
            match info.unicode.high() {
                kind @ (1..=6 | 16) => {
                    let kind = i64::from(kind);
                    *advance = ((i64::from(scale) + kind / 2) / kind) as i32;
                }
                normalize::SPACE_4_EM_18 => *advance = (i64::from(scale) * 4 / 18) as i32,
                normalize::SPACE_FIGURE => {
                    if let Some(g) =
                        (u32::from(b'0')..=u32::from(b'9')).find_map(|c| metrics.glyph(c))
                    {
                        *advance = metrics.advance(g);
                    }
                }
                normalize::SPACE_PUNCTUATION => {
                    if let Some(g) = metrics
                        .glyph(u32::from(b'.'))
                        .or_else(|| metrics.glyph(u32::from(b',')))
                    {
                        *advance = metrics.advance(g);
                    }
                }
                normalize::SPACE_NARROW => *advance /= 2,
                _ => {}
            }
        }
    }

    /// Default ignorables become the space glyph (or disappear without one).
    fn hide_default_ignorables(&self, buffer: &mut Buffer) -> Result<()> {
        if !buffer.scratch.default_ignorables {
            return Ok(());
        }
        if let Some(space) = self.font.glyph(0x20) {
            for info in buffer.infos_mut() {
                if info.is_default_ignorable() {
                    info.glyph = space;
                }
            }
            Ok(())
        } else {
            buffer.delete_glyphs_inplace(Info::is_default_ignorable)
        }
    }
}

/// Merges the clusters of each grapheme: a character and the continuations that
/// follow it.
fn form_clusters(buffer: &mut Buffer) -> Result<()> {
    let mut start = 0;
    while start < buffer.len() {
        let end = buffer.group_end(start, |_, b| b.is_continuation());
        buffer.merge_clusters(start, end)?;
        start = end;
    }
    Ok(())
}

fn zero_mark_widths(buffer: &mut Buffer, adjust_offsets: bool) {
    for (info, pos) in buffer.info.as_slice().iter().zip(buffer.pos.as_mut_slice()) {
        if info.is_mark() {
            let p = &mut pos.position;
            if adjust_offsets {
                p.x_offset = p.x_offset.saturating_sub(p.x_advance);
                p.y_offset = p.y_offset.saturating_sub(p.y_advance);
            }
            p.x_advance = 0;
            p.y_advance = 0;
        }
    }
}

/// Applies one lookup of the plan over the buffer: forwards with an output pass,
/// or backwards in place for reverse chaining substitutions.
fn apply_lookup(ctx: &mut Apply<'_, '_>, map: &crate::plan::LookupMap) -> Result<()> {
    let Some(lookup) = ctx.table.lookup(map.index) else {
        return Ok(());
    };
    ctx.auto_zwj = map.auto_zwj;
    ctx.auto_zwnj = map.auto_zwnj;
    ctx.random = map.random;
    ctx.per_syllable = map.per_syllable;
    apply_lookup_masked(ctx, &lookup, map.mask, map.digest)
}

/// Applies `lookup` to the glyphs `mask` selects, with the context's joiner,
/// random and syllable settings.
pub(crate) fn apply_lookup_masked<'a>(
    ctx: &mut Apply<'a, '_>,
    lookup: &Lookup<'a>,
    mask: u32,
    digest: Digest,
) -> Result<()> {
    if ctx.buffer.is_empty() || mask == 0 {
        return Ok(());
    }
    ctx.set_lookup_mask(mask);
    ctx.set_lookup_flags(lookup.flags);
    let substitute = ctx.stage == Stage::Substitute;
    let reverse = substitute && lookup.is_reverse_chain();
    let applies = |ctx: &Apply<'_, '_>, info: &Info| {
        digest.may_have(info.glyph)
            && info.mask & ctx.lookup_mask != 0
            && ctx.check_glyph_property(info, ctx.lookup_flags)
    };
    if reverse {
        let mut idx = ctx.buffer.len();
        while idx > 0 {
            idx -= 1;
            ctx.buffer.idx = idx;
            ctx.buffer.charge(1)?;
            let info = *ctx.buffer.cur();
            if applies(ctx, &info) {
                ctx.apply_subtables(lookup)?;
            }
        }
        return Ok(());
    }
    if substitute {
        ctx.buffer.clear_output();
    }
    ctx.buffer.idx = 0;
    while ctx.buffer.idx < ctx.buffer.len() {
        ctx.buffer.charge(1)?;
        let info = *ctx.buffer.cur();
        if !applies(ctx, &info) || !ctx.apply_subtables(lookup)? {
            ctx.buffer.next_glyph()?;
        }
    }
    if substitute {
        ctx.buffer.sync()?;
    }
    Ok(())
}
