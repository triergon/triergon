//! Whether a substitution lookup would apply to a glyph sequence on its own,
//! without a buffer: HarfBuzz's `hb_ot_layout_lookup_would_substitute`, which
//! the Indic shaper asks to learn which consonants have special forms.

use super::data::Data;
use super::layout::{LayoutTable, class, coverage};

const SINGLE: u16 = 1;
const MULTIPLE: u16 = 2;
const ALTERNATE: u16 = 3;
const LIGATURE: u16 = 4;
const CONTEXT: u16 = 5;
const CHAIN_CONTEXT: u16 = 6;
const EXTENSION: u16 = 7;
const REVERSE_CHAIN: u16 = 8;

/// Whether GSUB lookup `index` substitutes exactly `glyphs`; with
/// `zero_context`, contextual rules count only without backtrack or lookahead.
///
/// As in HarfBuzz, the first glyph must pass the lookup's coverage digest, and
/// then format 2 and 3 contextual subtables check the classes and coverages of
/// the glyphs after it only.
pub(crate) fn would_substitute(
    gsub: &LayoutTable<'_>,
    index: u16,
    glyphs: &[u32],
    zero_context: bool,
) -> bool {
    let Some(lookup) = gsub.lookup(index) else {
        return false;
    };
    let Some(&first) = glyphs.first() else {
        return false;
    };
    let subtables = || {
        lookup
            .subtables()
            .filter_map(move |data| resolve(lookup.kind, data))
    };
    let mut digest = Digest::default();
    for (kind, data) in subtables() {
        if let Some(coverage) = first_coverage(kind, data) {
            digest.add_coverage(coverage);
        }
    }
    digest.may_have(first)
        && subtables().any(|(kind, data)| applies(kind, data, glyphs, zero_context))
}

/// HarfBuzz's set digest, the filter its lookup accelerators test the first
/// glyph with: for each of three shifts, a 64-bit mask of the shifted glyph ids
/// modulo 64. It may hold glyphs that are not in the set.
#[derive(Default)]
struct Digest([u64; 3]);

impl Digest {
    const SHIFTS: [u32; 3] = [4, 0, 6];

    fn bit(glyph: u32, shift: u32) -> u64 {
        1 << ((glyph >> shift) & 63)
    }

    fn add(&mut self, glyph: u32) {
        for (mask, shift) in self.0.iter_mut().zip(Self::SHIFTS) {
            *mask |= Self::bit(glyph, shift);
        }
    }

    /// Adds `first..=last`; false once every mask is full.
    fn add_range(&mut self, first: u32, last: u32) -> bool {
        if self.0.iter().all(|&mask| mask == u64::MAX) {
            return false;
        }
        for (mask, shift) in self.0.iter_mut().zip(Self::SHIFTS) {
            if (last >> shift).wrapping_sub(first >> shift) >= 63 {
                *mask = u64::MAX;
            } else {
                let (a, b) = (Self::bit(first, shift), Self::bit(last, shift));
                *mask |= b
                    .wrapping_add(b.wrapping_sub(a))
                    .wrapping_sub(u64::from(b < a));
            }
        }
        true
    }

    fn add_coverage(&mut self, table: Data<'_>) {
        match table.u16(0) {
            1 => {
                for i in 0..table.count(2, 4, 2) {
                    self.add(u32::from(table.u16(4 + 2 * i)));
                }
            }
            2 => {
                for i in 0..table.count(2, 4, 6) {
                    let at = 4 + 6 * i;
                    if !self.add_range(u32::from(table.u16(at)), u32::from(table.u16(at + 2))) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    fn may_have(&self, glyph: u32) -> bool {
        self.0
            .iter()
            .zip(Self::SHIFTS)
            .all(|(&mask, shift)| mask & Self::bit(glyph, shift) != 0)
    }
}

/// The type and data of a subtable, through an extension.
fn resolve(kind: u16, data: Data<'_>) -> Option<(u16, Data<'_>)> {
    if kind != EXTENSION {
        return Some((kind, data));
    }
    let inner = data.u16(2);
    (data.u16(0) == 1 && inner != EXTENSION).then(|| (inner, data.offset32(4)))
}

/// The coverage of a subtable's first glyph.
fn first_coverage(kind: u16, data: Data<'_>) -> Option<Data<'_>> {
    Some(match (kind, data.u16(0)) {
        (SINGLE, 1 | 2) | (MULTIPLE | ALTERNATE | LIGATURE | REVERSE_CHAIN, 1) => data.offset16(2),
        (CONTEXT | CHAIN_CONTEXT, 1 | 2) => data.offset16(2),
        (CONTEXT, 3) => data.offset16(6),
        (CHAIN_CONTEXT, 3) => {
            let at = 4 + 2 * usize::from(data.u16(2));
            data.offset16(at + 2)
        }
        _ => return None,
    })
}

fn applies(kind: u16, data: Data<'_>, glyphs: &[u32], zero_context: bool) -> bool {
    let first = glyphs[0];
    let covered = |data: Data<'_>| coverage(data.offset16(2), first);
    match (kind, data.u16(0)) {
        (SINGLE, 1 | 2) | (MULTIPLE | ALTERNATE | REVERSE_CHAIN, 1) => {
            glyphs.len() == 1 && covered(data).is_some()
        }
        (LIGATURE, 1) => {
            let Some(index) = covered(data) else {
                return false;
            };
            let set = nth_offset(data, 4, index as usize);
            (0..set.count(0, 2, 2)).any(|l| {
                let ligature = set.offset16(2 + 2 * l);
                usize::from(ligature.u16(2)) == glyphs.len()
                    && matches(glyphs, ligature.at(4), |glyph, value| {
                        glyph == u32::from(value)
                    })
            })
        }
        (CONTEXT | CHAIN_CONTEXT, format @ (1 | 2)) => {
            let chained = kind == CHAIN_CONTEXT;
            let (set, classes) = if format == 1 {
                let Some(index) = covered(data) else {
                    return false;
                };
                (nth_offset(data, 4, index as usize), None)
            } else {
                let (classes, sets_at) = if chained {
                    (data.offset16(6), 10)
                } else {
                    (data.offset16(4), 6)
                };
                let set = nth_offset(data, sets_at, usize::from(class(classes, first)));
                (set, Some(classes))
            };
            let same = |glyph: u32, value: u16| match classes {
                Some(classes) => class(classes, glyph) == value,
                None => glyph == u32::from(value),
            };
            (0..set.count(0, 2, 2)).any(|r| {
                let rule = set.offset16(2 + 2 * r);
                let (backtrack, inputs, input, lookahead) = if chained {
                    let backtrack = usize::from(rule.u16(0));
                    let at = 2 + 2 * backtrack;
                    let inputs = usize::from(rule.u16(at));
                    let lookahead_at = at + 2 + 2 * inputs.saturating_sub(1);
                    (
                        backtrack,
                        inputs,
                        rule.at(at + 2),
                        usize::from(rule.u16(lookahead_at)),
                    )
                } else {
                    (0, usize::from(rule.u16(0)), rule.at(4), 0)
                };
                context_allowed(zero_context, backtrack, lookahead)
                    && inputs == glyphs.len()
                    && matches(glyphs, input, same)
            })
        }
        (CONTEXT, 3) => {
            usize::from(data.u16(2)) == glyphs.len()
                && matches(glyphs, data.at(8), |glyph, offset| {
                    coverage(data.at(usize::from(offset)), glyph).is_some()
                })
        }
        (CHAIN_CONTEXT, 3) => {
            let backtrack = usize::from(data.u16(2));
            let input_at = 4 + 2 * backtrack;
            let inputs = usize::from(data.u16(input_at));
            let lookahead = usize::from(data.u16(input_at + 2 + 2 * inputs));
            context_allowed(zero_context, backtrack, lookahead)
                && inputs == glyphs.len()
                && matches(glyphs, data.at(input_at + 4), |glyph, offset| {
                    coverage(data.at(usize::from(offset)), glyph).is_some()
                })
        }
        _ => false,
    }
}

fn context_allowed(zero_context: bool, backtrack: usize, lookahead: usize) -> bool {
    !zero_context || (backtrack == 0 && lookahead == 0)
}

/// Whether the glyphs after the first match the 16-bit values at `values`.
fn matches(glyphs: &[u32], values: Data<'_>, same: impl Fn(u32, u16) -> bool) -> bool {
    glyphs[1..]
        .iter()
        .enumerate()
        .all(|(i, &glyph)| same(glyph, values.u16(2 * i)))
}

/// The `index`th of the counted array of 16-bit offsets at `at`; empty beyond it.
fn nth_offset(data: Data<'_>, at: usize, index: usize) -> Data<'_> {
    if index < data.count(at, at + 2, 2) {
        data.offset16(at + 2 + 2 * index)
    } else {
        Data::EMPTY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_holds_its_glyphs_and_some_others() {
        let mut digest = Digest::default();
        // Coverage format 2: glyphs 10 to 20.
        digest.add_coverage(Data(&[0, 2, 0, 1, 0, 10, 0, 20, 0, 0]));
        assert!((10..=20).all(|g| digest.may_have(g)));
        assert!(!digest.may_have(9));
        // 10 + 64 matches every mask: a false positive HarfBuzz has too.
        assert!(digest.may_have(10 + 64 * 1024));
        let mut wide = Digest::default();
        assert!(wide.add_range(0, 5000));
        assert!(!wide.add_range(0, 1));
        assert!(wide.may_have(123_456));
    }

    #[test]
    fn single_and_ligature_subtables_need_their_length() {
        // A ligature subtable: coverage of glyph 5, one set, one ligature 9 of
        // glyphs 5 and 6.
        let ligature: &[u8] = &[
            0, 1, 0, 8, 0, 1, 0, 14, // format, coverage offset, one set at 14
            0, 1, 0, 1, 0, 5, // coverage format 1: glyph 5
            0, 1, 0, 4, // set: one ligature at 4
            0, 9, 0, 2, 0, 6, // ligature 9, two components, then glyph 6
        ];
        let data = Data(ligature);
        assert!(applies(LIGATURE, data, &[5, 6], true));
        assert!(!applies(LIGATURE, data, &[5, 7], true));
        assert!(!applies(LIGATURE, data, &[5], true));
        assert!(!applies(
            SINGLE,
            Data(&[0, 1, 0, 6, 0, 1, 0, 1, 0, 1, 0, 5]),
            &[5, 6],
            true
        ));
    }
}
