//! The structures GSUB and GPOS share: script, language system, feature and
//! lookup lists, feature variations at the default instance, coverage and class
//! definitions.

use super::data::{Data, bsearch};
use crate::tag::Tag;

/// A GSUB or GPOS table.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LayoutTable<'a> {
    scripts: Data<'a>,
    features: Data<'a>,
    lookups: Data<'a>,
    /// The feature substitutions that apply at the default instance.
    substitutions: Data<'a>,
}

/// "No feature": the required-feature index of a language system without one.
pub(crate) const NO_FEATURE: u16 = 0xffff;

impl<'a> LayoutTable<'a> {
    pub(crate) fn new(table: &'a [u8]) -> Self {
        let data = Data(table);
        if data.u16(0) != 1 {
            return Self::default();
        }
        let variations = if data.u16(2) >= 1 {
            data.offset32(10)
        } else {
            Data::EMPTY
        };
        Self {
            scripts: data.offset16(4),
            features: data.offset16(6),
            lookups: data.offset16(8),
            substitutions: default_substitutions(variations),
        }
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
            return Tag(0);
        }
        Tag(self.features.u32(2 + 6 * usize::from(index)))
    }

    /// The lookup indices of feature `index`, with the default instance's
    /// feature variations applied.
    pub(crate) fn feature_lookups(&self, index: u16) -> impl Iterator<Item = u16> + 'a {
        let feature = self.substitution(index).unwrap_or_else(|| {
            if usize::from(index) < self.feature_count() {
                self.features.offset16(2 + 6 * usize::from(index) + 4)
            } else {
                Data::EMPTY
            }
        });
        let count = feature.count(2, 4, 2);
        (0..count).map(move |i| feature.u16(4 + 2 * i))
    }

    fn substitution(&self, feature: u16) -> Option<Data<'a>> {
        let subst = self.substitutions;
        let count = subst.count(4, 6, 6);
        let i = bsearch(count, |i| subst.u16(6 + 6 * i).cmp(&feature))?;
        Some(subst.offset32(6 + 6 * i + 2))
    }

    pub(crate) fn lookup_count(&self) -> usize {
        self.lookups.count(0, 2, 2)
    }

    pub(crate) fn lookup(&self, index: u16) -> Option<Lookup<'a>> {
        if usize::from(index) >= self.lookup_count() {
            return None;
        }
        Some(Lookup::new(
            self.lookups.offset16(2 + 2 * usize::from(index)),
        ))
    }

    /// The first feature with `tag` in the whole feature list.
    pub(crate) fn find_feature(&self, tag: Tag) -> Option<u16> {
        (0..self.feature_count())
            .find(|&i| self.features.u32(2 + 6 * i) == tag.0)
            .map(|i| i as u16)
    }
}

/// The record with `tag` among `count` tag-and-offset records from `first`.
fn find_record(list: Data<'_>, first: usize, count: usize, tag: Tag) -> Option<u16> {
    bsearch(count, |i| list.u32(first + 6 * i).cmp(&tag.0)).map(|i| i as u16)
}

/// The feature table substitutions of the first feature-variation record whose
/// conditions all hold at the default instance.
fn default_substitutions(variations: Data<'_>) -> Data<'_> {
    if variations.u16(0) != 1 {
        return Data::EMPTY;
    }
    let declared = variations.u32(4) as usize;
    let count = declared.min(variations.0.len().saturating_sub(8) / 8);
    (0..count)
        .map(|i| 8 + 8 * i)
        .find(|&at| {
            let set = variations.offset32(at);
            let conditions = set.count(0, 2, 4);
            (0..conditions).all(|c| condition_holds(set.offset32(2 + 4 * c), 8))
        })
        .map_or(Data::EMPTY, |at| variations.offset32(at + 4))
}

/// Whether a condition holds with every axis at its default coordinate, zero.
fn condition_holds(condition: Data<'_>, depth: u8) -> bool {
    if depth == 0 {
        return false;
    }
    let nested = |i: usize| condition.at(condition.u24(3 + 3 * i) as usize);
    let nested_count = usize::from(condition.u8(2)).min(condition.0.len().saturating_sub(3) / 3);
    match condition.u16(0) {
        // A null offset is the always-true condition.
        0 => true,
        // Axis range: the default coordinate must lie inside.
        1 => condition.i16(4) <= 0 && 0 <= condition.i16(6),
        // Value: the default value, the deltas being zero at the default.
        2 => condition.i16(2) > 0,
        3 => (0..nested_count).all(|i| condition_holds(nested(i), depth - 1)),
        4 => (0..nested_count).any(|i| condition_holds(nested(i), depth - 1)),
        5 => !condition_holds(condition.at(condition.u24(2) as usize), depth - 1),
        _ => false,
    }
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

/// One lookup: its type, flags and subtables.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lookup<'a> {
    data: Data<'a>,
    pub(crate) kind: u16,
    /// The lookup flags in the low 16 bits and the mark filtering set above, as
    /// HarfBuzz's lookup props.
    pub(crate) props: u32,
}

impl<'a> Lookup<'a> {
    pub(crate) const RIGHT_TO_LEFT: u32 = 0x0001;
    pub(crate) const IGNORE_MARKS: u32 = 0x0008;
    pub(crate) const IGNORE_FLAGS: u32 = 0x000e;
    pub(crate) const USE_MARK_FILTERING_SET: u32 = 0x0010;
    pub(crate) const MARK_ATTACHMENT_TYPE: u32 = 0xff00;

    /// The lookup whose table starts at `data`.
    pub(crate) fn new(data: Data<'a>) -> Self {
        let flags = u32::from(data.u16(2));
        let count = data.count(4, 6, 2);
        let mut props = flags;
        if flags & Self::USE_MARK_FILTERING_SET != 0 {
            props |= u32::from(data.u16(6 + 2 * count)) << 16;
        }
        Self {
            data,
            kind: data.u16(0),
            props,
        }
    }

    pub(crate) fn subtables(&self) -> impl Iterator<Item = Data<'a>> + 'a {
        let data = self.data;
        (0..data.count(4, 6, 2)).map(move |i| data.offset16(6 + 2 * i))
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
