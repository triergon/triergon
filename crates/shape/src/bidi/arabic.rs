//! Cursive joining and Arabic presentation forms (The Unicode Standard,
//! chapter 9.2), as libass's simple shaping level applies them.
//!
//! Joining follows the logical order. A character joins the last joining
//! character before it, passing over transparent characters (marks and format
//! characters) at its level and ignored ones (BN controls), when both are at
//! one level and each joins on the side that faces the other. The sides are visual: at an
//! odd level the character before joins on its left and the character after it
//! on its right, as their joining types (ArabicShaping.txt) give them in
//! right-to-left text; at an even level the sides swap, so that a right-joining
//! character joins the character after it.
//!
//! Then every right-, left- and dual-joining character takes the presentation
//! form of its joined sides, a LAM and the ALEF right after it that it joins
//! form their mandatory ligature at an odd level, and characters at odd levels
//! are mirrored.
//!
//! FriBidi's choices, reproduced here: joining by visual sides at even levels,
//! where chapter 9.2 joins in logical order; a mark at another level than the
//! character before it ends the joining; and a character of class BN or one
//! that rule X9 removes, such as ZERO WIDTH JOINER, joins at every level,
//! counting as right-to-left, so that it joins the characters on both sides of
//! it whatever their levels.

use super::class::{BidiClass, JoiningType, Level};
use super::mirror;
use crate::unicode::bidi::{self as data, Form};
use triergon_core::{Error, Result};

/// What replaces a LAM that forms a ligature with the ALEF after it: ZERO WIDTH
/// NO-BREAK SPACE, so that the text keeps its length.
const LIGATED_LAM: char = '\u{FEFF}';

/// The last joining character seen, which the next one may join.
#[derive(Clone, Copy, Debug)]
struct Joiner {
    at: usize,
    joining: JoiningType,
    /// Its level, `None` for a character that joins at every level.
    level: Option<Level>,
    /// Whether it joins the character before it.
    joined_before: bool,
}

/// Shapes `text` as libass's simple shaping level does, given the classes and
/// the resolved levels (before line reordering) of its characters: cursive
/// joining, Arabic presentation forms, the mandatory LAM-ALEF ligatures and
/// mirroring. The text keeps its length.
///
/// # Errors
///
/// `Error::Invalid` if the three slices differ in length.
pub fn shape_simple(text: &mut [char], classes: &[BidiClass], levels: &[Level]) -> Result<()> {
    if text.len() != classes.len() || text.len() != levels.len() {
        return Err(Error::Invalid("bidi shaping lengths differ"));
    }
    let mut last: Option<Joiner> = None;
    for at in 0..text.len() {
        let joining = JoiningType::of(text[at]);
        let level = (!classes[at].is_removed_by_x9()).then_some(levels[at]);
        if joining == JoiningType::Ignored {
            continue;
        }
        if joining == JoiningType::Transparent {
            // A mark at another level ends the joining.
            if let Some(before) =
                last.filter(|b| level.is_some() && b.level.is_some_and(|l| Some(l) != level))
            {
                shape(text, levels, before, false);
                last = None;
            }
            continue;
        }
        let joins = last.is_some_and(|before| {
            // Each character's side that faces the other, by its own level.
            let left_to_right = |level: Option<Level>| level.is_some_and(|l| !l.is_rtl());
            let before_side = if left_to_right(before.level) {
                before.joining.joins_preceding()
            } else {
                before.joining.joins_following()
            };
            let side = if left_to_right(level) {
                joining.joins_following()
            } else {
                joining.joins_preceding()
            };
            before_side
                && side
                && (before.level == level || before.level.is_none() || level.is_none())
        });
        if let Some(before) = last {
            let adjacent = before.at + 1 == at;
            if joins && adjacent && levels[at].is_rtl() && u32::from(text[before.at]) == data::LAM {
                let ligature = data::lam_alef(u32::from(text[at]), before.joined_before)
                    .and_then(char::from_u32);
                if let Some(ligature) = ligature {
                    text[before.at] = LIGATED_LAM;
                    text[at] = ligature;
                    last = None;
                    continue;
                }
            }
            shape(text, levels, before, joins);
        }
        last = Some(Joiner {
            at,
            joining,
            level,
            joined_before: joins,
        });
    }
    if let Some(before) = last {
        shape(text, levels, before, false);
    }
    for (c, level) in text.iter_mut().zip(levels) {
        if let Some(mirrored) = mirror(*c).filter(|_| level.is_rtl()) {
            *c = mirrored;
        }
    }
    Ok(())
}

/// Gives the character of `joiner` its presentation form, now that whether it
/// joins the next character (`joins_after`) is known.
fn shape(text: &mut [char], levels: &[Level], joiner: Joiner, joins_after: bool) {
    if !matches!(
        joiner.joining,
        JoiningType::RightJoining | JoiningType::LeftJoining | JoiningType::DualJoining
    ) {
        return;
    }
    let rtl = levels[joiner.at].is_rtl();
    let (right, left) = if rtl {
        (joiner.joined_before, joins_after)
    } else {
        (joins_after, joiner.joined_before)
    };
    let form = match (right, left) {
        (true, true) => Form::Medial,
        (true, false) => Form::Final,
        (false, true) => Form::Initial,
        (false, false) => Form::Isolated,
    };
    if let Some(shaped) =
        data::presentation_form(u32::from(text[joiner.at]), form).and_then(char::from_u32)
    {
        text[joiner.at] = shaped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_and_mirrors() {
        let mut text = ['\u{628}', '\u{64E}', '\u{628}', ' ', '('];
        let classes = text.map(BidiClass::of);
        let levels = [1, 1, 1, 1, 1].map(Level);
        shape_simple(&mut text, &classes, &levels).unwrap();
        assert_eq!(text, ['\u{FE91}', '\u{64E}', '\u{FE90}', ' ', ')']);
    }
}
