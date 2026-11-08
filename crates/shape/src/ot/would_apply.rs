//! Whether a substitution lookup would apply to a glyph sequence on its own,
//! without a buffer: HarfBuzz's `hb_ot_layout_lookup_would_substitute`, which
//! the Indic shaper asks to learn which consonants have special forms.

use super::context::{self, FirstGlyph, Rule};
use super::data::Data;
use super::digest::Digest;
use super::layout::{LayoutTable, LookupType, coverage};
use crate::work;
use triergon_core::{Result, WorkBudget};

/// Whether GSUB lookup `index`, whose digest is `digest`, substitutes exactly
/// `glyphs`; with `zero_context`, contextual rules count only without
/// backtrack or lookahead. Each subtable and rule tried is charged to `work`.
///
/// As in HarfBuzz, the first glyph must pass the lookup's digest, and then
/// format 2 and 3 contextual subtables check the classes and coverages of the
/// glyphs after it only.
pub(crate) fn would_substitute(
    gsub: &LayoutTable<'_>,
    index: u16,
    digest: &Digest,
    glyphs: &[u32],
    zero_context: bool,
    work: &mut WorkBudget,
) -> Result<bool> {
    let (Some(lookup), Some(&first)) = (gsub.lookup(index), glyphs.first()) else {
        return Ok(false);
    };
    if !digest.may_have(first) {
        return Ok(false);
    }
    for (kind, data) in lookup.subtables() {
        work::charge(work, 1)?;
        if applies(kind, data, glyphs, zero_context, work)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn applies(
    kind: LookupType,
    data: Data<'_>,
    glyphs: &[u32],
    zero_context: bool,
    work: &mut WorkBudget,
) -> Result<bool> {
    let first = glyphs[0];
    let covered = || coverage(data.offset16(2), first);
    let format = data.u16(0);
    Ok(match kind {
        LookupType::Single if matches!(format, 1 | 2) => glyphs.len() == 1 && covered().is_some(),
        LookupType::Multiple | LookupType::Alternate | LookupType::ReverseChain if format == 1 => {
            glyphs.len() == 1 && covered().is_some()
        }
        LookupType::Ligature if format == 1 => {
            let Some(index) = covered() else {
                return Ok(false);
            };
            let index = index as usize;
            let set = if index < data.count(4, 6, 2) {
                data.offset16(6 + 2 * index)
            } else {
                Data::EMPTY
            };
            for l in 0..set.count(0, 2, 2) {
                work::charge(work, 1)?;
                let ligature = set.offset16(2 + 2 * l);
                let rest = ligature.at(4);
                if ligature.headless_count(2, 4, 2) == glyphs.len()
                    && glyphs[1..]
                        .iter()
                        .enumerate()
                        .all(|(i, &glyph)| u32::from(rest.u16(2 * i)) == glyph)
                {
                    return Ok(true);
                }
            }
            false
        }
        LookupType::Context | LookupType::ChainContext => {
            let chained = kind == LookupType::ChainContext;
            context::any_rule(data, chained, first, FirstGlyph::Unchecked, |rule| {
                work::charge(work, 1)?;
                Ok(matches(rule, glyphs, zero_context))
            })?
        }
        _ => false,
    })
}

/// Whether `rule` reads exactly `glyphs` as its input, and needs no context
/// with `zero_context`.
fn matches(rule: &Rule<'_>, glyphs: &[u32], zero_context: bool) -> bool {
    (!zero_context || rule.has_no_context())
        && rule.input.len == glyphs.len()
        && glyphs[1..]
            .iter()
            .enumerate()
            .all(|(i, &glyph)| rule.input.values.matches_glyph(glyph, i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ot::layout::{Lookup, Stage};

    fn applies(kind: LookupType, data: &[u8], glyphs: &[u32]) -> bool {
        super::applies(kind, Data(data), glyphs, true, &mut WorkBudget::new(100)).expect("work")
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
        assert!(applies(LookupType::Ligature, ligature, &[5, 6]));
        assert!(!applies(LookupType::Ligature, ligature, &[5, 7]));
        assert!(!applies(LookupType::Ligature, ligature, &[5]));
        let single = [0, 1, 0, 6, 0, 1, 0, 1, 0, 1, 0, 5];
        assert!(!applies(LookupType::Single, &single, &[5, 6]));
        assert!(applies(LookupType::Single, &single, &[5]));
    }

    #[test]
    fn format_3_context_reads_only_the_glyphs_after_the_first() {
        // Context format 3: two input glyphs, no lookups; the first coverage
        // holds glyph 1, the second glyph 2.
        let context: &[u8] = &[
            0, 3, 0, 2, 0, 0, 0, 10, 0,
            16, // format, glyph count, lookup count, two coverages
            0, 1, 0, 1, 0, 1, // coverage: glyph 1
            0, 1, 0, 1, 0, 2, // coverage: glyph 2
        ];
        assert!(applies(LookupType::Context, context, &[1, 2]));
        assert!(
            applies(LookupType::Context, context, &[7, 2]),
            "the digest alone checks the first glyph"
        );
        assert!(!applies(LookupType::Context, context, &[1, 3]));
        let _ = Lookup::new(Stage::Substitute, Data(context));
    }
}
