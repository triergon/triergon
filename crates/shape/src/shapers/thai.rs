//! Thai and Lao: SARA AM decomposed into NIKHAHIT and SARA AA with the NIKHAHIT
//! moved before the above-base marks, and, for Thai fonts without Thai
//! substitutions, marks and bases replaced by the private-use glyphs legacy Thai
//! fonts position with.
//!
//! The rules are those of <https://linux.thai.net/~thep/th-otf/shaping.html>, as
//! HarfBuzz implements them.

use crate::buffer::{Buffer, UnicodeProps};
use crate::metrics::Scaled;
use crate::plan::Plan;
use crate::unicode::GeneralCategory;
use triergon_core::Result;

/// The Lao letters are the Thai ones moved up by this bit.
const LAO_BIT: u32 = 0x80;
const SARA_AM: u32 = 0x0E33;
const NIKHAHIT: u32 = 0x0E4D;

/// SARA AM, Thai U+0E33 or Lao U+0EB3.
fn is_sara_am(c: u32) -> bool {
    c & !LAO_BIT == SARA_AM
}

/// The marks a NIKHAHIT from SARA AM moves in front of, Thai or Lao.
fn is_above_base_mark(c: u32) -> bool {
    matches!(
        c & !LAO_BIT,
        0x0E31 | 0x0E34..=0x0E37 | 0x0E3B | 0x0E47..=0x0E4E
    )
}

/// Decomposes every SARA AM, then shapes with private-use glyphs when the font
/// has no Thai substitutions.
pub(super) fn preprocess_text(plan: &Plan, buffer: &mut Buffer, font: &Scaled<'_>) -> Result<()> {
    decompose_sara_am(buffer)?;
    let (tags, count) = plan.script.ot_tags();
    let has_thai_gsub = plan.chosen_script[0].is_some_and(|tag| tags[..count].contains(&tag));
    if &plan.script.to_bytes() == b"Thai" && !has_thai_gsub {
        pua_shape(buffer, font);
    }
    Ok(())
}

/// `<C, T, SARA AM>` becomes `<C, NIKHAHIT, T, SARA AA>`, all in one cluster:
/// the NIKHAHIT, which only a SARA AM brings, goes before the above-base marks,
/// as Uniscribe orders them.
fn decompose_sara_am(buffer: &mut Buffer) -> Result<()> {
    buffer.clear_output();
    while buffer.idx < buffer.len() {
        buffer.charge(1)?;
        let am = buffer.cur().glyph;
        if !is_sara_am(am) {
            buffer.next_glyph()?;
            continue;
        }
        // Both halves keep the SARA AM's properties; the NIKHAHIT becomes a mark
        // of class zero, so mark widths are zeroed for it.
        buffer.output_glyph(am - SARA_AM + NIKHAHIT)?;
        let nikhahit = buffer.out_len - 1;
        let info = buffer.out_info_mut(nikhahit);
        info.unicode.set(UnicodeProps::CONTINUATION, true);
        info.unicode.set_category(GeneralCategory::NonSpacingMark);
        buffer.scratch.continuations = true;
        buffer.replace_glyph(am - 1)?;
        let end = buffer.out_len;
        let mut start = nikhahit;
        while start > 0 && is_above_base_mark(buffer.out_info(start - 1).glyph) {
            start -= 1;
        }
        buffer.charge(nikhahit - start)?;
        if start < nikhahit {
            buffer.merge_out_clusters(start, end)?;
            buffer.out_infos_mut()[start..=nikhahit].rotate_right(1);
        }
        // The NIKHAHIT combines, so it joins the preceding cluster.
        if start > 0 {
            buffer.merge_out_clusters(start - 1, end)?;
        }
    }
    buffer.sync()
}

/// How a consonant carries marks: the state both machines start in after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Consonant {
    /// Any other consonant.
    Normal,
    /// With an ascender: above marks shift left.
    Ascending,
    /// With a descender a below mark replaces.
    RemovableDescender,
    /// With a descender below marks go under.
    Descending,
}

impl Consonant {
    fn of(c: u32) -> Option<Self> {
        Some(match c {
            0x0E1B | 0x0E1D | 0x0E1F => Self::Ascending,
            0x0E0D | 0x0E10 => Self::RemovableDescender,
            0x0E0E | 0x0E0F => Self::Descending,
            0x0E01..=0x0E2E => Self::Normal,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    /// An above vowel or sign.
    AboveVowel,
    BelowVowel,
    Tone,
}

impl Mark {
    fn of(c: u32) -> Option<Self> {
        Some(match c {
            0x0E31 | 0x0E34..=0x0E37 | 0x0E47 | 0x0E4D..=0x0E4E => Self::AboveVowel,
            0x0E38..=0x0E3A => Self::BelowVowel,
            0x0E48..=0x0E4C => Self::Tone,
            _ => return None,
        })
    }
}

/// What the cluster above the base already holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Above {
    /// Nothing, over a normal consonant.
    Empty,
    /// Nothing, over an ascender.
    Ascender,
    /// A mark, shifted left over an ascender.
    ShiftedOverAscender,
    /// Nothing more moves.
    Full,
}

/// What is below the base, in the way of a below mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Descender {
    None,
    /// One a private-use form of the base leaves off.
    Removable,
    /// One a below mark has to shift down under, or a mark already there.
    Strict,
}

/// The private-use form a glyph takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    None,
    ShiftDown,
    ShiftLeft,
    ShiftDownLeft,
    /// The base without its descender.
    RemoveDescender,
}

impl Above {
    fn start(consonant: Option<Consonant>) -> Self {
        match consonant {
            Some(Consonant::Ascending) => Self::Ascender,
            Some(_) => Self::Empty,
            None => Self::Full,
        }
    }

    fn next(self, mark: Mark) -> (Action, Self) {
        match (self, mark) {
            (Self::Empty, Mark::Tone) => (Action::ShiftDown, Self::Full),
            (Self::Ascender, Mark::AboveVowel) => (Action::ShiftLeft, Self::ShiftedOverAscender),
            (Self::Ascender, Mark::Tone) => (Action::ShiftDownLeft, Self::ShiftedOverAscender),
            (Self::ShiftedOverAscender, Mark::Tone) => (Action::ShiftLeft, Self::Full),
            (state, Mark::BelowVowel) => (Action::None, state),
            _ => (Action::None, Self::Full),
        }
    }
}

impl Descender {
    fn start(consonant: Option<Consonant>) -> Self {
        match consonant {
            Some(Consonant::Normal | Consonant::Ascending) => Self::None,
            Some(Consonant::RemovableDescender) => Self::Removable,
            Some(Consonant::Descending) | None => Self::Strict,
        }
    }

    fn next(self, mark: Mark) -> (Action, Self) {
        match (self, mark) {
            (Self::None, Mark::BelowVowel) => (Action::None, Self::Strict),
            (Self::Removable, Mark::BelowVowel) => (Action::RemoveDescender, Self::Strict),
            (Self::Strict, Mark::BelowVowel) => (Action::ShiftDown, Self::Strict),
            (state, _) => (Action::None, state),
        }
    }
}

impl Action {
    /// Per character, its Windows and its Macintosh private-use form.
    fn forms(self) -> &'static [(u32, [u32; 2])] {
        match self {
            Self::None => &[],
            Self::ShiftDown => &[
                (0x0E48, [0xF70A, 0xF88B]),
                (0x0E49, [0xF70B, 0xF88E]),
                (0x0E4A, [0xF70C, 0xF891]),
                (0x0E4B, [0xF70D, 0xF894]),
                (0x0E4C, [0xF70E, 0xF897]),
                (0x0E38, [0xF718, 0xF89B]),
                (0x0E39, [0xF719, 0xF89C]),
                (0x0E3A, [0xF71A, 0xF89D]),
            ],
            Self::ShiftDownLeft => &[
                (0x0E48, [0xF705, 0xF88C]),
                (0x0E49, [0xF706, 0xF88F]),
                (0x0E4A, [0xF707, 0xF892]),
                (0x0E4B, [0xF708, 0xF895]),
                (0x0E4C, [0xF709, 0xF898]),
            ],
            Self::ShiftLeft => &[
                (0x0E48, [0xF713, 0xF88A]),
                (0x0E49, [0xF714, 0xF88D]),
                (0x0E4A, [0xF715, 0xF890]),
                (0x0E4B, [0xF716, 0xF893]),
                (0x0E4C, [0xF717, 0xF896]),
                (0x0E31, [0xF710, 0xF884]),
                (0x0E34, [0xF701, 0xF885]),
                (0x0E35, [0xF702, 0xF886]),
                (0x0E36, [0xF703, 0xF887]),
                (0x0E37, [0xF704, 0xF888]),
                (0x0E47, [0xF712, 0xF889]),
                (0x0E4D, [0xF711, 0xF899]),
            ],
            Self::RemoveDescender => &[(0x0E0D, [0xF70F, 0xF89A]), (0x0E10, [0xF700, 0xF89E])],
        }
    }

    /// The form of `c` the font has, Windows first; `c` itself otherwise.
    fn apply(self, c: u32, font: &Scaled<'_>) -> u32 {
        self.forms()
            .iter()
            .find(|&&(u, _)| u == c)
            .and_then(|(_, forms)| forms.iter().copied().find(|&pua| font.glyph(pua).is_some()))
            .unwrap_or(c)
    }
}

/// Runs the above and below state machines over each base and its marks,
/// replacing characters by the private-use forms the font has.
fn pua_shape(buffer: &mut Buffer, font: &Scaled<'_>) {
    let mut above = Above::start(None);
    let mut below = Descender::start(None);
    let mut base = 0;
    for i in 0..buffer.len() {
        let infos = buffer.infos_mut();
        let c = infos[i].glyph;
        let Some(mark) = Mark::of(c) else {
            let consonant = Consonant::of(c);
            above = Above::start(consonant);
            below = Descender::start(consonant);
            base = i;
            continue;
        };
        let (above_action, next_above) = above.next(mark);
        let (below_action, next_below) = below.next(mark);
        (above, below) = (next_above, next_below);
        // At most one of the two machines acts.
        let action = if above_action == Action::None {
            below_action
        } else {
            above_action
        };
        let target = if action == Action::RemoveDescender {
            base
        } else {
            i
        };
        infos[target].glyph = action.apply(infos[target].glyph, font);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Above, Action, Consonant, Descender, Mark, decompose_sara_am, is_above_base_mark,
        is_sara_am, pua_shape,
    };
    use crate::shapers::testing::{TestFont, contents, scaled, text_buffer};
    use crate::unicode::GeneralCategory;
    use triergon_core::Budget;

    #[test]
    fn nikhahit_of_sara_am_moves_before_above_marks() {
        let budget = Budget::new(1 << 16).expect("budget");
        for (text, decomposed) in [
            ([0x0E14, 0x0E4B, 0x0E33], [0x0E14, 0x0E4D, 0x0E4B, 0x0E32]),
            ([0x0E81, 0x0EC8, 0x0EB3], [0x0E81, 0x0ECD, 0x0EC8, 0x0EB2]),
        ] {
            let mut buffer = text_buffer(&budget, &text);
            decompose_sara_am(&mut buffer).expect("decomposes");
            assert_eq!(contents(&buffer), decomposed.map(|c| (c, 0)));
            let nikhahit = buffer
                .infos()
                .iter()
                .find(|i| i.glyph & !0x80 == 0x0E4D)
                .expect("nikhahit");
            assert_eq!(nikhahit.category(), GeneralCategory::NonSpacingMark);
            assert!(nikhahit.is_continuation());
        }
    }

    #[test]
    fn sara_am_joins_only_the_preceding_cluster() {
        let budget = Budget::new(1 << 16).expect("budget");
        let mut buffer = text_buffer(&budget, &[0x0020, 0x0E33, 0x0E33]);
        decompose_sara_am(&mut buffer).expect("decomposes");
        assert_eq!(
            contents(&buffer),
            [
                (0x20, 0),
                (0x0E4D, 0),
                (0x0E32, 0),
                (0x0E4D, 0),
                (0x0E32, 0)
            ]
        );
        // A below mark stays before the NIKHAHIT, which merges with it alone.
        let mut buffer = text_buffer(&budget, &[0x0E01, 0x0E38, 0x0E33]);
        decompose_sara_am(&mut buffer).expect("decomposes");
        assert_eq!(
            contents(&buffer),
            [(0x0E01, 0), (0x0E38, 1), (0x0E4D, 1), (0x0E32, 1)]
        );
    }

    #[test]
    fn private_use_forms_prefer_windows_then_macintosh() {
        let budget = Budget::new(1 << 16).expect("budget");
        let font = TestFont(&[(0xF705, 0), (0xF88C, 0), (0xF89E, 500), (0xF718, 0)]);
        let font = scaled(&font);
        let mut buffer = text_buffer(&budget, &[0x0E1B, 0x0E48, 0x0E10, 0x0E38, 0x0E0E, 0x0E38]);
        pua_shape(&mut buffer, &font);
        let glyphs: Vec<u32> = contents(&buffer).iter().map(|&(c, _)| c).collect();
        assert_eq!(glyphs, [0x0E1B, 0xF705, 0xF89E, 0x0E38, 0x0E0E, 0xF718]);
    }

    #[test]
    fn sara_am_and_above_marks_cover_thai_and_lao() {
        assert!(is_sara_am(0x0E33) && is_sara_am(0x0EB3));
        assert!(!is_sara_am(0x0E32) && !is_sara_am(0x0F33));
        for c in [
            0x0E31, 0x0E34, 0x0E37, 0x0E3B, 0x0E47, 0x0E4E, 0x0EB1, 0x0EBB, 0x0EC8,
        ] {
            assert!(is_above_base_mark(c), "{c:04X}");
        }
        for c in [0x0E32, 0x0E38, 0x0E3A, 0x0E4F, 0x0EB2] {
            assert!(!is_above_base_mark(c), "{c:04X}");
        }
    }

    #[test]
    fn tone_over_ascender_shifts_down_left_then_left() {
        let mut above = Above::start(Consonant::of(0x0E1B));
        let (action, next) = above.next(Mark::Tone);
        assert_eq!(action, Action::ShiftDownLeft);
        above = next;
        assert_eq!(above.next(Mark::Tone), (Action::ShiftLeft, Above::Full));
    }

    #[test]
    fn below_vowel_removes_a_removable_descender_once() {
        let below = Descender::start(Consonant::of(0x0E10));
        let (action, below) = below.next(Mark::BelowVowel);
        assert_eq!(action, Action::RemoveDescender);
        assert_eq!(below.next(Mark::BelowVowel).0, Action::ShiftDown);
        assert_eq!(
            Descender::start(Consonant::of(0x0E01))
                .next(Mark::BelowVowel)
                .0,
            Action::None
        );
    }
}
