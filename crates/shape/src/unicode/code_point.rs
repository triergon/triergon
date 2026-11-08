//! HarfBuzz's Unicode functions over any `u32`, as a buffer holds code points:
//! values that are not scalar values get HarfBuzz's defaults.

use super::{GeneralCategory, tables};
use crate::Script;

/// `hb_unicode_general_category`.
#[inline]
pub(crate) fn general_category(cp: u32) -> GeneralCategory {
    GeneralCategory::from_index(tables::GENERAL_CATEGORY.get(cp))
}

/// `hb_unicode_combining_class`: the Canonical_Combining_Class.
#[inline]
pub(crate) fn combining_class(cp: u32) -> u8 {
    tables::COMBINING_CLASS.get(cp)
}

/// `hb_unicode_script`, as an ISO 15924 code; [`Script::UNKNOWN`] for unassigned
/// code points.
#[inline]
pub(crate) fn script(cp: u32) -> Script {
    tables::SCRIPTS[usize::from(tables::SCRIPT.get(cp))]
}

/// `hb_unicode_mirroring`: the Bidi_Mirroring_Glyph, or `cp` itself.
#[inline]
pub(crate) fn mirroring(cp: u32) -> u32 {
    let delta = tables::MIRRORING_DELTAS[usize::from(tables::MIRRORING.get(cp))];
    cp.wrapping_add_signed(delta)
}

/// `hb_unicode_is_emoji_Extended_Pictographic` (HarfBuzz's emoji table).
#[inline]
pub(crate) fn is_extended_pictographic(cp: u32) -> bool {
    tables::EXTENDED_PICTOGRAPHIC.get(cp) != 0
}

/// Hangul syllables, composed of a leading consonant, a vowel and optionally a
/// trailing consonant (Unicode 3.12, Conjoining Jamo Behavior).
mod hangul {
    pub(super) const SYLLABLE: u32 = 0xAC00;
    pub(super) const LEADING: u32 = 0x1100;
    pub(super) const VOWEL: u32 = 0x1161;
    /// One before the first trailing consonant: index 0 means none.
    pub(super) const TRAILING: u32 = 0x11A7;
    pub(super) const LEADINGS: u32 = 19;
    pub(super) const VOWELS: u32 = 21;
    pub(super) const TRAILINGS: u32 = 28;
    pub(super) const SYLLABLES: u32 = LEADINGS * VOWELS * TRAILINGS;
}

/// `hb_unicode_decompose`: the canonical decomposition of `cp` into one or two
/// characters, `(a, 0)` for a singleton. Hangul syllables decompose
/// algorithmically: LV into L and V, LVT into LV and T.
pub(crate) fn decompose(cp: u32) -> Option<(u32, u32)> {
    use hangul::*;
    let syllable = cp.wrapping_sub(SYLLABLE);
    if syllable < SYLLABLES {
        let trailing = syllable % TRAILINGS;
        return Some(if trailing == 0 {
            let leading = syllable / (VOWELS * TRAILINGS);
            let vowel = syllable / TRAILINGS % VOWELS;
            (LEADING + leading, VOWEL + vowel)
        } else {
            (cp - trailing, TRAILING + trailing)
        });
    }
    let index = usize::from(tables::DECOMPOSITION.get(cp));
    let singles = tables::SINGLE_DECOMPOSITIONS.len();
    match index {
        0 => None,
        i if i <= singles => Some((tables::SINGLE_DECOMPOSITIONS[i - 1], 0)),
        i => Some(tables::PAIR_DECOMPOSITIONS[i - 1 - singles]),
    }
}

/// `hb_unicode_compose`: the primary composite of `a` and `b`. Hangul composes
/// algorithmically: L and V into LV, LV and T into LVT.
pub(crate) fn compose(a: u32, b: u32) -> Option<u32> {
    use hangul::*;
    let leading = a.wrapping_sub(LEADING);
    let vowel = b.wrapping_sub(VOWEL);
    if leading < LEADINGS && vowel < VOWELS {
        return Some(SYLLABLE + (leading * VOWELS + vowel) * TRAILINGS);
    }
    let syllable = a.wrapping_sub(SYLLABLE);
    let trailing = b.wrapping_sub(TRAILING);
    if syllable < SYLLABLES
        && syllable.is_multiple_of(TRAILINGS)
        && (1..TRAILINGS).contains(&trailing)
    {
        return Some(a + trailing);
    }
    let compositions = &tables::COMPOSITIONS;
    compositions
        .binary_search_by_key(&(a, b), |&(a, b, _)| (a, b))
        .ok()
        .map(|i| compositions[i].2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unicode::arabic_joining;

    #[test]
    fn general_categories() {
        use GeneralCategory::*;
        for (cp, category) in [
            (0x0041, UppercaseLetter),
            (0x0627, OtherLetter),
            (0x0301, NonSpacingMark),
            (0x05D0, OtherLetter),
            (0x1F600, OtherSymbol),
            (0xAC00, OtherLetter),
            (0xD800, Surrogate),
            (0xE000, PrivateUse),
            (0x200D, Format),
            (0x0378, Unassigned),
            (0x10FFFF, Unassigned),
            (0x110000, Unassigned),
            (u32::MAX, Unassigned),
        ] {
            assert_eq!(general_category(cp), category, "U+{cp:04X}");
        }
    }

    #[test]
    fn combining_classes_and_scripts() {
        assert_eq!(combining_class(0x0301), 230);
        assert_eq!(combining_class(0x0316), 220);
        assert_eq!(combining_class(0x05B0), 10);
        assert_eq!(combining_class(0x0041), 0);
        assert_eq!(combining_class(0x110000), 0);
        for (cp, tag) in [
            (0x0041, b"Latn"),
            (0x0627, b"Arab"),
            (0x05D0, b"Hebr"),
            (0x0301, b"Zinh"),
            (0x0020, b"Zyyy"),
            (0x1F600, b"Zyyy"),
            (0xAC00, b"Hang"),
            (0x4E00, b"Hani"),
            (0x0378, b"Zzzz"),
            (0xD800, b"Zzzz"),
            (0x110000, b"Zzzz"),
        ] {
            assert_eq!(script(cp), Script::from_code(*tag), "U+{cp:04X}");
        }
    }

    #[test]
    fn mirroring_and_emoji() {
        assert_eq!(mirroring(0x0028), 0x0029);
        assert_eq!(mirroring(0x0029), 0x0028);
        assert_eq!(mirroring(0x2329), 0x232A);
        assert_eq!(mirroring(0x0041), 0x0041);
        assert_eq!(mirroring(u32::MAX), u32::MAX);
        assert!(is_extended_pictographic(0x1F600));
        assert!(is_extended_pictographic(0x00A9));
        assert!(!is_extended_pictographic(0x0041));
        assert!(!is_extended_pictographic(0x110000));
    }

    #[test]
    fn decomposition_and_composition() {
        assert_eq!(decompose(0x00C0), Some((0x0041, 0x0300)));
        assert_eq!(decompose(0x1E0A), Some((0x0044, 0x0307)));
        assert_eq!(decompose(0x212B), Some((0x00C5, 0)));
        assert_eq!(decompose(0x2F800), Some((0x4E3D, 0)));
        assert_eq!(decompose(0x0344), Some((0x0308, 0x0301)));
        assert_eq!(decompose(0xAC00), Some((0x1100, 0x1161)));
        assert_eq!(decompose(0xAC01), Some((0xAC00, 0x11A8)));
        assert_eq!(decompose(0xD7A3), Some((0xD788, 0x11C2)));
        assert_eq!(decompose(0x0041), None);
        assert_eq!(decompose(0xFB01), None, "compatibility only");
        assert_eq!(decompose(u32::MAX), None);

        assert_eq!(compose(0x0041, 0x0300), Some(0x00C0));
        assert_eq!(compose(0x1100, 0x1161), Some(0xAC00));
        assert_eq!(compose(0xAC00, 0x11A8), Some(0xAC01));
        assert_eq!(
            compose(0xAC01, 0x11A8),
            None,
            "LVT does not compose further"
        );
        assert_eq!(compose(0xAC00, 0x11A7), None);
        assert_eq!(compose(0x0915, 0x093C), None, "composition exclusion");
        assert_eq!(compose(0x0308, 0x0301), None, "non-starter decomposition");
        assert_eq!(compose(0x0041, 0), None);
        assert_eq!(compose(0, 0x0300), None);
    }

    #[test]
    fn every_code_point_is_looked_up() {
        for cp in (0..=0x11_0000).chain([u32::MAX]) {
            let _ = (
                general_category(cp),
                combining_class(cp),
                script(cp),
                mirroring(cp),
            );
            let _ = (
                decompose(cp),
                is_extended_pictographic(cp),
                arabic_joining(cp),
            );
        }
    }
}
