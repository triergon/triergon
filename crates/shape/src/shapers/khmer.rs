//! Khmer: syllables, the coeng Ro and pre-base vowels moved before the base,
//! and the split vowels Unicode does not decompose.

use super::Pause;
use super::syllabic::{self, ANY, DottedCircle, Machine, for_each_syllable, syllable_type};
use crate::buffer::{Buffer, Info, ShaperProps};
use crate::metrics::Scaled;
use crate::plan::{Builder, FeatureFlags, Plan};
use crate::unicode::{self, IndicCategory as Cat, IndicPosition as Pos};
use std::ops::Range;
use triergon_core::Result;

/// The basic features, applied together within syllables after reordering.
const BASIC_FEATURES: [&[u8; 4]; 5] = [b"pref", b"blwf", b"abvf", b"pstf", b"cfar"];
/// The presentation features, applied together after syllables are done.
const OTHER_FEATURES: [&[u8; 4]; 4] = [b"pres", b"abvs", b"blws", b"psts"];

/// The steps this shaper takes between lookup stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Finds the syllables.
    SetupSyllables,
    /// Puts each syllable in visual order and gives it the basic feature masks.
    Reorder,
}

pub(super) fn collect_features(b: &mut Builder<'_>) {
    b.add_gsub_pause(Some(Pause::Khmer(Step::SetupSyllables)));
    b.add_gsub_pause(Some(Pause::Khmer(Step::Reorder)));
    // Uniscribe applies the basic features without pauses between them.
    let per_syllable = FeatureFlags {
        per_syllable: true,
        ..FeatureFlags::GLOBAL
    };
    b.add_feature(b"locl", per_syllable, 1);
    b.add_feature(b"ccmp", per_syllable, 1);
    let basic = FeatureFlags {
        manual_zwnj: true,
        manual_zwj: true,
        per_syllable: true,
        ..FeatureFlags::NONE
    };
    for tag in BASIC_FEATURES {
        b.add_feature(tag, basic, 1);
    }
    // Syllables are not needed after this.
    b.add_gsub_pause(None);
    for tag in OTHER_FEATURES {
        b.add_feature(tag, FeatureFlags::GLOBAL_MANUAL_JOINERS, 1);
    }
}

/// `clig` is one of the required Khmer features; `liga` is not.
pub(super) fn override_features(b: &mut Builder<'_>) {
    b.enable_feature(b"clig");
    b.disable_feature(b"liga");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Syllable {
    Consonant,
    Broken,
    NonKhmer,
}

/// The syllable grammar HarfBuzz extracted from what Uniscribe allows.
mod grammar {
    use super::super::syllabic::{Expr, class};
    use super::Cat;
    use Expr::{Alt, Opt, Seq, Star};

    const fn one(category: Cat) -> Expr {
        class(&[category as u8])
    }

    const C: Expr = class(&[Cat::C as u8, Cat::Ra as u8, Cat::V as u8]);
    const H: Expr = one(Cat::H);
    const JOINER: Expr = class(&[Cat::Zwj as u8, Cat::Zwnj as u8]);
    const ROBATIC: Expr = one(Cat::Robatic);
    const CN: Expr = Seq(&[C, Opt(&Seq(&[Opt(&JOINER), ROBATIC]))]);
    const XGROUP: Expr = Star(&Seq(&[Star(&JOINER), one(Cat::Xgroup)]));
    const YGROUP: Expr = Star(&one(Cat::Ygroup));
    const MATRA_GROUP: Expr = Seq(&[
        Opt(&one(Cat::VPre)),
        XGROUP,
        Opt(&one(Cat::VBlw)),
        XGROUP,
        Opt(&Seq(&[Opt(&JOINER), one(Cat::VAbv)])),
        XGROUP,
        Opt(&one(Cat::VPst)),
    ]);
    const SYLLABLE_TAIL: Expr = Seq(&[XGROUP, MATRA_GROUP, XGROUP, Opt(&Seq(&[H, C])), YGROUP]);

    pub(super) const BROKEN: Expr = Seq(&[
        Opt(&ROBATIC),
        Star(&Seq(&[H, CN])),
        Alt(&[H, SYLLABLE_TAIL]),
    ]);
    pub(super) const CONSONANT: Expr = Seq(&[
        Alt(&[CN, one(Cat::Placeholder), one(Cat::DottedCircle)]),
        BROKEN,
    ]);
}

static MACHINE: Machine = Machine::new(&[
    (grammar::CONSONANT, Syllable::Consonant as u8),
    (grammar::BROKEN, Syllable::Broken as u8),
    (ANY, Syllable::NonKhmer as u8),
]);

fn category(info: &Info) -> Cat {
    match info.shaper {
        ShaperProps::Indic(category, _) => category,
        _ => Cat::X,
    }
}

pub(super) fn pause(
    step: Step,
    plan: &Plan,
    buffer: &mut Buffer,
    font: &Scaled<'_>,
) -> Result<bool> {
    match step {
        Step::SetupSyllables => syllabic::find_syllables(&MACHINE, buffer, |_, _| false)?,
        Step::Reorder => {
            let circle = DottedCircle {
                broken: Syllable::Broken as u8,
                shaper: ShaperProps::Indic(Cat::DottedCircle, Pos::Start),
                after: None,
            };
            syllabic::insert_dotted_circles(buffer, font, circle)?;
            let masks = BASIC_FEATURES.map(|tag| plan.one_mask(tag));
            for_each_syllable(buffer, |buffer, range| {
                let kind = syllable_type(&buffer.infos()[range.start]);
                if kind == Syllable::NonKhmer as u8 {
                    return Ok(());
                }
                reorder_syllable(&masks, buffer, range)
            })?;
        }
    }
    Ok(false)
}

/// Gives the glyphs after the first the post-base form masks, moves a coeng
/// and Ro (among the first two subscripts) to the start for `pref`, with
/// `cfar` on what follows, and moves pre-base vowels to the start.
fn reorder_syllable(masks: &[u32; 5], buffer: &mut Buffer, range: Range<usize>) -> Result<()> {
    let Range { start, end } = range;
    let [pref, blwf, abvf, pstf, cfar] = *masks;
    for info in &mut buffer.infos_mut()[start + 1..end] {
        info.mask |= blwf | abvf | pstf;
    }
    let mut coengs = 0;
    for i in start + 1..end {
        let cat = category(&buffer.infos()[i]);
        if cat == Cat::H && coengs <= 2 && i + 1 < end {
            coengs += 1;
            if category(&buffer.infos()[i + 1]) == Cat::Ra {
                for info in &mut buffer.infos_mut()[i..i + 2] {
                    info.mask |= pref;
                }
                buffer.merge_clusters(start, i + 2)?;
                let infos = buffer.infos_mut();
                infos[start..i + 2].rotate_right(2);
                for info in &mut infos[i + 2..end] {
                    info.mask |= cfar;
                }
                coengs = 2;
            }
        } else if cat == Cat::VPre {
            buffer.merge_clusters(start, i + 1)?;
            buffer.infos_mut()[start..=i].rotate_right(1);
        }
    }
    Ok(())
}

/// Khmer decomposition: Unicode's, and the vowels written with a pre-base
/// E and a second part that Unicode keeps whole.
pub(super) fn decompose(ab: u32) -> Option<(u32, u32)> {
    match ab {
        0x17BE..=0x17C0 | 0x17C4 | 0x17C5 => Some((0x17C1, ab)),
        _ => unicode::code_point::decompose(ab),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &[Cat]) -> Vec<(usize, Syllable)> {
        let types = [Syllable::Consonant, Syllable::Broken, Syllable::NonKhmer];
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
        assert_eq!(kinds(&[C, H, Ra, VPre]), [(4, Syllable::Consonant)]);
        assert_eq!(
            kinds(&[VAbv, C]),
            [(1, Syllable::Broken), (1, Syllable::Consonant)]
        );
        assert_eq!(
            kinds(&[C, Robatic, Xgroup, Ygroup]),
            [(4, Syllable::Consonant)]
        );
        assert_eq!(kinds(&[X]), [(1, Syllable::NonKhmer)]);
    }

    #[test]
    fn split_vowels_decompose() {
        assert_eq!(decompose(0x17BE), Some((0x17C1, 0x17BE)));
        assert_eq!(decompose(0x17C1), None);
    }
}
