//! Arabic and Syriac: joining forms, `stch` stretching and the order of modifier
//! combining marks.

use super::Pause;
use crate::buffer::{Buffer, Info};
use crate::face::Face;
use crate::metrics::Scaled;
use crate::plan::{Builder, FeatureFlags, Plan};
use crate::unicode::{ArabicJoining, arabic_joining};
use triergon_core::{Error, Result};

/// The joining form features, in the order of [`Action`].
const FEATURES: [&[u8; 4]; 7] = [
    b"isol", b"fina", b"fin2", b"fin3", b"medi", b"med2", b"init",
];

/// What a glyph's joining asks for: one of [`FEATURES`], or a `stch` tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Action {
    Isol,
    Fina,
    Fin2,
    Fin3,
    Medi,
    Med2,
    Init,
    None,
    StchFixed,
    StchRepeating,
}

impl Action {
    const ALL: [Self; 10] = [
        Self::Isol,
        Self::Fina,
        Self::Fin2,
        Self::Fin3,
        Self::Medi,
        Self::Med2,
        Self::Init,
        Self::None,
        Self::StchFixed,
        Self::StchRepeating,
    ];

    fn of(info: &Info) -> Self {
        Self::ALL
            .get(usize::from(info.shaper[0]))
            .copied()
            .unwrap_or(Self::None)
    }

    fn set(self, info: &mut Info) {
        info.shaper[0] = self as u8;
    }

    fn is_stch(self) -> bool {
        matches!(self, Self::StchFixed | Self::StchRepeating)
    }
}

fn is_syriac(tag: &[u8; 4]) -> bool {
    matches!(tag[3], b'2' | b'3')
}

/// The steps this shaper takes between lookup stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Records the glyphs `stch` substituted, for stretching.
    RecordStretching,
    /// Applies the fallback shaping from presentation forms.
    Fallback,
}

pub(super) fn collect_features(b: &mut Builder<'_>, script_is_arabic: bool) {
    b.enable_feature(b"stch");
    b.add_gsub_pause(Some(Pause::Arabic(Step::RecordStretching)));
    let manual_zwj = FeatureFlags {
        manual_zwj: true,
        ..FeatureFlags::GLOBAL
    };
    b.add_feature(b"ccmp", manual_zwj, 1);
    b.add_feature(b"locl", manual_zwj, 1);
    b.add_gsub_pause(None);
    for tag in FEATURES {
        let flags = FeatureFlags {
            manual_zwj: true,
            has_fallback: script_is_arabic && !is_syriac(tag),
            ..FeatureFlags::NONE
        };
        b.add_feature(tag, flags, 1);
        b.add_gsub_pause(None);
    }
    b.add_gsub_pause(None);
    b.add_feature(
        b"rlig",
        FeatureFlags {
            has_fallback: true,
            ..manual_zwj
        },
        1,
    );
    if script_is_arabic {
        b.add_gsub_pause(Some(Pause::Arabic(Step::Fallback)));
    }
    b.add_feature(b"calt", manual_zwj, 1);
    if !b.has_feature(b"rclt") {
        b.add_gsub_pause(None);
    }
    b.add_feature(b"liga", manual_zwj, 1);
    b.add_feature(b"clig", manual_zwj, 1);
    b.add_feature(b"mset", manual_zwj, 1);
}

/// The joining state machine: per state and joining class (U, L, R, D, Alaph,
/// Dalath-Rish), the action for the previous glyph, the action for this one and
/// the next state.
const STATES: [[(Action, Action, u8); 6]; 7] = {
    use Action::{Fin2, Fin3, Fina, Init, Isol, Med2, Medi, None as N};
    [
        [
            (N, N, 0),
            (N, Isol, 2),
            (N, Isol, 1),
            (N, Isol, 2),
            (N, Isol, 1),
            (N, Isol, 6),
        ],
        [
            (N, N, 0),
            (N, Isol, 2),
            (N, Isol, 1),
            (N, Isol, 2),
            (N, Fin2, 5),
            (N, Isol, 6),
        ],
        [
            (N, N, 0),
            (N, Isol, 2),
            (Init, Fina, 1),
            (Init, Fina, 3),
            (Init, Fina, 4),
            (Init, Fina, 6),
        ],
        [
            (N, N, 0),
            (N, Isol, 2),
            (Medi, Fina, 1),
            (Medi, Fina, 3),
            (Medi, Fina, 4),
            (Medi, Fina, 6),
        ],
        [
            (N, N, 0),
            (N, Isol, 2),
            (Med2, Isol, 1),
            (Med2, Isol, 2),
            (Med2, Fin2, 5),
            (Med2, Isol, 6),
        ],
        [
            (N, N, 0),
            (N, Isol, 2),
            (Isol, Isol, 1),
            (Isol, Isol, 2),
            (Isol, Fin2, 5),
            (Isol, Isol, 6),
        ],
        [
            (N, N, 0),
            (N, Isol, 2),
            (N, Isol, 1),
            (N, Isol, 2),
            (N, Fin3, 5),
            (N, Isol, 6),
        ],
    ]
};

/// The state machine column of a joining type; `None` for transparent ones.
fn column(joining: ArabicJoining) -> Option<usize> {
    Some(match joining {
        ArabicJoining::NonJoining => 0,
        ArabicJoining::LeftJoining => 1,
        ArabicJoining::RightJoining => 2,
        ArabicJoining::DualJoining => 3,
        ArabicJoining::Alaph => 4,
        ArabicJoining::DalathRish => 5,
        ArabicJoining::Transparent => return None,
    })
}

/// Gives every glyph its joining action, starting and ending with the context
/// characters, and the mask of the form feature that action applies.
pub(super) fn setup_masks(plan: &Plan, buffer: &mut Buffer) {
    let context_column = |c: u32| column(arabic_joining(c));
    let mut state = 0;
    if let Some(col) = buffer.context[0]
        .chars()
        .iter()
        .find_map(|&c| context_column(c))
    {
        state = usize::from(STATES[state][col].2);
    }
    let mut prev: Option<usize> = None;
    for i in 0..buffer.len() {
        let info = buffer.infos()[i];
        let Some(col) = column(arabic_joining(info.glyph)) else {
            Action::None.set(&mut buffer.infos_mut()[i]);
            continue;
        };
        let (prev_action, action, next) = STATES[state][col];
        if let (Some(p), true) = (prev, prev_action != Action::None) {
            prev_action.set(&mut buffer.infos_mut()[p]);
        }
        action.set(&mut buffer.infos_mut()[i]);
        prev = Some(i);
        state = usize::from(next);
    }
    if let Some(col) = buffer.context[1]
        .chars()
        .iter()
        .find_map(|&c| context_column(c))
    {
        let (prev_action, _, _) = STATES[state][col];
        if let (Some(p), true) = (prev, prev_action != Action::None) {
            prev_action.set(&mut buffer.infos_mut()[p]);
        }
    }
    let masks = FEATURES.map(|tag| plan.one_mask(tag));
    for info in buffer.infos_mut() {
        if let Some(&mask) = masks.get(usize::from(info.shaper[0])) {
            info.mask |= mask;
        }
    }
}

/// Mongolian free variation selectors take the joining form of the character
/// before them, after [`setup_masks`].
pub(super) fn copy_to_mongolian_variation_selectors(plan: &Plan, buffer: &mut Buffer) {
    let masks = FEATURES.map(|tag| plan.one_mask(tag));
    let infos = buffer.infos_mut();
    for i in 1..infos.len() {
        if matches!(infos[i].glyph, 0x180B..=0x180D | 0x180F) {
            let action = infos[i - 1].shaper[0];
            infos[i].shaper[0] = action;
            infos[i].mask |= masks.get(usize::from(action)).copied().unwrap_or(0);
        }
    }
}

/// Whether the plan must synthesise the joining forms: Arabic in a font whose
/// language system lacks all of the non-Syriac form features.
fn needs_fallback(plan: &Plan) -> bool {
    &plan.script.to_bytes() == b"Arab"
        && FEATURES
            .iter()
            .all(|tag| is_syriac(tag) || plan.needs_fallback(tag))
}

pub(super) fn pause(
    step: Step,
    plan: &Plan,
    face: &Face<'_>,
    buffer: &mut Buffer,
    font: &Scaled<'_>,
) -> Result<bool> {
    match step {
        Step::RecordStretching => {
            if plan.one_mask(b"stch") != 0 {
                let mut stretched = false;
                for info in buffer.infos_mut() {
                    if info.multiplied() {
                        let action = if info.lig.component() % 2 == 1 {
                            Action::StchRepeating
                        } else {
                            Action::StchFixed
                        };
                        action.set(info);
                        stretched = true;
                    }
                }
                buffer.scratch.arabic_stch |= stretched;
            }
            Ok(false)
        }
        Step::Fallback if needs_fallback(plan) => {
            super::arabic_fallback::shape(plan, face, buffer, font)?;
            Ok(true)
        }
        Step::Fallback => Ok(false),
    }
}

/// Repeats `stch` tiles to fill the width of the rest of their word.
pub(super) fn postprocess_glyphs(
    _plan: &Plan,
    buffer: &mut Buffer,
    _font: &Scaled<'_>,
) -> Result<()> {
    if buffer.scratch.arabic_stch && buffer.infos().iter().any(|i| Action::of(i).is_stch()) {
        return Err(Error::Unsupported("Arabic stch stretching"));
    }
    Ok(())
}

/// Modifier combining marks that Unicode orders after other marks of their class
/// but fonts expect first.
const MODIFIER_COMBINING_MARKS: [u32; 14] = [
    0x0654, 0x0655, 0x0658, 0x06DC, 0x06E3, 0x06E7, 0x06E8, 0x08CA, 0x08CB, 0x08CD, 0x08CE, 0x08CF,
    0x08D3, 0x08F3,
];

/// Moves modifier combining marks of class 220 and then 230 to the start of the
/// mark sequence, as HarfBuzz does after the canonical reordering.
pub(super) fn reorder_marks(buffer: &mut Buffer, mut start: usize, end: usize) -> Result<()> {
    let mut i = start;
    for cc in [220, 230] {
        let class = |buffer: &Buffer, k: usize| buffer.infos()[k].combining_class();
        while i < end && class(buffer, i) < cc {
            i += 1;
        }
        if i == end {
            break;
        }
        if class(buffer, i) > cc {
            continue;
        }
        let mut j = i;
        while j < end
            && class(buffer, j) == cc
            && MODIFIER_COMBINING_MARKS.contains(&buffer.infos()[j].glyph)
        {
            j += 1;
        }
        if i == j {
            continue;
        }
        buffer.merge_clusters(start, j)?;
        buffer.infos_mut()[start..j].rotate_left(i - start);
        let new_start = start + j - i;
        let new_class = if cc == 220 { 25 } else { 26 };
        for info in &mut buffer.infos_mut()[start..new_start] {
            info.set_combining_class(new_class);
        }
        start = new_start;
        i = j;
    }
    Ok(())
}
