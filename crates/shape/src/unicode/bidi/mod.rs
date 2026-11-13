//! The bidi properties of Unicode 14.0.0 and the Arabic presentation forms its
//! UnicodeData.txt decomposes, looked up for [`crate::bidi`].

#[rustfmt::skip] // Laid out by the generator.
mod tables;

use crate::bidi::{BidiClass, Bracket, JoiningType};
use crate::unicode::ArabicForm;

/// The Bidi_Class of `cp`, with the defaults of unassigned code points; L beyond
/// U+10FFFF.
#[inline]
pub(crate) fn bidi_class(cp: u32) -> BidiClass {
    BidiClass::ALL[usize::from(tables::BIDI_CLASS.get(cp))]
}

/// The paired bracket `cp` is, if any.
#[inline]
pub(crate) fn bracket(cp: u32) -> Option<Bracket> {
    let index = usize::from(tables::BRACKET.get(cp)).checked_sub(1)?;
    let value = tables::BRACKETS[index];
    let pair = char::from_u32(value & 0x7FFF_FFFF)?;
    Some(Bracket::new(pair, value & 0x8000_0000 != 0))
}

/// The Bidi_Mirroring_Glyph of `cp`, if it has one.
#[inline]
pub(crate) fn mirror(cp: u32) -> Option<u32> {
    let index = usize::from(tables::MIRROR.get(cp));
    index.checked_sub(1).map(|i| tables::MIRRORED[i])
}

/// The Joining_Type of `cp`.
#[inline]
pub(crate) fn joining_type(cp: u32) -> JoiningType {
    JoiningType::ALL[usize::from(tables::JOINING_TYPE.get(cp))]
}

/// The presentation form of `cp` in `form`, if UnicodeData.txt has one; the
/// generated table has a column per form, in the order of [`ArabicForm`].
pub(crate) fn presentation_form(cp: u32, form: ArabicForm) -> Option<u32> {
    let row =
        tables::PRESENTATION_FORMS.get(cp.checked_sub(tables::PRESENTATION_FIRST)? as usize)?;
    Some(u32::from(row[form as usize])).filter(|&f| f != 0)
}

/// The ligature of LAM with the ALEF `alef`: isolated, or final when the LAM
/// joins the character before it.
pub(crate) fn lam_alef(alef: u32, last: bool) -> Option<u32> {
    let at = tables::LAM_ALEF
        .binary_search_by_key(&alef, |&(a, _, _)| a)
        .ok()?;
    let (_, isolated, final_form) = tables::LAM_ALEF[at];
    Some(if last { final_form } else { isolated })
}

/// LAM, the first character of the ligatures [`lam_alef`] forms.
pub(crate) const LAM: u32 = 0x0644;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_and_defaults() {
        assert_eq!(bidi_class(0x41), BidiClass::L);
        assert_eq!(bidi_class(0x5D0), BidiClass::R);
        assert_eq!(bidi_class(0x627), BidiClass::Al);
        assert_eq!(bidi_class(0x2069), BidiClass::Pdi);
        // Unassigned: in an Arabic block, a Hebrew block, and a noncharacter.
        assert_eq!(bidi_class(0x07BF), BidiClass::Al);
        assert_eq!(bidi_class(0x05FF), BidiClass::R);
        assert_eq!(bidi_class(0xFDD0), BidiClass::Bn);
        assert_eq!(bidi_class(0x11_0000), BidiClass::L);
    }

    #[test]
    fn brackets_pair_through_decompositions() {
        let open = bracket(0x28).unwrap();
        assert!(open.opens());
        assert!(open.pairs_with(bracket(0x29).unwrap()));
        assert!(open.pairs_with(bracket(0xFF09).unwrap()));
        assert!(
            bracket(0x2329)
                .unwrap()
                .pairs_with(bracket(0x3009).unwrap())
        );
        assert_eq!(bracket(0x41), None);
    }

    #[test]
    fn forms_and_ligatures() {
        assert_eq!(mirror(0x28), Some(0x29));
        assert_eq!(mirror(0x41), None);
        assert_eq!(joining_type(0x628), JoiningType::DualJoining);
        assert_eq!(joining_type(0x627), JoiningType::RightJoining);
        assert_eq!(joining_type(0x64E), JoiningType::Transparent);
        assert_eq!(joining_type(0x200D), JoiningType::JoinCausing);
        assert_eq!(presentation_form(0x628, ArabicForm::Initial), Some(0xFE91));
        assert_eq!(presentation_form(0x627, ArabicForm::Initial), None);
        assert_eq!(lam_alef(0x627, false), Some(0xFEFB));
        assert_eq!(lam_alef(0x627, true), Some(0xFEFC));
    }
}
