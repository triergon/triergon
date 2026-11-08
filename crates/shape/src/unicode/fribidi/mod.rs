//! FriBidi 1.0.17's character properties, from Unicode 14.0.0: what
//! `fribidi_get_bidi_type`, `fribidi_get_bracket`, `fribidi_get_mirror_char`,
//! `fribidi_get_joining_type` and `fribidi_shape_arabic` look up.

#[rustfmt::skip] // Laid out by the generator.
mod tables;

use super::ArabicForm;

/// A Bidi_Class (UAX #9), as FriBidi assigns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BidiClass {
    /// Left-to-right.
    L,
    /// Right-to-left.
    R,
    /// Arabic letter.
    Al,
    /// European number.
    En,
    /// European separator.
    Es,
    /// European terminator.
    Et,
    /// Arabic number.
    An,
    /// Common separator.
    Cs,
    /// Nonspacing mark.
    Nsm,
    /// Boundary neutral.
    Bn,
    /// Paragraph separator.
    B,
    /// Segment separator.
    S,
    /// Whitespace.
    Ws,
    /// Other neutral.
    On,
    /// Left-to-right embedding.
    Lre,
    /// Left-to-right override.
    Lro,
    /// Right-to-left embedding.
    Rle,
    /// Right-to-left override.
    Rlo,
    /// Pop directional format.
    Pdf,
    /// Left-to-right isolate.
    Lri,
    /// Right-to-left isolate.
    Rli,
    /// First strong isolate.
    Fsi,
    /// Pop directional isolate.
    Pdi,
}

/// `fribidi_get_bidi_type`. Code points UnicodeData.txt does not list, including
/// those inside its ranges, have FriBidi's defaults: R or AL in the blocks
/// reserved for right-to-left scripts, BN for noncharacters and default
/// ignorables, L otherwise and beyond U+10FFFF.
#[inline]
pub(crate) fn bidi_class(cp: u32) -> BidiClass {
    tables::BIDI_CLASSES[usize::from(tables::BIDI_CLASS.get(cp))]
}

/// Whether a paired bracket opens or closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BracketKind {
    Open,
    Close,
}

/// A paired bracket (BidiBrackets.txt).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Bracket {
    /// The identity the two brackets of a pair share: the opening bracket, or the
    /// first character of its decomposition, canonical or compatibility (U+2329
    /// and U+232A pair as U+3008, like U+3008 and U+3009; U+FF08 and U+FF09 as
    /// U+0028, as FriBidi 1.0.17's tables have them).
    pub(crate) pair: u32,
    pub(crate) kind: BracketKind,
}

/// `fribidi_get_bracket`: the bracket `cp` is, if any (FriBidi's value is the
/// identity, with the open bit for an opening bracket, or 0).
#[inline]
pub(crate) fn bracket(cp: u32) -> Option<Bracket> {
    let (offset, kind) = tables::BRACKETS[usize::from(tables::BRACKET.get(cp))]?;
    Some(Bracket {
        pair: cp.wrapping_add_signed(offset),
        kind,
    })
}

/// `fribidi_get_mirror_char`: the Bidi_Mirroring_Glyph of `cp`, if it has one.
#[inline]
pub(crate) fn mirror(cp: u32) -> Option<u32> {
    let delta = tables::MIRROR_DELTAS[usize::from(tables::MIRROR.get(cp))];
    (delta != 0).then(|| cp.wrapping_add_signed(delta))
}

/// A joining type, as FriBidi assigns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum JoiningType {
    /// U: does not join.
    NonJoining,
    /// R: joins the preceding character (in logical order) only.
    RightJoining,
    /// D: joins both sides.
    DualJoining,
    /// C: causes joining on both sides without changing form.
    JoinCausing,
    /// T: marks and format characters, skipped by joining.
    Transparent,
    /// L: joins the following character only.
    LeftJoining,
    /// G: BN and the explicit directional formatting characters, skipped by joining.
    Ignored,
}

/// `fribidi_get_joining_type`: ArabicShaping.txt's type, else transparent for Mn,
/// Me and Cf, ignored for BN and the explicit formatting classes, and
/// non-joining otherwise.
#[inline]
pub(crate) fn joining_type(cp: u32) -> JoiningType {
    tables::JOINING_TYPES[usize::from(tables::JOINING_TYPE.get(cp))]
}

/// The presentation form `fribidi_shape_arabic` gives `cp` in `form`, if it has one
/// other than `cp` itself.
pub(crate) fn presentation_form(cp: u32, form: ArabicForm) -> Option<u32> {
    let row = cp.checked_sub(tables::PRESENTATION_FIRST)?;
    let forms = tables::PRESENTATION_FORMS.get(row as usize)?;
    Some(u32::from(forms[form as usize])).filter(|&f| f != 0)
}

/// The mandatory ligature of a LAM presentation form followed by an ALEF one, as
/// `fribidi_shape_arabic` forms it (with `FRIBIDI_FLAG_SHAPE_ARAB_LIGA`).
pub(crate) fn lam_alef_ligature(lam: u32, alef: u32) -> Option<u32> {
    let ligatures = &tables::LAM_ALEF_LIGATURES;
    ligatures
        .binary_search_by_key(&(lam, alef), |&(lam, alef, _)| (lam, alef))
        .ok()
        .map(|i| ligatures[i].2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidi_classes() {
        use BidiClass::*;
        for (cp, class) in [
            (0x0041, L),
            (0x05D0, R),
            (0x0627, Al),
            (0x0030, En),
            (0x0660, An),
            (0x0301, Nsm),
            (0x0020, Ws),
            (0x000A, B),
            (0x0009, S),
            (0x200D, Bn),
            (0x2067, Rli),
            (0x202E, Rlo),
            (0x0021, On),
            // Defaults of code points UnicodeData.txt 14.0.0 does not list.
            (0x05FF, R),
            (0x07BF, Al),
            (0xFDD0, Bn),
            (0x1FFFE, Bn),
            (0x10FFFF, Bn),
            (0x10FFF, R),
            (0xE0FFF, Bn),
            (0x0378, L),
            (0x4E01, L),
            (0x110000, L),
            (u32::MAX, L),
        ] {
            assert_eq!(bidi_class(cp), class, "U+{cp:04X}");
        }
    }

    #[test]
    fn brackets_and_mirrors() {
        use BracketKind::*;
        let bracket_at = |pair, kind| Some(Bracket { pair, kind });
        assert_eq!(bracket(0x0028), bracket_at(0x0028, Open));
        assert_eq!(bracket(0x0029), bracket_at(0x0028, Close));
        assert_eq!(bracket(0x005D), bracket_at(0x005B, Close));
        assert_eq!(bracket(0x2329), bracket_at(0x3008, Open));
        assert_eq!(bracket(0x232A), bracket_at(0x3008, Close));
        assert_eq!(bracket(0x3008), bracket_at(0x3008, Open));
        assert_eq!(bracket(0x3009), bracket_at(0x3008, Close));
        assert_eq!(
            bracket(0xFF08),
            bracket_at(0x0028, Open),
            "compatibility folding"
        );
        assert_eq!(bracket(0xFF09), bracket_at(0x0028, Close));
        assert_eq!(bracket(0x0041), None);
        assert_eq!(bracket(u32::MAX), None);
        assert_eq!(mirror(0x0028), Some(0x0029));
        assert_eq!(mirror(0x2329), Some(0x232A));
        assert_eq!(mirror(0x0041), None);
        assert_eq!(mirror(u32::MAX), None);
    }

    #[test]
    fn joining_types() {
        use JoiningType::*;
        for (cp, joining) in [
            (0x0627, RightJoining),
            (0x0644, DualJoining),
            (0x0640, JoinCausing),
            (0x200D, JoinCausing),
            (0x064B, Transparent),
            (0x200B, Transparent),
            (0x202B, Transparent),
            (0x0001, Ignored),
            (0x007F, Ignored),
            (0x00AD, Transparent),
            (0x0041, NonJoining),
            (0x110000, NonJoining),
        ] {
            assert_eq!(joining_type(cp), joining, "U+{cp:04X}");
        }
    }

    #[test]
    fn arabic_shaping() {
        use ArabicForm::*;
        assert_eq!(presentation_form(0x0644, Initial), Some(0xFEDF));
        assert_eq!(presentation_form(0x0644, Medial), Some(0xFEE0));
        assert_eq!(presentation_form(0x0627, Final), Some(0xFE8E));
        assert_eq!(presentation_form(0x0627, Medial), None);
        assert_eq!(presentation_form(0x0621, Isolated), Some(0xFE80));
        assert_eq!(presentation_form(0x0041, Isolated), None);
        assert_eq!(lam_alef_ligature(0xFEDF, 0xFE8E), Some(0xFEFB));
        assert_eq!(lam_alef_ligature(0xFEE0, 0xFE8E), Some(0xFEFC));
        assert_eq!(lam_alef_ligature(0xFEE0, 0xFE82), Some(0xFEF6));
        assert_eq!(lam_alef_ligature(0xFEDF, 0xFEDF), None);
    }

    #[test]
    fn every_code_point_is_looked_up() {
        for cp in (0..=0x11_0000).chain([u32::MAX]) {
            let _ = (bidi_class(cp), bracket(cp), mirror(cp), joining_type(cp));
            let _ = presentation_form(cp, ArabicForm::Isolated);
        }
    }
}
