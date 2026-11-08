//! The Arabic joining and fallback shaping data of HarfBuzz's Arabic shaper
//! (`hb-ot-shaper-arabic-table.hh`).

use super::code_point::general_category;
use super::{GeneralCategory, tables};

/// How a character joins, as HarfBuzz's Arabic shaper classifies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ArabicJoining {
    /// U: does not join.
    NonJoining,
    /// L: joins the following character only.
    LeftJoining,
    /// R: joins the preceding character only.
    RightJoining,
    /// D, and C (join-causing), which HarfBuzz treats alike: joins both sides.
    DualJoining,
    /// The Syriac ALAPH joining group (right-joining, with its own final forms).
    Alaph,
    /// The Syriac DALATH and RISH joining groups (right-joining).
    DalathRish,
    /// T: skipped by joining.
    Transparent,
}

/// The joining type HarfBuzz's Arabic shaper gives `cp`: ArabicShaping.txt's,
/// and for characters it does not list, transparent for Mn, Me and Cf and
/// non-joining otherwise.
pub(crate) fn arabic_joining(cp: u32) -> ArabicJoining {
    tables::ARABIC_JOININGS[usize::from(tables::ARABIC_JOINING.get(cp))].unwrap_or_else(|| {
        match general_category(cp) {
            GeneralCategory::NonSpacingMark
            | GeneralCategory::EnclosingMark
            | GeneralCategory::Format => ArabicJoining::Transparent,
            _ => ArabicJoining::NonJoining,
        }
    })
}

/// The positional form of a joining character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ArabicForm {
    /// Joins the following character only.
    Initial,
    /// Joins both sides.
    Medial,
    /// Joins the preceding character only.
    Final,
    /// Joins neither side.
    Isolated,
}

/// Every `(character, presentation form)` of `form`, in code point order: what
/// HarfBuzz's fallback shaping considers for that feature's lookup.
pub(crate) fn arabic_presentation_forms(form: ArabicForm) -> impl Iterator<Item = (u32, u32)> {
    (tables::ARABIC_SHAPING_FIRST..)
        .zip(&tables::ARABIC_SHAPING)
        .filter_map(move |(cp, forms)| {
            let presentation = u32::from(forms[form as usize]);
            (presentation != 0).then_some((cp, presentation))
        })
}

/// A ligature of HarfBuzz's fallback shaping: the presentation forms of the
/// components after the first, and the ligature they form.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ArabicLigature<const N: usize> {
    pub(crate) components: [u32; N],
    pub(crate) ligature: u32,
}

/// The ligatures that start with one presentation form, in the order HarfBuzz
/// lists them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ArabicLigatureSet<const N: usize> {
    pub(crate) first: u32,
    pub(crate) ligatures: &'static [ArabicLigature<N>],
}

/// The two-letter ligatures (`ligature_table`, the second `rlig` lookup), sorted by
/// first component.
pub(crate) fn arabic_ligatures() -> &'static [ArabicLigatureSet<1>] {
    &tables::ARABIC_LIGATURES
}

/// The ligatures of SHADDA with a mark (`ligature_mark_table`, the third `rlig`
/// lookup, which does not skip marks).
pub(crate) fn arabic_mark_ligatures() -> &'static [ArabicLigatureSet<1>] {
    &tables::ARABIC_MARK_LIGATURES
}

/// The three-letter ligatures (`ligature_3_table`, the first `rlig` lookup).
pub(crate) fn arabic_ligatures_3() -> &'static [ArabicLigatureSet<2>] {
    &tables::ARABIC_LIGATURES_3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joining() {
        use ArabicJoining::*;
        for (cp, joining) in [
            (0x0627, RightJoining),
            (0x0644, DualJoining),
            (0x0640, DualJoining),
            (0x200D, DualJoining),
            (0x0710, Alaph),
            (0x0715, DalathRish),
            (0x064B, Transparent),
            (0x0301, Transparent),
            (0x200B, Transparent),
            (0x05D0, NonJoining),
            (0x0041, NonJoining),
            (0x110000, NonJoining),
        ] {
            assert_eq!(arabic_joining(cp), joining, "U+{cp:04X}");
        }
    }

    #[test]
    fn presentation_forms() {
        use ArabicForm::*;
        let form = |cp: u32, form| {
            arabic_presentation_forms(form).find_map(|(c, f)| (c == cp).then_some(f))
        };
        assert_eq!(form(0x0628, Initial), Some(0xFE91));
        assert_eq!(form(0x0628, Medial), Some(0xFE92));
        assert_eq!(form(0x0628, Final), Some(0xFE90));
        assert_eq!(form(0x0628, Isolated), Some(0xFE8F));
        assert_eq!(form(0x0627, Initial), None);
        assert_eq!(form(0x0627, Final), Some(0xFE8E));
        assert_eq!(form(0x0620, Isolated), None);
        assert_eq!(form(0x0041, Isolated), None);
        let finals: Vec<_> = arabic_presentation_forms(Final).take(2).collect();
        assert_eq!(finals, [(0x0622, 0xFE82), (0x0623, 0xFE84)]);
    }

    #[test]
    fn ligatures() {
        let lam = arabic_ligatures()
            .iter()
            .find(|set| set.first == 0xFEDF)
            .unwrap();
        assert!(lam.ligatures.contains(&ArabicLigature {
            components: [0xFE8E],
            ligature: 0xFEFB
        }));
        assert!(arabic_ligatures().is_sorted_by_key(|set| set.first));
        let shadda = &arabic_mark_ligatures()[0];
        assert_eq!(shadda.first, 0x0651);
        assert_eq!(shadda.ligatures.last().unwrap().ligature, 0xF2EE);
        assert_eq!(arabic_ligatures_3()[0].first, 0xFEDF);
    }
}
