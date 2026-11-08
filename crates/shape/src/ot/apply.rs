//! Applying lookups: the context every subtable sees, glyph skipping and matching,
//! and the sequence machinery shared by ligatures and contextual lookups.

use super::data::Data;
use super::gdef::Gdef;
use super::layout::{LayoutTable, Lookup, class, coverage};
use crate::buffer::{Buffer, Info, LigProps, glyph_props};
use crate::metrics::Scaled;
use crate::tag::Direction;
use triergon_core::{Error, Result};

/// Glyphs one rule may match, as HarfBuzz bounds them.
pub(crate) const MAX_CONTEXT_LENGTH: usize = 64;
const MAX_NESTING_LEVEL: u8 = 64;

/// Which table's lookups are applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Substitute,
    Position,
}

/// Everything a subtable reads or changes while it applies.
pub(crate) struct Apply<'a, 'b> {
    pub(crate) stage: Stage,
    pub(crate) table: LayoutTable<'a>,
    pub(crate) gdef: &'b Gdef<'a>,
    pub(crate) buffer: &'b mut Buffer,
    /// Positioning needs the font's scale and metrics.
    pub(crate) font: &'b Scaled<'b>,
    pub(crate) direction: Direction,
    pub(crate) lookup_mask: u32,
    pub(crate) lookup_props: u32,
    pub(crate) auto_zwnj: bool,
    pub(crate) auto_zwj: bool,
    pub(crate) per_syllable: bool,
    pub(crate) random: bool,
    nesting_left: u8,
    /// Where the glyphs of the last match are, input first.
    pub(crate) positions: [usize; MAX_CONTEXT_LENGTH],
    /// The syllable a shaper gives glyphs a substitution produces, if any.
    pub(crate) new_syllables: Option<u8>,
    /// The last base a mark-to-base lookup found, valid before `last_base_until`.
    pub(crate) last_base: Option<usize>,
    pub(crate) last_base_until: usize,
}

impl std::fmt::Debug for Apply<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Apply")
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}

/// Whether a glyph may be skipped while matching.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Skip {
    No,
    /// A default ignorable: skipped unless it matches.
    Maybe,
    Yes,
}

/// What the next glyph of a sequence must be.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Values<'a> {
    /// Any glyph.
    Any,
    /// Glyph ids, one `u16` each.
    Glyphs(Data<'a>),
    /// Classes, one `u16` each, in a class definition.
    Classes(Data<'a>, Data<'a>),
    /// Offsets, one `u16` each from `base`, to coverage tables.
    Coverages(Data<'a>, Data<'a>),
}

impl Values<'_> {
    fn matches(self, info: &Info, i: usize) -> Option<bool> {
        let value = |data: Data<'_>| data.u16(2 * i);
        Some(match self {
            Self::Any => return None,
            Self::Glyphs(glyphs) => u32::from(value(glyphs)) == info.glyph,
            Self::Classes(values, classes) => class(classes, info.glyph) == value(values),
            Self::Coverages(offsets, base) => {
                coverage(base.at(usize::from(value(offsets))), info.glyph).is_some()
            }
        })
    }
}

/// How one pass over the buffer matches glyphs: HarfBuzz's skipping iterator.
#[derive(Clone, Copy)]
pub(crate) struct Matcher<'a> {
    lookup_props: u32,
    mask: u32,
    ignore_zwnj: bool,
    ignore_zwj: bool,
    ignore_hidden: bool,
    per_syllable: bool,
    syllable: u8,
    values: Values<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Match,
    NotMatch,
    Skip,
}

/// Where a search for the next glyph of a sequence ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// At the glyph that matched.
    Match(usize),
    /// Before this index: after the glyph that did not match, or at the end.
    Stop(usize),
}

impl<'a, 'b> Apply<'a, 'b> {
    pub(crate) fn new(
        stage: Stage,
        table: LayoutTable<'a>,
        gdef: &'b Gdef<'a>,
        buffer: &'b mut Buffer,
        font: &'b Scaled<'b>,
        direction: Direction,
    ) -> Self {
        Self {
            stage,
            table,
            gdef,
            buffer,
            font,
            direction,
            lookup_mask: 1,
            lookup_props: 0,
            auto_zwnj: true,
            auto_zwj: true,
            per_syllable: false,
            random: false,
            nesting_left: MAX_NESTING_LEVEL,
            positions: [0; MAX_CONTEXT_LENGTH],
            new_syllables: None,
            last_base: None,
            last_base_until: 0,
        }
    }

    pub(crate) fn set_lookup_props(&mut self, props: u32) {
        self.lookup_props = self.gdef.sanitize_lookup_props(props);
    }

    pub(crate) fn set_lookup_mask(&mut self, mask: u32) {
        self.lookup_mask = mask;
        self.last_base = None;
        self.last_base_until = 0;
    }

    /// Whether lookups with `props` see `info` at all.
    pub(crate) fn check_glyph_property(&self, info: &Info, props: u32) -> bool {
        let glyph = u32::from(info.glyph_props);
        if glyph & props & Lookup::IGNORE_FLAGS != 0 {
            return false;
        }
        if glyph & u32::from(glyph_props::MARK) != 0 {
            if props & Lookup::USE_MARK_FILTERING_SET != 0 {
                return self
                    .gdef
                    .mark_set_covers((props >> 16) as usize, info.glyph);
            }
            if props & Lookup::MARK_ATTACHMENT_TYPE != 0 {
                return props & Lookup::MARK_ATTACHMENT_TYPE
                    == glyph & Lookup::MARK_ATTACHMENT_TYPE;
            }
        }
        true
    }

    /// The matcher for the glyphs a rule reads (`context` false, under the lookup
    /// mask) or the context around them (`context` true, any mask).
    pub(crate) fn matcher(&self, context: bool, values: Values<'a>) -> Matcher<'a> {
        let gpos = self.stage == Stage::Position;
        let syllable = self
            .buffer
            .infos()
            .get(self.buffer.idx)
            .map_or(0, |i| i.syllable);
        Matcher {
            lookup_props: self.lookup_props,
            mask: if context { u32::MAX } else { self.lookup_mask },
            ignore_zwnj: gpos || (context && self.auto_zwnj),
            ignore_zwj: context || self.auto_zwj,
            ignore_hidden: gpos,
            per_syllable: !gpos && self.per_syllable,
            syllable,
            values,
        }
    }

    fn may_skip(&self, m: &Matcher<'_>, info: &Info) -> Skip {
        if !self.check_glyph_property(info, m.lookup_props) {
            return Skip::Yes;
        }
        if info.is_default_ignorable()
            && (m.ignore_zwnj || !info.is_zwnj())
            && (m.ignore_zwj || !info.is_zwj())
            && (m.ignore_hidden || !info.is_hidden())
        {
            return Skip::Maybe;
        }
        Skip::No
    }

    /// Whether `info` would be skipped outright by `m`.
    pub(crate) fn skipped(&self, m: &Matcher<'_>, info: &Info) -> bool {
        self.may_skip(m, info) == Skip::Yes
    }

    fn outcome(&self, m: &Matcher<'_>, info: &Info, value: usize) -> Outcome {
        let skip = self.may_skip(m, info);
        if skip == Skip::Yes {
            return Outcome::Skip;
        }
        let matched = if info.mask & m.mask == 0
            || (m.per_syllable && m.syllable != 0 && m.syllable != info.syllable)
        {
            Some(false)
        } else {
            m.values.matches(info, value)
        };
        match (matched, skip) {
            (Some(true), _) | (None, Skip::No) => Outcome::Match,
            (_, Skip::No) => Outcome::NotMatch,
            _ => Outcome::Skip,
        }
    }

    /// Whether `m` matches `info` outright (rather than skipping or rejecting it).
    pub(crate) fn outcome_is_match(&self, m: &Matcher<'_>, info: &Info) -> bool {
        self.outcome(m, info, 0) == Outcome::Match
    }

    /// The input glyph after `idx` that matches value `value` of `m`, if the next
    /// glyph that is not skipped does. Each glyph looked at is charged.
    pub(crate) fn next(
        &mut self,
        m: &Matcher<'_>,
        idx: usize,
        value: usize,
    ) -> Result<Option<usize>> {
        Ok(match self.next_or_stop(m, idx, value)? {
            Next::Match(i) => Some(i),
            Next::Stop(_) => None,
        })
    }

    /// [`Self::next`], or where the search stopped.
    pub(crate) fn next_or_stop(
        &mut self,
        m: &Matcher<'_>,
        idx: usize,
        value: usize,
    ) -> Result<Next> {
        let mut i = idx;
        while i + 1 < self.buffer.len() {
            i += 1;
            self.buffer.charge(1)?;
            let info = self.buffer.infos()[i];
            match self.outcome(m, &info, value) {
                Outcome::Match => return Ok(Next::Match(i)),
                Outcome::NotMatch => return Ok(Next::Stop(i + 1)),
                Outcome::Skip => {}
            }
        }
        Ok(Next::Stop(self.buffer.len()))
    }

    /// The backtrack glyph before `idx` that matches value `value` of `m`. Each
    /// glyph looked at is charged.
    pub(crate) fn prev(
        &mut self,
        m: &Matcher<'_>,
        idx: usize,
        value: usize,
    ) -> Result<Option<usize>> {
        let mut i = idx;
        while i > 0 {
            i -= 1;
            self.buffer.charge(1)?;
            let info = *self.buffer.backtrack_info(i);
            match self.outcome(m, &info, value) {
                Outcome::Match => return Ok(Some(i)),
                Outcome::NotMatch => return Ok(None),
                Outcome::Skip => {}
            }
        }
        Ok(None)
    }

    /// Matches the rest of an input sequence after the current glyph, filling
    /// `positions`. Returns the end of the match and, for ligatures, the number of
    /// components the matched glyphs carry.
    pub(crate) fn match_input(
        &mut self,
        count: usize,
        values: Values<'a>,
    ) -> Result<Option<(usize, u8)>> {
        if count > MAX_CONTEXT_LENGTH {
            return Ok(None);
        }
        let idx = self.buffer.idx;
        self.positions[0] = idx;
        if count <= 1 {
            return Ok(Some((idx + 1, self.buffer.cur().lig_components())));
        }
        let m = self.matcher(false, values);
        let first = self.buffer.cur().lig;
        let (first_id, first_component) = (first.id(), first.component());
        let mut lig_base_skippable = None;
        let mut components = 0_u8;
        let mut at = idx;
        for i in 1..count {
            let Some(next) = self.next(&m, at, i - 1)? else {
                return Ok(None);
            };
            at = next;
            self.positions[i] = at;
            let this = self.buffer.infos()[at];
            let (this_id, this_component) = (this.lig.id(), this.lig.component());
            if first_id != 0 && first_component != 0 {
                // A mark of a ligature component may only combine with marks of the
                // same component, unless the ligature itself is skipped.
                if first_id != this_id || first_component != this_component {
                    let skippable = match lig_base_skippable {
                        Some(skippable) => skippable,
                        None => {
                            *lig_base_skippable.insert(self.ligature_base_skippable(&m, first_id)?)
                        }
                    };
                    if !skippable {
                        return Ok(None);
                    }
                }
            } else if this_id != 0 && this_component != 0 && this_id != first_id {
                // Do not ligate marks of another ligature with a different first.
                return Ok(None);
            }
            components = components.wrapping_add(this.lig_components_in_ligation());
        }
        let total = components.wrapping_add(self.buffer.cur().lig_components());
        Ok(Some((at + 1, total)))
    }

    /// Whether the ligature that the current mark belongs to would be skipped.
    fn ligature_base_skippable(&mut self, m: &Matcher<'_>, lig_id: u8) -> Result<bool> {
        let mut j = self.buffer.out_len;
        while j > 0 && self.buffer.out_info(j - 1).lig.id() == lig_id {
            self.buffer.charge(1)?;
            if self.buffer.out_info(j - 1).lig.component() == 0 {
                return Ok(self.skipped(m, self.buffer.out_info(j - 1)));
            }
            j -= 1;
        }
        Ok(false)
    }

    /// Matches `count` backtrack glyphs before the current one.
    pub(crate) fn match_backtrack(&mut self, count: usize, values: Values<'a>) -> Result<bool> {
        let m = self.matcher(true, values);
        let mut at = self.buffer.backtrack_len();
        for i in 0..count {
            let Some(p) = self.prev(&m, at, i)? else {
                return Ok(false);
            };
            at = p;
        }
        Ok(true)
    }

    /// Matches `count` lookahead glyphs from `start`.
    pub(crate) fn match_lookahead(
        &mut self,
        count: usize,
        values: Values<'a>,
        start: usize,
    ) -> Result<bool> {
        let m = self.matcher(true, values);
        let mut at = start - 1;
        for i in 0..count {
            let Some(n) = self.next(&m, at, i)? else {
                return Ok(false);
            };
            at = n;
        }
        Ok(true)
    }

    /// Records a substitution of the current glyph by `glyph` in its glyph props.
    fn set_glyph_class(&mut self, glyph: u32, class_guess: u16, ligature: bool, component: bool) {
        let has_classes = self.gdef.has_glyph_classes();
        let new_props = self.gdef.glyph_props(glyph);
        let syllable = self.new_syllables;
        self.buffer.digest.add(glyph);
        let cur = self.buffer.cur_mut();
        if let Some(syllable) = syllable {
            cur.syllable = syllable;
        }
        let mut props = cur.glyph_props | glyph_props::SUBSTITUTED;
        if ligature {
            props |= glyph_props::LIGATED;
            props &= !glyph_props::MULTIPLIED;
        }
        if component {
            props |= glyph_props::MULTIPLIED;
        }
        cur.glyph_props = if has_classes {
            (props & glyph_props::PRESERVE) | new_props
        } else if class_guess != 0 {
            (props & glyph_props::PRESERVE) | class_guess
        } else {
            props
        };
    }

    pub(crate) fn replace_glyph(&mut self, glyph: u32) -> Result<()> {
        self.set_glyph_class(glyph, 0, false, false);
        self.buffer.replace_glyph(glyph)
    }

    pub(crate) fn replace_glyph_inplace(&mut self, glyph: u32) {
        self.set_glyph_class(glyph, 0, false, false);
        self.buffer.cur_mut().glyph = glyph;
    }

    pub(crate) fn replace_glyph_with_ligature(
        &mut self,
        glyph: u32,
        class_guess: u16,
    ) -> Result<()> {
        self.set_glyph_class(glyph, class_guess, true, false);
        self.buffer.replace_glyph(glyph)
    }

    pub(crate) fn output_glyph_for_component(
        &mut self,
        glyph: u32,
        class_guess: u16,
    ) -> Result<()> {
        self.set_glyph_class(glyph, class_guess, false, true);
        self.buffer.output_glyph(glyph)
    }

    /// Replaces the matched glyphs by `glyph`, keeping marks between them and
    /// recording which ligature component each mark belongs to.
    pub(crate) fn ligate(
        &mut self,
        count: usize,
        match_end: usize,
        glyph: u32,
        total: u8,
    ) -> Result<()> {
        let buffer = &mut *self.buffer;
        buffer.merge_clusters(buffer.idx, match_end)?;
        let positions = self.positions;
        let infos = buffer.infos();
        let first = &infos[positions[0]];
        let rest_are_marks = positions[1..count].iter().all(|&p| infos[p].is_mark());
        let is_base_ligature = first.is_base_glyph() && rest_are_marks;
        let is_mark_ligature = first.is_mark() && rest_are_marks;
        let is_ligature = !is_base_ligature && !is_mark_ligature;
        let class_guess = if is_ligature {
            glyph_props::LIGATURE
        } else {
            0
        };
        let lig_id = if is_ligature {
            buffer.allocate_lig_id()
        } else {
            0
        };
        let mut last_lig_id = buffer.cur().lig.id();
        let mut last_components = buffer.cur().lig_components();
        let mut components_so_far = last_components;
        if is_ligature {
            let cur = buffer.cur_mut();
            cur.lig = LigProps::ligature(lig_id, total);
            if cur.category() == crate::unicode::GeneralCategory::NonSpacingMark {
                cur.unicode
                    .set_category(crate::unicode::GeneralCategory::OtherLetter);
            }
        }
        self.replace_glyph_with_ligature(glyph, class_guess)?;
        let component_of = |this: u8, so_far: u8, last: u8| {
            let this = if this == 0 { last } else { this };
            so_far.wrapping_sub(last).wrapping_add(this.min(last))
        };
        for &position in &positions[1..count] {
            while self.buffer.idx < position {
                if is_ligature {
                    let cur = self.buffer.cur_mut();
                    let component =
                        component_of(cur.lig.component(), components_so_far, last_components);
                    cur.lig = LigProps::mark(lig_id, component);
                }
                self.buffer.next_glyph()?;
            }
            let cur = self.buffer.cur();
            last_lig_id = cur.lig.id();
            last_components = cur.lig_components_in_ligation();
            components_so_far = components_so_far.wrapping_add(last_components);
            self.buffer.skip_glyph();
        }
        if !is_mark_ligature && last_lig_id != 0 {
            // Marks that followed the last component keep belonging to it.
            let idx = self.buffer.idx;
            for info in &mut self.buffer.infos_mut()[idx..] {
                if info.lig.id() != last_lig_id || info.lig.component() == 0 {
                    break;
                }
                let component =
                    component_of(info.lig.component(), components_so_far, last_components);
                info.lig = LigProps::mark(lig_id, component);
            }
        }
        Ok(())
    }

    /// Applies nested lookups at positions of the last match (`count` glyphs ending
    /// at `match_end`), then moves past the match.
    pub(crate) fn apply_records(
        &mut self,
        mut count: usize,
        records: Data<'a>,
        record_count: usize,
        match_end: usize,
    ) -> Result<()> {
        let backtrack = self.buffer.backtrack_len();
        let mut end = (backtrack + match_end - self.buffer.idx) as isize;
        let delta = backtrack as isize - self.buffer.idx as isize;
        for p in &mut self.positions[..count] {
            *p = (*p as isize + delta) as usize;
        }
        for r in 0..record_count {
            let index = usize::from(records.u16(4 * r));
            let lookup = records.u16(4 * r + 2);
            if index >= count {
                continue;
            }
            let orig_len = self.buffer.backtrack_len() + self.buffer.lookahead_len();
            if self.positions[index] >= orig_len {
                continue;
            }
            self.buffer.move_to(self.positions[index])?;
            if !self.recurse(lookup)? {
                continue;
            }
            let new_len = self.buffer.backtrack_len() + self.buffer.lookahead_len();
            let mut delta = new_len as isize - orig_len as isize;
            if delta == 0 {
                continue;
            }
            // The nested lookup changed the glyph count: shift the later positions.
            end += delta;
            let at = self.positions[index] as isize;
            if end < at {
                delta += at - end;
                end = at;
            }
            let mut next = index + 1;
            if delta > 0 {
                if delta as usize + count > MAX_CONTEXT_LENGTH {
                    break;
                }
            } else {
                delta = delta.max(next as isize - count as isize);
                next = (next as isize - delta) as usize;
            }
            let moved = count - next;
            let target = (next as isize + delta) as usize;
            self.positions.copy_within(next..next + moved, target);
            next = target;
            count = (count as isize + delta) as usize;
            for j in index + 1..next {
                self.positions[j] = self.positions[j - 1] + 1;
            }
            for p in &mut self.positions[next..count] {
                *p = (*p as isize + delta) as usize;
            }
        }
        self.buffer.move_to(end.max(0) as usize)
    }

    fn recurse(&mut self, lookup: u16) -> Result<bool> {
        if self.nesting_left == 0 {
            return Err(Error::Policy("nested lookups"));
        }
        self.buffer.charge(1)?;
        self.nesting_left -= 1;
        let result = self.apply_nested(lookup);
        self.nesting_left += 1;
        result
    }

    /// Whether this is the outermost lookup (reverse chaining lookups only apply
    /// there).
    pub(crate) fn is_outermost(&self) -> bool {
        self.nesting_left == MAX_NESTING_LEVEL
    }

    /// Applies lookup `index` to the current glyph, as a nested lookup does, with
    /// its own flags.
    fn apply_nested(&mut self, index: u16) -> Result<bool> {
        let Some(lookup) = self.table.lookup(index) else {
            return Ok(false);
        };
        let saved = self.lookup_props;
        // The nested lookup's matches must not overwrite the positions of the
        // rule that applies it.
        let positions = self.positions;
        self.set_lookup_props(lookup.props);
        let applied = self.apply_subtables(&lookup);
        self.lookup_props = saved;
        self.positions = positions;
        applied
    }

    /// Applies the first subtable of `lookup` that applies to the current glyph.
    pub(crate) fn apply_subtables(&mut self, lookup: &Lookup<'a>) -> Result<bool> {
        for data in lookup.subtables() {
            self.buffer.charge(1)?;
            let applied = match self.stage {
                Stage::Substitute => super::gsub::apply(self, lookup.kind, data)?,
                Stage::Position => super::gpos::apply(self, lookup.kind, data)?,
            };
            if applied {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
