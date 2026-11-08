//! The structures GSUB and GPOS share: script, language system, feature and
//! lookup lists, feature variations at the default instance, lookups and their
//! flags, coverage and class definitions.

use super::data::{Data, bsearch};
use crate::buffer::GlyphClass;
use crate::tag::Tag;
use crate::work;
use triergon_core::{Result, WorkBudget};

/// Which table a lookup belongs to: GSUB substitutes, GPOS positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Substitute,
    Position,
}

impl Stage {
    /// The lookup type of the table's extension subtables.
    const fn extension(self) -> u16 {
        match self {
            Self::Substitute => 7,
            Self::Position => 9,
        }
    }
}

/// A GSUB or GPOS table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LayoutTable<'a> {
    stage: Stage,
    /// The length of the table in bytes.
    len: usize,
    scripts: Data<'a>,
    features: Data<'a>,
    lookups: Data<'a>,
    /// The feature variations list.
    variations: Data<'a>,
}

/// The feature table substitutions in effect at the default instance, found
/// by [`LayoutTable::default_substitutions`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FeatureSubstitutions<'a>(Data<'a>);

/// "No feature": the required-feature index of a language system without one.
pub(crate) const NO_FEATURE: u16 = 0xffff;

/// How deep feature variation conditions may nest before they count as false.
const MAX_CONDITION_DEPTH: u8 = 8;

impl<'a> LayoutTable<'a> {
    /// The table in `table`, if any; an absent table or one of an unknown major
    /// version is empty.
    pub(crate) fn new(stage: Stage, table: Option<&'a [u8]>) -> Self {
        let empty = Self {
            stage,
            len: 0,
            scripts: Data::EMPTY,
            features: Data::EMPTY,
            lookups: Data::EMPTY,
            variations: Data::EMPTY,
        };
        let data = Data(table.unwrap_or_default());
        if data.u16(0) != 1 {
            return empty;
        }
        Self {
            len: data.0.len(),
            scripts: data.offset16(4),
            features: data.offset16(6),
            lookups: data.offset16(8),
            variations: if data.u16(2) >= 1 {
                data.offset32(10)
            } else {
                Data::EMPTY
            },
            ..empty
        }
    }

    /// The length of the table in bytes; zero for one that is absent or
    /// unreadable.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn has_data(&self) -> bool {
        !(self.scripts.is_empty() && self.features.is_empty() && self.lookups.is_empty())
    }

    fn script_count(&self) -> usize {
        self.scripts.count(0, 2, 6)
    }

    /// The index of the script record for `tag`, found by binary search.
    pub(crate) fn find_script(&self, tag: Tag) -> Option<u16> {
        find_record(self.scripts, 2, self.script_count(), tag)
    }

    /// The script at `index`; an absent index gives an empty script.
    pub(crate) fn script(&self, index: Option<u16>) -> ScriptTable<'a> {
        match index {
            Some(i) if usize::from(i) < self.script_count() => {
                ScriptTable(self.scripts.offset16(2 + 6 * usize::from(i) + 4))
            }
            _ => ScriptTable(Data::EMPTY),
        }
    }

    pub(crate) fn feature_count(&self) -> usize {
        self.features.count(0, 2, 6)
    }

    pub(crate) fn feature_tag(&self, index: u16) -> Tag {
        if usize::from(index) >= self.feature_count() {
            return Tag::from_u32(0);
        }
        Tag::from_u32(self.features.u32(2 + 6 * usize::from(index)))
    }

    /// The lookup indices of feature `index`, with `substitutions` applied.
    pub(crate) fn feature_lookups(
        &self,
        substitutions: FeatureSubstitutions<'a>,
        index: u16,
    ) -> impl ExactSizeIterator<Item = u16> + 'a {
        let feature = substitutions.of(index).unwrap_or_else(|| {
            if usize::from(index) < self.feature_count() {
                self.features.offset16(2 + 6 * usize::from(index) + 4)
            } else {
                Data::EMPTY
            }
        });
        let count = feature.count(2, 4, 2);
        (0..count).map(move |i| feature.u16(4 + 2 * i))
    }

    pub(crate) fn lookup_count(&self) -> usize {
        self.lookups.count(0, 2, 2)
    }

    pub(crate) fn lookup(&self, index: u16) -> Option<Lookup<'a>> {
        if usize::from(index) >= self.lookup_count() {
            return None;
        }
        Some(Lookup::new(
            self.stage,
            self.lookups.offset16(2 + 2 * usize::from(index)),
        ))
    }

    /// The first feature with `tag` in the whole feature list, each record
    /// looked at charged to `work`.
    pub(crate) fn find_feature(&self, tag: Tag, work: &mut WorkBudget) -> Result<Option<u16>> {
        for i in 0..self.feature_count() {
            work::charge(work, 1)?;
            if self.features.u32(2 + 6 * i) == tag.to_u32() {
                return Ok(Some(i as u16));
            }
        }
        Ok(None)
    }

    /// The feature table substitutions of the first feature variation record
    /// whose conditions all hold with every axis at its default coordinate.
    /// Each record tried and each condition evaluated is charged to `work`:
    /// conditions may share nested conditions, so evaluation is not bounded by
    /// the size of the table.
    pub(crate) fn default_substitutions(
        &self,
        work: &mut WorkBudget,
    ) -> Result<FeatureSubstitutions<'a>> {
        let variations = self.variations;
        if variations.u16(0) != 1 {
            return Ok(FeatureSubstitutions::default());
        }
        let declared = variations.u32(4) as usize;
        let count = declared.min(variations.0.len().saturating_sub(8) / 8);
        for at in (0..count).map(|i| 8 + 8 * i) {
            work::charge(work, 1)?;
            let set = variations.offset32(at);
            if conditions_hold(set, work)? {
                return Ok(FeatureSubstitutions(variations.offset32(at + 4)));
            }
        }
        Ok(FeatureSubstitutions::default())
    }
}

impl<'a> FeatureSubstitutions<'a> {
    /// The substitute table of feature `index`, if it has one.
    fn of(self, feature: u16) -> Option<Data<'a>> {
        let subst = self.0;
        let count = subst.count(4, 6, 6);
        let i = bsearch(count, |i| subst.u16(6 + 6 * i).cmp(&feature))?;
        Some(subst.offset32(6 + 6 * i + 2))
    }
}

/// The record with `tag` among `count` tag-and-offset records from `first`.
fn find_record(list: Data<'_>, first: usize, count: usize, tag: Tag) -> Option<u16> {
    bsearch(count, |i| list.u32(first + 6 * i).cmp(&tag.to_u32())).map(|i| i as u16)
}

/// Whether every condition of a condition set holds at the default instance.
fn conditions_hold(set: Data<'_>, work: &mut WorkBudget) -> Result<bool> {
    for c in 0..set.count(0, 2, 4) {
        if !condition_holds(set.offset32(2 + 4 * c), MAX_CONDITION_DEPTH, work)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether a condition holds with every axis at its default coordinate, zero.
fn condition_holds(condition: Data<'_>, depth: u8, work: &mut WorkBudget) -> Result<bool> {
    work::charge(work, 1)?;
    let Some(depth) = depth.checked_sub(1) else {
        return Ok(false);
    };
    let nested = |i: usize| condition.at(condition.u24(3 + 3 * i) as usize);
    let nested_count = usize::from(condition.u8(2)).min(condition.0.len().saturating_sub(3) / 3);
    Ok(match condition.u16(0) {
        // A null offset is the always-true condition.
        0 => true,
        // Axis range: the default coordinate must lie inside.
        1 => condition.i16(4) <= 0 && 0 <= condition.i16(6),
        // Value: the default value, the deltas being zero at the default.
        2 => condition.i16(2) > 0,
        // And, or: evaluated in order until the answer is known.
        format @ (3 | 4) => {
            let any = format == 4;
            for i in 0..nested_count {
                if condition_holds(nested(i), depth, work)? == any {
                    return Ok(any);
                }
            }
            !any
        }
        5 => !condition_holds(condition.at(condition.u24(2) as usize), depth, work)?,
        _ => false,
    })
}

/// A script table: its language systems.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScriptTable<'a>(Data<'a>);

impl<'a> ScriptTable<'a> {
    /// The language system record for `tag`, found by binary search.
    pub(crate) fn find_lang_sys(&self, tag: Tag) -> Option<u16> {
        find_record(self.0, 4, self.0.count(2, 4, 6), tag)
    }

    /// The language system `index`, or the default one for `None`.
    pub(crate) fn lang_sys(&self, index: Option<u16>) -> LangSys<'a> {
        match index {
            Some(i) if usize::from(i) < self.0.count(2, 4, 6) => {
                LangSys(self.0.offset16(4 + 6 * usize::from(i) + 4))
            }
            Some(_) => LangSys(Data::EMPTY),
            None => LangSys(self.0.offset16(0)),
        }
    }
}

/// A language system: a required feature and the features it lists.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LangSys<'a>(Data<'a>);

impl<'a> LangSys<'a> {
    pub(crate) fn required_feature(&self) -> Option<u16> {
        let index = self.0.u16(2);
        (index != NO_FEATURE && !self.0.is_empty()).then_some(index)
    }

    pub(crate) fn feature_indices(&self) -> impl DoubleEndedIterator<Item = u16> + 'a {
        let data = self.0;
        (0..data.count(4, 6, 2)).map(move |i| data.u16(6 + 2 * i))
    }
}

/// What a lookup subtable does: its lookup type, read through extension
/// subtables. `Single`, `Context` and `ChainContext` exist in both tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LookupType {
    Single,
    /// GSUB only.
    Multiple,
    /// GSUB only.
    Alternate,
    /// GSUB only.
    Ligature,
    /// GSUB only: reverse chaining contextual single substitution.
    ReverseChain,
    /// GPOS only.
    Pair,
    /// GPOS only.
    Cursive,
    /// GPOS only.
    MarkToBase,
    /// GPOS only.
    MarkToLigature,
    /// GPOS only.
    MarkToMark,
    Context,
    ChainContext,
}

impl LookupType {
    /// The type numbered `value` in `stage`'s table; `None` for extension and
    /// unknown types.
    const fn of(stage: Stage, value: u16) -> Option<Self> {
        Some(match (stage, value) {
            (_, 1) => Self::Single,
            (Stage::Substitute, 2) => Self::Multiple,
            (Stage::Substitute, 3) => Self::Alternate,
            (Stage::Substitute, 4) => Self::Ligature,
            (Stage::Substitute, 5) | (Stage::Position, 7) => Self::Context,
            (Stage::Substitute, 6) | (Stage::Position, 8) => Self::ChainContext,
            (Stage::Substitute, 8) => Self::ReverseChain,
            (Stage::Position, 2) => Self::Pair,
            (Stage::Position, 3) => Self::Cursive,
            (Stage::Position, 4) => Self::MarkToBase,
            (Stage::Position, 5) => Self::MarkToLigature,
            (Stage::Position, 6) => Self::MarkToMark,
            _ => return None,
        })
    }

    /// The coverage of the first glyph of a subtable of this type in `data`,
    /// for the formats it has; `None` for an unknown format. Every type and
    /// format has it at offset 2 but the format 3 contextual ones, whose first
    /// input coverage comes after a count (or the backtrack).
    pub(crate) fn first_coverage(self, data: Data<'_>) -> Option<Data<'_>> {
        let at = match (self, data.u16(0)) {
            (_, 1) | (Self::Single | Self::Pair | Self::Context | Self::ChainContext, 2) => 2,
            // Glyph count, lookup count, then the coverages.
            (Self::Context, 3) => 6,
            // The input coverages follow the backtrack.
            (Self::ChainContext, 3) => 4 + 2 * data.count(2, 4, 2) + 2,
            _ => return None,
        };
        Some(data.offset16(at))
    }
}

/// A lookup's flags (OpenType's LookupFlag) with its mark filtering set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LookupFlags {
    bits: u16,
    mark_filtering_set: Option<u16>,
}

impl LookupFlags {
    const RIGHT_TO_LEFT: u16 = 0x0001;
    /// The bits that skip glyph classes: each is the bit of the class it skips.
    const IGNORE_CLASSES: u16 = 0x000e;
    const USE_MARK_FILTERING_SET: u16 = 0x0010;
    const MARK_ATTACHMENT_TYPE: u16 = 0xff00;

    /// Flags that skip marks and nothing else.
    pub(crate) const IGNORE_MARKS: Self = Self {
        bits: GlyphClass::Mark as u16,
        mark_filtering_set: None,
    };

    /// The flags of the lookup table at `data`.
    fn read(data: Data<'_>) -> Self {
        let bits = data.u16(2);
        let count = data.count(4, 6, 2);
        let mark_filtering_set =
            (bits & Self::USE_MARK_FILTERING_SET != 0).then(|| data.u16(6 + 2 * count));
        Self {
            bits: bits & !Self::USE_MARK_FILTERING_SET,
            mark_filtering_set,
        }
    }

    /// For cursive attachment: the glyph that does not lead hangs off the other.
    pub(crate) const fn right_to_left(self) -> bool {
        self.bits & Self::RIGHT_TO_LEFT != 0
    }

    /// Whether glyphs of `class` are skipped.
    pub(crate) const fn ignores(self, class: GlyphClass) -> bool {
        self.bits & Self::IGNORE_CLASSES & class as u16 != 0
    }

    /// The mark glyph set that the marks matched must belong to.
    pub(crate) const fn mark_filtering_set(self) -> Option<u16> {
        self.mark_filtering_set
    }

    /// The mark attachment class the marks matched must have; zero for any.
    pub(crate) const fn mark_attachment_class(self) -> u8 {
        ((self.bits & Self::MARK_ATTACHMENT_TYPE) >> 8) as u8
    }

    /// The flags without the classes they skip.
    #[must_use]
    pub(crate) const fn skipping_no_class(self) -> Self {
        Self {
            bits: self.bits & !Self::IGNORE_CLASSES,
            ..self
        }
    }

    /// The flags without a mark filtering set beyond the `sets` GDEF has, as
    /// HarfBuzz sanitises them.
    #[must_use]
    pub(crate) fn with_mark_sets(self, sets: usize) -> Self {
        Self {
            mark_filtering_set: self
                .mark_filtering_set
                .filter(|&set| usize::from(set) < sets),
            ..self
        }
    }
}

/// What a lookup's type says about its subtables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Subtables {
    /// All of this type.
    Of(LookupType),
    /// Extension subtables, each naming its own type.
    Extension,
    /// Of an unknown type.
    Unknown,
}

/// One lookup: its flags and subtables.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lookup<'a> {
    data: Data<'a>,
    stage: Stage,
    subtables: Subtables,
    count: usize,
    pub(crate) flags: LookupFlags,
}

impl<'a> Lookup<'a> {
    /// The lookup of `stage`'s table whose table starts at `data`.
    pub(crate) fn new(stage: Stage, data: Data<'a>) -> Self {
        let kind = data.u16(0);
        let subtables = if kind == stage.extension() {
            Subtables::Extension
        } else {
            LookupType::of(stage, kind).map_or(Subtables::Unknown, Subtables::Of)
        };
        Self {
            data,
            stage,
            subtables,
            count: data.count(4, 6, 2),
            flags: LookupFlags::read(data),
        }
    }

    /// The subtable at `index` with its type, read through an extension
    /// subtable; `None` for an unknown type or an extension of an extension.
    fn subtable(&self, index: usize) -> Option<(LookupType, Data<'a>)> {
        let data = self.data.offset16(6 + 2 * index);
        match self.subtables {
            Subtables::Of(kind) => Some((kind, data)),
            Subtables::Extension => {
                let inner = data.u16(2);
                if data.u16(0) != 1 || inner == self.stage.extension() {
                    return None;
                }
                Some((LookupType::of(self.stage, inner)?, data.offset32(4)))
            }
            Subtables::Unknown => None,
        }
    }

    /// The subtables of a known type, in order.
    pub(crate) fn subtables(&self) -> impl Iterator<Item = (LookupType, Data<'a>)> + 'a {
        let lookup = *self;
        (0..self.count).filter_map(move |i| lookup.subtable(i))
    }

    /// Whether the lookup substitutes in reverse, as its first subtable says.
    pub(crate) fn is_reverse_chain(&self) -> bool {
        self.count > 0
            && self
                .subtable(0)
                .is_some_and(|(kind, _)| kind == LookupType::ReverseChain)
    }
}

/// The coverage index of `glyph`.
pub(crate) fn coverage(table: Data<'_>, glyph: u32) -> Option<u32> {
    match table.u16(0) {
        1 => {
            let count = table.count(2, 4, 2);
            bsearch(count, |i| u32::from(table.u16(4 + 2 * i)).cmp(&glyph)).map(|i| i as u32)
        }
        2 => {
            let count = table.count(2, 4, 6);
            let i = bsearch(count, |i| range_cmp(table, 4 + 6 * i, glyph))?;
            let first = u32::from(table.u16(4 + 6 * i));
            let start = u32::from(table.u16(4 + 6 * i + 4));
            Some(start + glyph - first)
        }
        _ => None,
    }
}

/// How the glyph range at `at` (first, last) compares with `glyph`.
fn range_cmp(table: Data<'_>, at: usize, glyph: u32) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let first = u32::from(table.u16(at));
    let last = u32::from(table.u16(at + 2));
    if glyph < first {
        Ordering::Greater
    } else if glyph > last {
        Ordering::Less
    } else {
        Ordering::Equal
    }
}

/// The class of `glyph` in a class definition; zero when it has none.
pub(crate) fn class(table: Data<'_>, glyph: u32) -> u16 {
    match table.u16(0) {
        1 => {
            let start = u32::from(table.u16(2));
            let count = table.count(4, 6, 2) as u32;
            match glyph.checked_sub(start) {
                Some(i) if i < count => table.u16(6 + 2 * i as usize),
                _ => 0,
            }
        }
        2 => {
            let count = table.count(2, 4, 6);
            bsearch(count, |i| range_cmp(table, 4 + 6 * i, glyph))
                .map_or(0, |i| table.u16(4 + 6 * i + 4))
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutTable, Stage};
    use triergon_core::{Error, WorkBudget};

    /// A GSUB 1.1 table whose only feature variation record has one condition:
    /// an `or` of `children` copies of an `or` of `children` copies of ...
    /// (seven levels) of a false axis range. With 255 children it is a few
    /// kilobytes that would take 255⁷ evaluations.
    fn shared_conditions(children: u8) -> Vec<u8> {
        const LEVELS: usize = 7;
        let mut table = vec![0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 14];
        // Feature variations: version 1.0, one record.
        table.extend([0, 1, 0, 0, 0, 0, 0, 1]);
        table.extend(16_u32.to_be_bytes()); // the condition set, after the record
        table.extend(0_u32.to_be_bytes()); // no substitutions
        // The condition set: one condition, right after it.
        table.extend([0, 1]);
        table.extend(6_u32.to_be_bytes());
        let size = 3 + 3 * usize::from(children);
        for _ in 0..LEVELS {
            table.extend([0, 4, children]);
            for _ in 0..children {
                table.extend(&(size as u32).to_be_bytes()[1..]);
            }
        }
        // An axis range that excludes the default coordinate.
        table.extend([0, 1, 0, 0, 0x40, 0, 0x40, 0]);
        table
    }

    #[test]
    fn shared_conditions_run_out_of_work() {
        let table = shared_conditions(255);
        assert!(table.len() < 6000);
        let gsub = LayoutTable::new(Stage::Substitute, Some(&table));
        let mut work = WorkBudget::new(1 << 20);
        let result = gsub.default_substitutions(&mut work);
        assert_eq!(result.err(), Some(Error::Policy("shaping work")));
        assert_eq!(work.remaining(), 0);
    }

    #[test]
    fn unshared_conditions_cost_one_unit_each() {
        let table = shared_conditions(1);
        let gsub = LayoutTable::new(Stage::Substitute, Some(&table));
        let mut work = WorkBudget::new(1 << 20);
        assert!(gsub.default_substitutions(&mut work).is_ok());
        // The record, seven `or`s and the range.
        assert_eq!(work.spent(), 9);
    }
}
