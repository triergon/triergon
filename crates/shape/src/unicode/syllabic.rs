//! Character categories of the syllabic shapers: the Indic, Khmer and Myanmar
//! categories and positions, the Universal Shaping Engine categories and the
//! vowel sequences that get a dotted circle, as HarfBuzz 14.6.0 tabulates them.

use super::syllabic_tables::{INDIC, INDIC_PROPERTIES, USE, USE_CATEGORIES, VOWEL_CONSTRAINTS};
use crate::Script;

/// How the Indic, Khmer and Myanmar shapers classify a character, from its
/// Indic_Syllabic_Category with HarfBuzz's overrides. The three shapers read the
/// same table: each knows the categories of its own script and treats the rest
/// as `X`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum IndicCategory {
    /// Anything else.
    X,
    /// A consonant.
    C,
    /// An independent vowel (Myanmar IV).
    V,
    /// A nukta or tone mark (Myanmar DB, dot below).
    N,
    /// A virama or invisible stacker (Khmer coeng).
    H,
    Zwnj,
    Zwj,
    /// A dependent vowel (matra) or killer.
    M,
    /// A syllable modifier with a position: bindu, visarga.
    SM,
    /// A Vedic accent or cantillation mark (VD in the specification).
    A,
    /// A placeholder, number or other base standing in for a consonant (Myanmar GB).
    Placeholder,
    /// U+25CC DOTTED CIRCLE.
    DottedCircle,
    /// A register shifter.
    RS,
    /// A post-base matra that may follow a bindu (U+0A40).
    MPst,
    /// A repha character.
    Repha,
    /// The consonant that forms reph or a pre-base form.
    Ra,
    /// A medial or final consonant sign.
    CM,
    /// An avagraha or other symbol that takes marks.
    Symbol,
    /// A consonant with stacker.
    CS,
    /// A syllable modifier without a position.
    SMPst,
    /// Khmer and Myanmar dependent vowels, by position.
    VAbv,
    VBlw,
    VPre,
    VPst,
    /// Khmer robat and the like.
    Robatic,
    /// Khmer signs that stay with the consonant or come after the vowel.
    Xgroup,
    Ygroup,
    /// Myanmar asat.
    As,
    /// Myanmar medial ha, medial ra, medial wa, medial ya and medial Mon la.
    MH,
    MR,
    MW,
    MY,
    ML,
    /// Pwo and other tones.
    PT,
    /// A variation selector.
    VS,
}

/// Where a character goes in an Indic syllable, in reordering order: sorting by
/// position puts a syllable in its visual order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum IndicPosition {
    Start,
    RaToBecomeReph,
    PreM,
    PreC,
    BaseC,
    AfterMain,
    AboveC,
    BeforeSub,
    BelowC,
    AfterSub,
    BeforePost,
    PostC,
    AfterPost,
    /// Syllable modifiers and Vedic signs, at the end.
    Smvd,
    End,
}

/// The Universal Shaping Engine category of a character, as the USE
/// specification abbreviates them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum UseCategory {
    /// Other.
    O,
    /// Base.
    B,
    /// Base number.
    N,
    /// Base other (placeholders).
    GB,
    /// Combining grapheme joiner, variation selectors and ZWJ.
    Cgj,
    /// Subjoined consonant.
    Sub,
    /// Halant.
    H,
    /// Halant number.
    HN,
    Zwnj,
    /// Word joiner and other invisible characters.
    WJ,
    /// Repha.
    R,
    /// Consonant with stacker.
    CS,
    /// Invisible stacker.
    IS,
    /// Sakot.
    Sk,
    /// Hieroglyph.
    G,
    /// Hieroglyph joiner.
    J,
    /// Hieroglyph segment begin and end.
    SB,
    SE,
    /// Halant or vowel modifier.
    Hvm,
    /// Hieroglyph modifier and mirror.
    HM,
    HR,
    /// Reordering killer.
    RK,
    /// Final consonants, by position.
    FAbv,
    FBlw,
    FPst,
    /// Medial consonants, by position.
    MAbv,
    MBlw,
    MPst,
    MPre,
    /// Consonant modifiers, by position.
    CMAbv,
    CMBlw,
    /// Dependent vowels, by position.
    VAbv,
    VBlw,
    VPst,
    VPre,
    /// Vowel modifiers, by position.
    VMAbv,
    VMBlw,
    VMPst,
    VMPre,
    /// Symbol modifiers, by position.
    SMAbv,
    SMBlw,
    /// Final consonant modifiers, by position.
    FMAbv,
    FMBlw,
    FMPst,
}

/// The Indic category and position of `c`.
pub(crate) fn indic_properties(c: u32) -> (IndicCategory, IndicPosition) {
    INDIC_PROPERTIES[usize::from(INDIC.get(c))]
}

/// The Universal Shaping Engine category of `c`.
pub(crate) fn use_category(c: u32) -> UseCategory {
    USE_CATEGORIES[usize::from(USE.get(c))]
}

/// The vowel sequences of `script` that look like another vowel; a dotted
/// circle goes before the last character of each.
pub(crate) fn vowel_constraints(script: Script) -> &'static [&'static [u32]] {
    VOWEL_CONSTRAINTS
        .binary_search_by_key(&script, |&(s, _)| s)
        .map_or(&[], |i| VOWEL_CONSTRAINTS[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_follow_harfbuzz_overrides() {
        use IndicCategory as C;
        use IndicPosition as P;
        assert_eq!(indic_properties(0x0915), (C::C, P::BaseC));
        assert_eq!(indic_properties(0x0930), (C::Ra, P::BaseC));
        assert_eq!(indic_properties(0x093F), (C::M, P::PreM));
        assert_eq!(indic_properties(0x094D), (C::H, P::BelowC));
        assert_eq!(indic_properties(0x0902), (C::SM, P::Smvd));
        assert_eq!(indic_properties(0x25CC), (C::DottedCircle, P::BaseC));
        assert_eq!(indic_properties(0x17C1), (C::VPre, P::PreC));
        assert_eq!(indic_properties(0x103C), (C::MR, P::End));
        assert_eq!(indic_properties(0x0041), (C::X, P::End));
        assert_eq!(use_category(0x0D9A), UseCategory::B);
        assert_eq!(use_category(0x0DCA), UseCategory::Hvm);
        assert_eq!(use_category(0x0DD9), UseCategory::VPre);
        assert_eq!(use_category(0x200D), UseCategory::Cgj);
        assert_eq!(use_category(0x0041), UseCategory::O);
    }

    #[test]
    fn vowel_constraints_are_found_by_script() {
        let deva = vowel_constraints(Script::from_code(*b"Deva"));
        assert!(deva.contains(&&[0x0930, 0x094D, 0x0907][..]));
        assert!(vowel_constraints(Script::from_code(*b"Latn")).is_empty());
        assert!(VOWEL_CONSTRAINTS.is_sorted_by_key(|(s, _)| *s));
    }
}
