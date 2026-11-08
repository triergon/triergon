//! Cursive joining and Arabic presentation forms, as libass's simple shaping
//! gets them from FriBidi: `fribidi_join_arabic`, then `fribidi_shape` with the
//! default and Arabic flags.
//!
//! Joining follows the logical order. A character joins the last joining
//! character before it, passing over transparent characters (marks) and
//! ignored ones (BN and the embedding characters without a joining type), when
//! both sides allow it and their levels agree; the embedding characters and BN
//! agree with every level and count as right-to-left. Which side of a character
//! is joined is visual: at an odd level the preceding character joins on the
//! right. Marks between two joined characters join on both of those sides.
//!
//! Then every shaping character takes the presentation form of its joined
//! sides, a LAM and ALEF pair at one odd level forms the mandatory ligature
//! (the LAM becoming U+FEFF), and characters at odd levels are mirrored.

use super::class::{BidiClass, JoiningType, Level};
use super::mirror;
use crate::unicode::ArabicForm;
use crate::unicode::fribidi;
use triergon_core::{Error, Result};

/// What replaces a LAM that forms a ligature with the ALEF after it.
const LIGATURE_FILL: char = '\u{FEFF}';

/// The visual side of a character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Right,
    Left,
}

/// The sides on which a character joins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sides {
    right: bool,
    left: bool,
}

impl Sides {
    const fn has(self, side: Side) -> bool {
        match side {
            Side::Right => self.right,
            Side::Left => self.left,
        }
    }

    fn set(&mut self, side: Side, joins: bool) {
        match side {
            Side::Right => self.right = joins,
            Side::Left => self.left = joins,
        }
    }

    const fn form(self) -> ArabicForm {
        match (self.right, self.left) {
            (false, false) => ArabicForm::Isolated,
            (true, false) => ArabicForm::Final,
            (false, true) => ArabicForm::Initial,
            (true, true) => ArabicForm::Medial,
        }
    }
}

/// The level joining compares, `None` for the characters that match any.
type JoinLevel = Option<u8>;

/// The side of the preceding character at `level`.
fn preceding(level: JoinLevel) -> Side {
    match level {
        Some(level) if level % 2 == 0 => Side::Left,
        _ => Side::Right,
    }
}

fn following(level: JoinLevel) -> Side {
    match preceding(level) {
        Side::Right => Side::Left,
        Side::Left => Side::Right,
    }
}

/// A character's joining behaviour, from its joining type.
#[derive(Clone, Copy, Debug)]
struct Joining {
    /// The sides it may join.
    sides: Sides,
    /// Whether it takes presentation forms.
    shapes: bool,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Takes part in joining.
    Base,
    /// Passed over, but takes a form.
    Transparent,
    /// Passed over entirely.
    Ignored,
}

impl Joining {
    fn of(c: char) -> Self {
        let (right, left, shapes, kind) = match JoiningType::of(c) {
            JoiningType::NonJoining => (false, false, false, Kind::Base),
            JoiningType::RightJoining => (true, false, true, Kind::Base),
            JoiningType::DualJoining => (true, true, true, Kind::Base),
            JoiningType::JoinCausing => (true, true, false, Kind::Base),
            JoiningType::LeftJoining => (false, true, true, Kind::Base),
            JoiningType::Transparent => (false, false, true, Kind::Transparent),
            JoiningType::Ignored => (false, false, false, Kind::Ignored),
        };
        Self {
            sides: Sides { right, left },
            shapes,
            kind,
        }
    }
}

/// The last joining character, waiting for the next one.
#[derive(Clone, Copy, Debug)]
struct Anchor {
    at: usize,
    level: JoinLevel,
    sides: Sides,
    shapes: bool,
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
    if classes.len() != text.len() || levels.len() != text.len() {
        return Err(Error::Invalid("bidi shaping lengths differ"));
    }
    join(text, classes, levels);
    for at in 1..text.len() {
        let level = levels[at - 1];
        if level.is_rtl()
            && level == levels[at]
            && let Some(ligature) =
                fribidi::lam_alef_ligature(u32::from(text[at - 1]), u32::from(text[at]))
                    .and_then(char::from_u32)
        {
            text[at - 1] = LIGATURE_FILL;
            text[at] = ligature;
        }
    }
    for (c, level) in text.iter_mut().zip(levels) {
        if level.is_rtl() {
            *c = mirror(*c).unwrap_or(*c);
        }
    }
    Ok(())
}

/// Joins and gives every shaping character its presentation form, in one pass:
/// a character's form is known once the next joining character is seen.
fn join(text: &mut [char], classes: &[BidiClass], levels: &[Level]) {
    let mut anchor: Option<Anchor> = None;
    // Whether the anchor joins forward; the marks after it are undecided.
    let mut joins = false;
    let mut marks_from = 0;
    for at in 0..text.len() {
        let joining = Joining::of(text[at]);
        if joining.kind == Kind::Ignored {
            continue;
        }
        let level = (!classes[at].is_removed()).then_some(levels[at].0);
        let mut disjoins = false;
        if joins
            && let Some(a) = anchor
            && a.level.is_some_and(|l| level.is_some_and(|m| l != m))
        {
            disjoins = true;
            joins = false;
        }
        let mut sides = joining.sides;
        // The sides of the marks between the anchor and this character.
        let mut marks = Sides::default();
        if joining.kind == Kind::Base {
            let side = preceding(level);
            if !joins {
                if joining.shapes {
                    sides.set(side, false);
                }
            } else if !sides.has(side) {
                disjoins = true;
            } else if let Some(a) = anchor {
                marks.set(side, true);
                marks.set(following(a.level), true);
            }
        }
        if disjoins
            && let Some(a) = anchor.as_mut()
            && a.shapes
        {
            a.sides.set(following(a.level), false);
        }
        if joining.kind == Kind::Base {
            if let Some(a) = anchor {
                shape(text, a.at, a.shapes, a.sides);
            }
            shape_marks(text, marks_from..at, marks);
            anchor = Some(Anchor {
                at,
                level,
                sides,
                shapes: joining.shapes,
            });
            joins = sides.has(following(level));
            marks_from = at + 1;
        }
    }
    if let Some(mut a) = anchor {
        if joins && a.shapes {
            a.sides.set(following(a.level), false);
        }
        shape(text, a.at, a.shapes, a.sides);
    }
    shape_marks(text, marks_from..text.len(), Sides::default());
}

fn shape(text: &mut [char], at: usize, shapes: bool, sides: Sides) {
    if shapes {
        let c = text[at];
        text[at] = fribidi::presentation_form(u32::from(c), sides.form())
            .and_then(char::from_u32)
            .unwrap_or(c);
    }
}

/// Gives the transparent characters in `range` the form of `sides`.
fn shape_marks(text: &mut [char], range: std::ops::Range<usize>, sides: Sides) {
    for at in range {
        if Joining::of(text[at]).kind == Kind::Transparent {
            shape(text, at, true, sides);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shaped(text: &str, level: u8) -> String {
        let mut text: Vec<char> = text.chars().collect();
        let classes: Vec<BidiClass> = text.iter().map(|&c| BidiClass::of(c)).collect();
        let levels = vec![Level(level); text.len()];
        shape_simple(&mut text, &classes, &levels).expect("shape");
        text.into_iter().collect()
    }

    #[test]
    fn joining_forms() {
        // BEH BEH BEH: initial, medial, final.
        assert_eq!(
            shaped("\u{628}\u{628}\u{628}", 1),
            "\u{FE91}\u{FE92}\u{FE90}"
        );
        // ALEF does not join forward: BEH ALEF BEH.
        assert_eq!(
            shaped("\u{628}\u{627}\u{628}", 1),
            "\u{FE91}\u{FE8E}\u{FE8F}"
        );
        // Letters join across a mark, which FriBidi gives no forms.
        assert_eq!(
            shaped("\u{628}\u{64E}\u{628}", 1),
            "\u{FE91}\u{64E}\u{FE90}"
        );
        // Even levels join on the other sides.
        assert_eq!(shaped("\u{628}\u{628}", 0), "\u{FE90}\u{FE91}");
    }

    #[test]
    fn lam_alef_and_mirroring() {
        assert_eq!(shaped("\u{644}\u{627}", 1), "\u{FEFF}\u{FEFB}");
        // At an even level LAM and ALEF do not join, nor form the ligature.
        assert_eq!(shaped("\u{644}\u{627}", 0), "\u{FEDD}\u{FE8D}");
        assert_eq!(shaped("(a)", 1), ")a(");
        assert_eq!(shaped("(a)", 0), "(a)");
    }

    #[test]
    fn lengths_must_agree() {
        let mut text = ['a'];
        assert!(shape_simple(&mut text, &[], &[Level(0)]).is_err());
    }
}
