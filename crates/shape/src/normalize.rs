//! Normalisation against the font: characters the font lacks are decomposed into
//! ones it has, marks are put in canonical order, and marks are recomposed with
//! their base where the font has the composite — HarfBuzz's "composed
//! diacritics" mode. Shapers that ask for no normalisation get only the
//! decomposition of characters the font lacks and the mark order.

use crate::buffer::{Buffer, CharFlag, Info};
use crate::metrics::Scaled;
use crate::plan::Plan;
use crate::props::set_unicode_props;
use crate::unicode;
use triergon_core::Result;

/// Mark sequences longer than this are left in their order.
const MAX_COMBINING_MARKS: usize = 32;

/// How a shaper wants text normalised, as HarfBuzz's normalisation modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Characters the font has stay as they are, marks too; nothing composes.
    /// (Hangul fonts are not made to mix syllables and jamo.)
    None,
    /// Marks are fully decomposed and composed back with their base where the
    /// font has the composite.
    ComposedDiacritics,
    /// As `ComposedDiacritics`, but every character is decomposed as far as the
    /// font allows, not only those the font lacks (split vowel signs).
    ComposedDiacriticsNoShortCircuit,
}

struct Normalizer<'a, 'b> {
    plan: &'a Plan,
    font: &'a Scaled<'b>,
}

impl Normalizer<'_, '_> {
    fn glyph(&self, c: u32) -> Option<u32> {
        self.font.glyph(c)
    }

    /// Outputs the character `c` with glyph `glyph` as a copy of the current one.
    fn output_char(&self, buffer: &mut Buffer, c: u32, glyph: u32) -> Result<()> {
        buffer.cur_mut().normalized = glyph;
        buffer.output_glyph(c)?;
        let out = buffer.out_len - 1;
        let mut info = *buffer.out_info(out);
        set_unicode_props(&mut info, &mut buffer.scratch);
        *buffer.out_info_mut(out) = info;
        Ok(())
    }

    fn next_char(buffer: &mut Buffer, glyph: u32) -> Result<()> {
        buffer.cur_mut().normalized = glyph;
        buffer.next_glyph()
    }

    /// Outputs the decomposition of `ab` the font supports, preferring the
    /// shortest (`shortest`) or the full one; returns the characters output.
    fn decompose(&self, buffer: &mut Buffer, shortest: bool, ab: u32, depth: u8) -> Result<usize> {
        let Some((a, b)) = self.plan.shaper.decompose(ab) else {
            return Ok(0);
        };
        let b_glyph = match b {
            0 => None,
            b => match self.glyph(b) {
                Some(g) => Some(g),
                None => return Ok(0),
            },
        };
        let a_glyph = self.glyph(a);
        let output_b = |buffer: &mut Buffer| -> Result<usize> {
            match b_glyph {
                Some(g) => self.output_char(buffer, b, g).map(|()| 1),
                None => Ok(0),
            }
        };
        if let (true, Some(g)) = (shortest, a_glyph) {
            self.output_char(buffer, a, g)?;
            return Ok(1 + output_b(buffer)?);
        }
        // Canonical decompositions are only a few levels deep.
        let nested = if depth < 8 {
            self.decompose(buffer, shortest, a, depth + 1)?
        } else {
            0
        };
        if nested > 0 {
            return Ok(nested + output_b(buffer)?);
        }
        if let Some(g) = a_glyph {
            self.output_char(buffer, a, g)?;
            return Ok(1 + output_b(buffer)?);
        }
        Ok(0)
    }

    /// Maps or decomposes the current character: the glyph first when
    /// `shortest`, the decomposition first otherwise.
    fn decompose_current(&self, buffer: &mut Buffer, shortest: bool) -> Result<()> {
        let u = buffer.cur().glyph;
        let glyph = self.glyph(u);
        if let (true, Some(glyph)) = (shortest, glyph) {
            return Self::next_char(buffer, glyph);
        }
        if self.decompose(buffer, shortest, u, 0)? > 0 {
            buffer.skip_glyph();
            return Ok(());
        }
        if let Some(glyph) = glyph {
            return Self::next_char(buffer, glyph);
        }
        if buffer.cur().category() == unicode::GeneralCategory::SpaceSeparator
            && let (Some(space), Some(kind)) = (self.glyph(0x20), space_fallback(u))
        {
            buffer.cur_mut().unicode.set_high(kind);
            buffer.scratch.space_fallback = true;
            return Self::next_char(buffer, space);
        }
        if u == 0x2011 {
            // A non-breaking hyphen looks like a hyphen.
            if let Some(hyphen) = self.glyph(0x2010) {
                return Self::next_char(buffer, hyphen);
            }
        }
        Self::next_char(buffer, 0)
    }

    fn set_glyph(&self, info: &mut Info) {
        info.normalized = self.glyph(info.glyph).unwrap_or(0);
    }

    /// A cluster with variation selectors: a base with a selector becomes the
    /// variant glyph, or keeps its glyph and the selector stays as is.
    fn variation_cluster(&self, buffer: &mut Buffer, end: usize) -> Result<()> {
        while buffer.idx + 1 < end {
            let next = buffer.infos()[buffer.idx + 1].glyph;
            if !is_variation_selector(next) {
                let mut info = *buffer.cur();
                self.set_glyph(&mut info);
                *buffer.cur_mut() = info;
                buffer.next_glyph()?;
                continue;
            }
            let base = buffer.cur().glyph;
            if let Some(glyph) = self.font.variation_glyph(base, next) {
                buffer.cur_mut().normalized = glyph;
                buffer.replace_glyphs(2, &[base])?;
            } else {
                let mut info = *buffer.cur();
                self.set_glyph(&mut info);
                *buffer.cur_mut() = info;
                buffer.next_glyph()?;
                buffer.scratch.variation_selector_fallback = true;
                let cur = buffer.cur_mut();
                cur.unicode.set_category(unicode::GeneralCategory::Format);
                cur.unicode.set(CharFlag::VariationSelector, true);
                let mut info = *buffer.cur();
                self.set_glyph(&mut info);
                *buffer.cur_mut() = info;
                buffer.next_glyph()?;
            }
            while buffer.idx < end && is_variation_selector(buffer.cur().glyph) {
                let mut info = *buffer.cur();
                self.set_glyph(&mut info);
                *buffer.cur_mut() = info;
                buffer.next_glyph()?;
            }
        }
        if buffer.idx < end {
            let mut info = *buffer.cur();
            self.set_glyph(&mut info);
            *buffer.cur_mut() = info;
            buffer.next_glyph()?;
        }
        Ok(())
    }

    fn multi_char_cluster(&self, buffer: &mut Buffer, end: usize) -> Result<()> {
        if buffer.infos()[buffer.idx..end]
            .iter()
            .any(|i| is_variation_selector(i.glyph))
        {
            return self.variation_cluster(buffer, end);
        }
        let shortest = self.plan.shaper.normalization() == Mode::None;
        while buffer.idx < end {
            self.decompose_current(buffer, shortest)?;
        }
        Ok(())
    }
}

fn is_variation_selector(c: u32) -> bool {
    matches!(c, 0xFE00..=0xFE0F | 0xE0100..=0xE01EF)
}

/// HarfBuzz's fallback width class of a space character.
fn space_fallback(c: u32) -> Option<u8> {
    Some(match c {
        0x0020 | 0x00A0 => SPACE,
        0x2000 | 0x2002 => 2,
        0x2001 | 0x2003 | 0x3000 => 1,
        0x2004 => 3,
        0x2005 => 4,
        0x2006 => 6,
        0x2007 => SPACE_FIGURE,
        0x2008 => SPACE_PUNCTUATION,
        0x2009 => 5,
        0x200A => 16,
        0x202F => SPACE_NARROW,
        0x205F => SPACE_4_EM_18,
        _ => return None,
    })
}

pub(crate) const SPACE_4_EM_18: u8 = 17;
pub(crate) const SPACE: u8 = 18;
pub(crate) const SPACE_FIGURE: u8 = 19;
pub(crate) const SPACE_PUNCTUATION: u8 = 20;
pub(crate) const SPACE_NARROW: u8 = 21;

/// Normalises the buffer against the font, leaving each character's glyph in
/// `normalized`.
pub(crate) fn normalize(plan: &Plan, buffer: &mut Buffer, font: &Scaled<'_>) -> Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }
    let n = Normalizer { plan, font };
    let mode = plan.shaper.normalization();
    let short_circuit = mode != Mode::ComposedDiacriticsNoShortCircuit;
    let mut all_simple = true;
    buffer.clear_output();
    let count = buffer.len();
    loop {
        // Characters up to the last base before a mark map directly when the font
        // has them.
        let mut end = buffer.idx + 1;
        while end < count && !buffer.infos()[end].is_unicode_mark() {
            end += 1;
        }
        if end < count {
            end -= 1;
        }
        buffer.charge(end - buffer.idx)?;
        while short_circuit && buffer.idx < end {
            let Some(glyph) = n.glyph(buffer.cur().glyph) else {
                break;
            };
            Normalizer::next_char(buffer, glyph)?;
        }
        while buffer.idx < end {
            n.decompose_current(buffer, short_circuit)?;
        }
        if buffer.idx == count {
            break;
        }
        all_simple = false;
        let mut end = buffer.idx + 1;
        while end < count && buffer.infos()[end].is_unicode_mark() {
            end += 1;
        }
        n.multi_char_cluster(buffer, end)?;
        if buffer.idx >= count {
            break;
        }
    }
    buffer.sync()?;
    if !all_simple {
        reorder(plan, buffer)?;
    }
    if buffer.scratch.cgj {
        let infos = buffer.infos_mut();
        for i in 1..infos.len().saturating_sub(1) {
            if infos[i].glyph == 0x034F
                && (infos[i + 1].combining_class() == 0
                    || infos[i - 1].combining_class() <= infos[i + 1].combining_class())
            {
                infos[i].unicode.set(CharFlag::Hidden, false);
            }
        }
    }
    if !all_simple && mode != Mode::None {
        recompose(&n, buffer)?;
    }
    Ok(())
}

/// Sorts each mark sequence by modified combining class, then lets the shaper
/// adjust it.
fn reorder(plan: &Plan, buffer: &mut Buffer) -> Result<()> {
    let count = buffer.len();
    let mut i = 0;
    while i < count {
        if buffer.infos()[i].combining_class() == 0 {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < count && buffer.infos()[end].combining_class() != 0 {
            end += 1;
        }
        if end - i <= MAX_COMBINING_MARKS {
            buffer.sort_by_key(i, end, Info::combining_class)?;
            plan.shaper.reorder_marks(buffer, i, end)?;
        }
        i = end + 1;
    }
    Ok(())
}

/// Composes marks with their starter where nothing between blocks it and the
/// font has the composite.
fn recompose(n: &Normalizer<'_, '_>, buffer: &mut Buffer) -> Result<()> {
    buffer.clear_output();
    let count = buffer.len();
    let mut starter = 0;
    buffer.next_glyph()?;
    while buffer.idx < count {
        let cur = *buffer.cur();
        if cur.is_unicode_mark() {
            let prev = buffer.prev();
            let unblocked =
                starter == buffer.out_len - 1 || prev.combining_class() < cur.combining_class();
            let starter_char = buffer.out_info(starter).glyph;
            let composed = unblocked
                .then(|| n.plan.shaper.compose(n.plan, starter_char, cur.glyph))
                .flatten()
                .and_then(|ab| n.glyph(ab).map(|g| (ab, g)));
            if let Some((ab, glyph)) = composed {
                buffer.next_glyph()?;
                buffer.merge_out_clusters(starter, buffer.out_len)?;
                buffer.pop_out();
                let mut info = *buffer.out_info(starter);
                info.glyph = ab;
                info.normalized = glyph;
                set_unicode_props(&mut info, &mut buffer.scratch);
                *buffer.out_info_mut(starter) = info;
                continue;
            }
        }
        buffer.next_glyph()?;
        if buffer.prev().combining_class() == 0 {
            starter = buffer.out_len - 1;
        }
    }
    buffer.sync()
}
