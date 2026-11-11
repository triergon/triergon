//! Scripts, styles and which style hints each glyph of a face, as FreeType's
//! auto-hinter assigns them (`af_face_globals_compute_style_coverage`) from
//! the character ranges of its scripts. Without HarfBuzz no style reaches a
//! glyph through OpenType features, so only the characters of the face's
//! Unicode charmap count.

use super::tables::{SCRIPTS, STYLE_FALLBACK, STYLES};
use crate::cmap::Charmap;
use crate::work::charge;
use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

/// The hinter of a style (`AF_WritingSystem`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WritingSystem {
    /// No hinting: the outline is only scaled (`afdummy.c`).
    Dummy,
    /// Blue zones and stems (`aflatin.c`), the hinter of most scripts.
    Latin,
    /// CJK ideographs (`afcjk.c`).
    Cjk,
    /// Indic scripts, hinted by the CJK routines (`afindic.c`).
    Indic,
}

/// A script (`AF_ScriptClassRec`): the characters it covers, those of them
/// that are not base characters, the direction its horizontal edges are
/// sorted in, and the characters its standard stem widths are measured on.
#[derive(Debug)]
pub(crate) struct Script {
    /// Inclusive ranges of code points.
    pub(crate) ranges: &'static [(u32, u32)],
    /// Inclusive ranges of the code points blue zones do not apply to.
    pub(crate) nonbase: &'static [(u32, u32)],
    pub(crate) top_to_bottom: bool,
    /// Space-separated clusters; the first one the face maps wins.
    pub(crate) standard: &'static str,
}

/// A style (`AF_StyleClassRec`): a script hinted by a writing system.
#[derive(Debug)]
pub(crate) struct Style {
    pub(crate) writing_system: WritingSystem,
    /// The index of the script in [`SCRIPTS`].
    pub(crate) script: usize,
    /// The blue strings of a style of the latin or CJK writing system.
    pub(crate) blues: &'static [BlueString],
    /// Whether the style covers its script's characters
    /// (`AF_COVERAGE_DEFAULT`) rather than the glyphs of an OpenType feature.
    pub(crate) default_coverage: bool,
}

/// A blue string (`AF_Blue_StringRec`): space-separated clusters whose
/// extrema give one blue zone, and the zone's properties
/// (`AF_BLUE_PROPERTY_LATIN_*` or `AF_BLUE_PROPERTY_CJK_*`).
#[derive(Debug)]
pub(crate) struct BlueString {
    pub(crate) text: &'static str,
    pub(crate) properties: u8,
}

/// The bits of a glyph's entry in [`Coverage`] that hold its style.
pub(crate) const STYLE_MASK: u16 = 0x1FFF;
/// A glyph no style covers yet (`AF_STYLE_UNASSIGNED`).
const UNASSIGNED: u16 = STYLE_MASK;
/// The glyph of an ASCII digit (`AF_DIGIT`).
pub(crate) const DIGIT: u16 = 0x8000;
/// A glyph blue zones do not apply to (`AF_NONBASE`).
pub(crate) const NONBASE: u16 = 0x4000;
/// A glyph the charmap maps a character to (`AF_HAS_CMAP_ENTRY`).
const HAS_CMAP_ENTRY: u16 = 0x2000;

/// The characters FreeType's cluster reader without HarfBuzz
/// (`af_shaper_get_cluster`) finds in a string of space-separated clusters:
/// each cluster's character, or `None` for a cluster of several characters,
/// which it skips.
pub(crate) fn clusters(text: &'static str) -> impl Iterator<Item = Option<char>> {
    text.split(' ').filter(|c| !c.is_empty()).map(|cluster| {
        let mut chars = cluster.chars();
        let first = chars.next();
        chars.next().is_none().then_some(first).flatten()
    })
}

/// The style of each glyph of a face and its `DIGIT`, `NONBASE` and
/// `HAS_CMAP_ENTRY` bits: FreeType's `glyph_styles`.
#[derive(Debug)]
pub(crate) struct Coverage {
    entries: BudgetVec<u16>,
}

impl Coverage {
    /// `af_face_globals_compute_style_coverage` through `charmap`, the
    /// face's Unicode charmap (`None` without one: every glyph then takes
    /// the fallback style).
    ///
    /// Styles are visited in order and each gives its script's characters
    /// to the glyphs no earlier style took; a style then marks those of its
    /// glyphs a non-base character maps to. The glyphs of the ASCII digits
    /// are marked, and every glyph left takes the fallback style, CJK.
    pub(crate) fn new(
        charmap: Option<Charmap<'_>>,
        glyph_count: u16,
        budget: &Budget,
        work: &mut WorkBudget,
    ) -> Result<Self> {
        let mut entries = BudgetVec::try_filled(budget, UNASSIGNED, usize::from(glyph_count))?;
        if let Some(charmap) = charmap {
            // Every mapping by code, as `FT_Get_Next_Char` steps through them.
            let mut mappings = BudgetVec::new(budget);
            for mapping in charmap.mappings() {
                let (code, glyph) = mapping?;
                mappings.try_push((code, glyph.0)).map_err(|e| e.error)?;
            }
            charge(work, mappings.len())?;
            mappings.as_mut_slice().sort_unstable();
            let mappings = mappings.as_slice();
            let within = |first: u32, last: u32| {
                let start = mappings.partition_point(|&(code, _)| code < first);
                mappings[start..]
                    .iter()
                    .take_while(move |&&(code, _)| code <= last)
            };
            for (index, style) in STYLES.iter().enumerate() {
                // Other coverages are OpenType features, found only by HarfBuzz.
                if !style.default_coverage {
                    continue;
                }
                let style_index = index as u16;
                let script = &SCRIPTS[style.script];
                for &(first, last) in script.ranges {
                    for &(_, glyph) in within(first, last) {
                        charge(work, 1)?;
                        let entry = &mut entries[usize::from(glyph)];
                        if *entry & STYLE_MASK == UNASSIGNED {
                            *entry = style_index | HAS_CMAP_ENTRY;
                        }
                    }
                }
                for &(first, last) in script.nonbase {
                    for &(_, glyph) in within(first, last) {
                        charge(work, 1)?;
                        let entry = &mut entries[usize::from(glyph)];
                        if *entry & STYLE_MASK == style_index {
                            *entry |= NONBASE;
                        }
                    }
                }
            }
            for digit in 0x30..=0x39 {
                if let Some(glyph) = charmap.glyph(digit) {
                    entries[usize::from(glyph.0)] |= DIGIT;
                }
            }
        }
        for entry in entries.iter_mut() {
            if *entry & STYLE_MASK == UNASSIGNED {
                *entry = (*entry & !STYLE_MASK) | STYLE_FALLBACK;
            }
        }
        Ok(Self { entries })
    }

    /// The entry of `glyph`, a glyph of the face.
    pub(crate) fn entry(&self, glyph: u16) -> u16 {
        self.entries
            .get(usize::from(glyph))
            .copied()
            .unwrap_or(STYLE_FALLBACK)
    }

    /// The style of `glyph`, an index into [`STYLES`].
    pub(crate) fn style(&self, glyph: u16) -> usize {
        usize::from(self.entry(glyph) & STYLE_MASK)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tables::{SCRIPTS, STYLE_FALLBACK, STYLES};
    use super::{WritingSystem, clusters};

    #[test]
    fn clusters_of_several_characters_are_skipped() {
        let found: Vec<_> = clusters("o  O \u{915}\u{94d} 0").collect();
        assert_eq!(found, [Some('o'), Some('O'), None, Some('0')]);
    }

    #[test]
    fn the_tables_name_the_styles_the_loader_relies_on() {
        let fallback = &STYLES[usize::from(STYLE_FALLBACK)];
        assert_eq!(fallback.writing_system, WritingSystem::Cjk);
        // Its blue zones are a top and a bottom one, each string's
        // reference characters before a `|` and its overshoot ones after.
        let properties: Vec<_> = fallback.blues.iter().map(|b| b.properties).collect();
        assert_eq!(properties, [0x01, 0x00]);
        assert!(fallback.blues[0].text.starts_with("\u{4ed6} \u{4eec} "));
        assert!(fallback.blues.iter().all(|b| b.text.contains(" | ")));
        // The dummy style covers no characters.
        let dummy: Vec<_> = STYLES
            .iter()
            .filter(|s| s.writing_system == WritingSystem::Dummy)
            .collect();
        assert_eq!(dummy.len(), 1);
        assert!(SCRIPTS[dummy[0].script].ranges.is_empty());
        // The Latin default style measures its stems on "o O 0".
        let latin = STYLES
            .iter()
            .find(|s| SCRIPTS[s.script].standard == "o O 0" && s.default_coverage)
            .expect("a Latin default style");
        assert_eq!(latin.writing_system, WritingSystem::Latin);
        assert_eq!(latin.blues.len(), 6);
        assert_eq!(latin.blues[0].text, "T H E Z O C Q S");
    }
}
