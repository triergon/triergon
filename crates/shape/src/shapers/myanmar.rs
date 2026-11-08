//! Myanmar (fonts for the `mym2` OpenType tag): syllables, kinzi, and the
//! reordering of medial Ra and pre-base vowels before the base.

use super::Pause;
use super::syllabic::{self, ANY, DottedCircle, Expr, Machine, for_each_syllable, syllable_type};
use crate::buffer::{Buffer, Info};
use crate::metrics::Scaled;
use crate::plan::{Builder, FeatureFlags};
use crate::unicode::{IndicCategory as Cat, IndicPosition as Pos};
use std::ops::Range;
use triergon_core::Result;

/// The basic features, applied one at a time within syllables after
/// reordering.
const BASIC_FEATURES: [&[u8; 4]; 4] = [b"rphf", b"pref", b"blwf", b"pstf"];
/// The presentation features, applied together after syllables are done.
const OTHER_FEATURES: [&[u8; 4]; 4] = [b"pres", b"abvs", b"blws", b"psts"];

/// The steps this shaper takes between lookup stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Finds the syllables.
    SetupSyllables,
    /// Puts each syllable in visual order.
    Reorder,
}

pub(super) fn collect_features(b: &mut Builder<'_>) {
    let per_syllable = FeatureFlags {
        per_syllable: true,
        ..FeatureFlags::GLOBAL
    };
    let manual_zwj = FeatureFlags {
        manual_zwj: true,
        ..FeatureFlags::GLOBAL
    };
    b.add_gsub_pause(Some(Pause::Myanmar(Step::SetupSyllables)));
    b.add_feature(b"locl", per_syllable, 1);
    b.add_feature(b"ccmp", per_syllable, 1);
    b.add_gsub_pause(Some(Pause::Myanmar(Step::Reorder)));
    for tag in BASIC_FEATURES {
        let flags = FeatureFlags {
            per_syllable: true,
            ..manual_zwj
        };
        b.add_feature(tag, flags, 1);
        b.add_gsub_pause(None);
    }
    // Syllables are not needed after this.
    b.add_gsub_pause(None);
    for tag in OTHER_FEATURES {
        b.add_feature(tag, manual_zwj, 1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Syllable {
    Consonant,
    Broken,
    NonMyanmar,
}

/// The syllable grammar of the OpenType Myanmar specification, as HarfBuzz
/// refines it. Independent vowels are `V`, dot below is `N` and other bases
/// (GB) are placeholders.
mod grammar {
    use super::super::syllabic::{Expr, class};
    use super::Cat;
    use Expr::{Alt, Opt, Seq, Star};

    const fn one(category: Cat) -> Expr {
        class(&[category as u8])
    }

    const AS: Expr = one(Cat::As);
    const H: Expr = one(Cat::H);
    const A: Expr = one(Cat::A);
    const DB: Expr = one(Cat::N);
    const IV: Expr = one(Cat::V);
    const VS: Expr = one(Cat::VS);
    const MH: Expr = one(Cat::MH);
    const ML: Expr = one(Cat::ML);
    const C: Expr = class(&[Cat::C as u8, Cat::Ra as u8]);
    pub(super) const JOINER: Expr = class(&[Cat::Zwj as u8, Cat::Zwnj as u8]);
    pub(super) const SMPST: Expr = one(Cat::SMPst);
    /// Kinzi.
    const K: Expr = Seq(&[one(Cat::Ra), AS, H]);

    const MEDIAL_GROUP: Expr = Seq(&[
        Opt(&one(Cat::MY)),
        Opt(&AS),
        Opt(&one(Cat::MR)),
        Opt(&Seq(&[
            Alt(&[
                Seq(&[one(Cat::MW), Opt(&MH), Opt(&ML)]),
                Seq(&[MH, Opt(&ML)]),
                ML,
            ]),
            Opt(&AS),
        ])),
    ]);
    const DOT_BELOW: Expr = Opt(&Seq(&[DB, Opt(&AS)]));
    const MAIN_VOWEL_GROUP: Expr = Seq(&[
        Star(&Seq(&[one(Cat::VPre), Opt(&VS)])),
        Star(&one(Cat::VAbv)),
        Star(&one(Cat::VBlw)),
        Star(&A),
        DOT_BELOW,
    ]);
    const POST_VOWEL_GROUP: Expr = Seq(&[
        one(Cat::VPst),
        Opt(&MH),
        Opt(&ML),
        Star(&AS),
        Star(&one(Cat::VAbv)),
        Star(&A),
        DOT_BELOW,
    ]);
    const TONE_GROUP: Expr = Alt(&[
        class(&[Cat::SM as u8, Cat::SMPst as u8]),
        Seq(&[one(Cat::PT), Star(&A), Opt(&DB), Opt(&AS)]),
    ]);
    const COMPLEX_TAIL: Expr = Seq(&[
        Star(&AS),
        MEDIAL_GROUP,
        MAIN_VOWEL_GROUP,
        Star(&POST_VOWEL_GROUP),
        Star(&TONE_GROUP),
        Opt(&JOINER),
    ]);
    const SYLLABLE_TAIL: Expr = Seq(&[
        Star(&Seq(&[H, Alt(&[C, IV]), Opt(&VS)])),
        Alt(&[H, COMPLEX_TAIL]),
    ]);

    pub(super) const CONSONANT: Expr = Seq(&[
        Opt(&Alt(&[K, one(Cat::CS)])),
        Alt(&[C, IV, one(Cat::Placeholder), one(Cat::DottedCircle)]),
        Opt(&VS),
        SYLLABLE_TAIL,
    ]);
    pub(super) const BROKEN: Expr = Seq(&[Opt(&K), Opt(&VS), SYLLABLE_TAIL]);
}

static MACHINE: Machine = Machine::new(&[
    (grammar::CONSONANT, Syllable::Consonant as u8),
    (
        Expr::Alt(&[grammar::JOINER, grammar::SMPST]),
        Syllable::NonMyanmar as u8,
    ),
    (grammar::BROKEN, Syllable::Broken as u8),
    (ANY, Syllable::NonMyanmar as u8),
]);

fn category(info: &Info) -> Cat {
    Cat::from_index(info.shaper[0])
}

fn position(info: &Info) -> Pos {
    Pos::from_index(info.shaper[1])
}

fn set_position(info: &mut Info, position: Pos) {
    info.shaper[1] = position as u8;
}

fn is_consonant(info: &Info) -> bool {
    !info.ligated()
        && matches!(
            category(info),
            Cat::C | Cat::CS | Cat::Ra | Cat::V | Cat::Placeholder | Cat::DottedCircle
        )
}

pub(super) fn pause(step: Step, buffer: &mut Buffer, font: &Scaled<'_>) -> Result<bool> {
    match step {
        Step::SetupSyllables => syllabic::find_syllables(&MACHINE, buffer, |_, _| false)?,
        Step::Reorder => {
            let circle = DottedCircle {
                broken: Syllable::Broken as u8,
                shaper: [Cat::DottedCircle as u8, Pos::Start as u8],
                after: None,
            };
            syllabic::insert_dotted_circles(buffer, font, circle)?;
            for_each_syllable(buffer, |buffer, range| {
                let kind = syllable_type(&buffer.infos()[range.start]);
                if kind == Syllable::NonMyanmar as u8 {
                    return Ok(());
                }
                reorder_syllable(buffer, range)
            })?;
        }
    }
    Ok(false)
}

/// Sorts a syllable into visual order: kinzi after the base, medial Ra and
/// pre-base vowels before it, below-base vowels and their marks before the
/// rest; several pre-base vowels keep their order.
fn reorder_syllable(buffer: &mut Buffer, range: Range<usize>) -> Result<()> {
    let Range { start, end } = range;
    let infos = buffer.infos_mut();
    let has_kinzi = start + 3 <= end
        && category(&infos[start]) == Cat::Ra
        && category(&infos[start + 1]) == Cat::As
        && category(&infos[start + 2]) == Cat::H;
    let limit = if has_kinzi { start + 3 } else { start };
    let base = (limit..end)
        .find(|&i| is_consonant(&infos[i]))
        .unwrap_or(start);

    // Kinzi goes after the base, everything else before the base before it.
    let mut i = start;
    while i < base.max(limit).min(end) {
        let kinzi = has_kinzi && i < start + 3;
        set_position(
            &mut infos[i],
            if kinzi { Pos::AfterMain } else { Pos::PreC },
        );
        i += 1;
    }
    if i < end {
        set_position(&mut infos[i], Pos::BaseC);
        i += 1;
    }
    let mut pos = Pos::AfterMain;
    for i in i..end {
        let cat = category(&infos[i]);
        let new = match cat {
            Cat::MR => Pos::PreC,
            Cat::VPre => Pos::PreM,
            Cat::VS => position(&infos[i - 1]),
            Cat::VBlw if pos == Pos::AfterMain => {
                pos = Pos::BelowC;
                pos
            }
            Cat::A if pos == Pos::BelowC => Pos::BeforeSub,
            Cat::VBlw if pos == Pos::BelowC => pos,
            _ if pos == Pos::BelowC => {
                pos = Pos::AfterSub;
                pos
            }
            _ => pos,
        };
        set_position(&mut infos[i], new);
    }

    buffer.sort_by_key(start, end, |info| info.shaper[1])?;

    let infos = buffer.infos();
    let mut left = (start..end).filter(|&i| position(&infos[i]) == Pos::PreM);
    let (first, last) = (left.next(), left.next_back());
    if let (Some(first), Some(last)) = (first, last)
        && first < last
    {
        buffer.reverse_range(first, last + 1);
        let mut i = first;
        for j in first..=last {
            if category(&buffer.infos()[j]) == Cat::VPre {
                buffer.reverse_range(i, j + 1);
                i = j + 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &[Cat]) -> Vec<(usize, Syllable)> {
        let types = [Syllable::Consonant, Syllable::Broken, Syllable::NonMyanmar];
        let mut out = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let (found, _) = MACHINE.longest(text[start..].iter().map(|&c| c as u8));
            let (len, rule) = found.unwrap();
            out.push((len, types[usize::from(rule)]));
            start += len;
        }
        out
    }

    #[test]
    fn syllables_follow_the_grammar() {
        use Cat::*;
        // Kinzi, consonant, medial Ra, vowel sign E.
        assert_eq!(kinds(&[Ra, As, H, C, MR, VPre]), [(6, Syllable::Consonant)]);
        assert_eq!(
            kinds(&[Zwj, VPre]),
            [(1, Syllable::NonMyanmar), (1, Syllable::Broken)]
        );
        assert_eq!(kinds(&[C, VPst, SM]), [(3, Syllable::Consonant)]);
    }
}
