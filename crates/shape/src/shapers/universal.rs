//! The Universal Shaping Engine: the shaper of most other complex scripts
//! (Balinese, Javanese, Sinhala, Tibetan and many more), and of Indic scripts in
//! fonts made for their `*3` OpenType tags. Syllables follow one grammar over
//! the USE categories; reph moves to the end and pre-base vowels to the start of
//! their cluster after the basic forms.

use super::Pause;
use super::arabic;
use super::syllabic::{self, ANY, DottedCircle, Expr, Machine, for_each_syllable, syllable_type};
use crate::buffer::{Buffer, Info};
use crate::metrics::Scaled;
use crate::plan::{Builder, FeatureFlags, Plan};
use crate::tag::Script;
use crate::unicode::{self, UseCategory as Cat};
use std::ops::Range;
use triergon_core::Result;

/// The basic features, applied together within syllables before reordering.
const BASIC_FEATURES: [&[u8; 4]; 7] = [
    b"rkrf", b"abvf", b"blwf", b"half", b"pstf", b"vatu", b"cjct",
];
/// The joining forms, in the order of [`Form`].
const TOPOGRAPHICAL_FEATURES: [&[u8; 4]; 4] = [b"isol", b"init", b"medi", b"fina"];

/// The joining form of a cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    Isol,
    Init,
    Medi,
    Fina,
}
/// The presentation features, applied together after reordering.
const OTHER_FEATURES: [&[u8; 4]; 5] = [b"abvs", b"blws", b"haln", b"pres", b"psts"];

/// The steps this shaper takes between lookup stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Finds the syllables and the glyphs that may form reph.
    SetupSyllables,
    /// Forgets which glyphs were substituted, before a feature whose substitutions are recorded.
    ClearSubstitutionFlags,
    /// Records which glyphs became reph.
    RecordRphf,
    /// Records which glyphs became pre-base forms.
    RecordPref,
    /// Moves reph and pre-base glyphs to their places.
    Reorder,
}

pub(super) fn collect_features(b: &mut Builder<'_>) {
    let per_syllable = FeatureFlags {
        per_syllable: true,
        ..FeatureFlags::GLOBAL
    };
    let manual_zwj = FeatureFlags {
        manual_zwj: true,
        ..per_syllable
    };
    b.add_gsub_pause(Some(Pause::Universal(Step::SetupSyllables)));
    // Default glyph pre-processing.
    b.add_feature(b"locl", per_syllable, 1);
    b.add_feature(b"ccmp", per_syllable, 1);
    b.add_feature(b"nukt", per_syllable, 1);
    b.add_feature(b"akhn", manual_zwj, 1);
    // Reordering.
    b.add_gsub_pause(Some(Pause::Universal(Step::ClearSubstitutionFlags)));
    let rphf = FeatureFlags {
        global: false,
        ..manual_zwj
    };
    b.add_feature(b"rphf", rphf, 1);
    b.add_gsub_pause(Some(Pause::Universal(Step::RecordRphf)));
    b.add_gsub_pause(Some(Pause::Universal(Step::ClearSubstitutionFlags)));
    b.add_feature(b"pref", manual_zwj, 1);
    b.add_gsub_pause(Some(Pause::Universal(Step::RecordPref)));
    // Orthographic units.
    for tag in BASIC_FEATURES {
        b.add_feature(tag, manual_zwj, 1);
    }
    b.add_gsub_pause(Some(Pause::Universal(Step::Reorder)));
    // Syllables are not needed after this.
    b.add_gsub_pause(None);
    for tag in TOPOGRAPHICAL_FEATURES {
        b.add_feature(tag, FeatureFlags::NONE, 1);
    }
    b.add_gsub_pause(None);
    let presentation = FeatureFlags {
        manual_zwj: true,
        ..FeatureFlags::GLOBAL
    };
    for tag in OTHER_FEATURES {
        b.add_feature(tag, presentation, 1);
    }
}

/// The syllable types, in the order of the grammar's rules (the last two
/// rules find clusters of no syllable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Syllable {
    ViramaTerminated,
    SakotTerminated,
    Standard,
    NumberJoinerTerminated,
    Numeral,
    Symbol,
    Hieroglyph,
    Broken,
    NonCluster,
}

/// The cluster grammar of the USE specification, as HarfBuzz refines it.
mod grammar {
    use super::super::syllabic::{Expr, class};
    use super::Cat;
    use Expr::{Alt, Opt, Plus, Seq, Star};

    const fn one(category: Cat) -> Expr {
        class(&[category as u8])
    }

    const B: Expr = one(Cat::B);
    pub(super) const N: Expr = one(Cat::N);
    const GB: Expr = one(Cat::GB);
    const SUB: Expr = one(Cat::Sub);
    const H: Expr = one(Cat::H);
    const HN: Expr = one(Cat::HN);
    pub(super) const ZWNJ: Expr = one(Cat::Zwnj);
    const R: Expr = one(Cat::R);
    const CS: Expr = one(Cat::CS);
    const IS: Expr = one(Cat::IS);
    const SK: Expr = one(Cat::Sk);
    const G: Expr = one(Cat::G);
    const J: Expr = one(Cat::J);
    const SB: Expr = one(Cat::SB);
    const SE: Expr = one(Cat::SE);
    const HVM: Expr = one(Cat::Hvm);
    const HM: Expr = one(Cat::HM);
    const HR: Expr = one(Cat::HR);
    const RK: Expr = one(Cat::RK);
    const O: Expr = one(Cat::O);
    pub(super) const FMPST: Expr = one(Cat::FMPst);
    /// The halants: halant, halant or vowel modifier, invisible stacker, sakot.
    const HALANT: Expr = class(&[Cat::H as u8, Cat::Hvm as u8, Cat::IS as u8, Cat::Sk as u8]);

    const CONSONANT_MODIFIERS: Expr = Seq(&[
        Star(&one(Cat::CMAbv)),
        Star(&one(Cat::CMBlw)),
        Star(&Seq(&[
            Alt(&[Seq(&[HALANT, B]), SUB]),
            Star(&one(Cat::CMAbv)),
            Star(&one(Cat::CMBlw)),
        ])),
    ]);
    const MEDIAL_CONSONANTS: Expr = Seq(&[
        Opt(&one(Cat::MPre)),
        Opt(&one(Cat::MAbv)),
        Opt(&one(Cat::MBlw)),
        Opt(&one(Cat::MPst)),
    ]);
    const DEPENDENT_VOWELS: Expr = Alt(&[
        Seq(&[
            Star(&one(Cat::VPre)),
            Star(&one(Cat::VAbv)),
            Star(&one(Cat::VBlw)),
            Star(&one(Cat::VPst)),
        ]),
        H,
    ]);
    const VOWEL_MODIFIERS: Expr = Seq(&[
        Opt(&HVM),
        Star(&one(Cat::VMPre)),
        Star(&one(Cat::VMAbv)),
        Star(&one(Cat::VMBlw)),
        Star(&one(Cat::VMPst)),
    ]);
    const FINAL_CONSONANTS: Expr = Seq(&[
        Star(&one(Cat::FAbv)),
        Star(&one(Cat::FBlw)),
        Star(&one(Cat::FPst)),
    ]);
    const FINAL_MODIFIERS: Expr = Alt(&[
        Seq(&[Star(&one(Cat::FMAbv)), Star(&one(Cat::FMBlw))]),
        Opt(&FMPST),
    ]);
    const START: Expr = Seq(&[Opt(&Alt(&[R, CS])), Alt(&[B, GB])]);
    const MIDDLE: Expr = Seq(&[
        CONSONANT_MODIFIERS,
        MEDIAL_CONSONANTS,
        DEPENDENT_VOWELS,
        VOWEL_MODIFIERS,
        Star(&Seq(&[SK, B])),
    ]);
    const COMPLEX_TAIL: Expr = Seq(&[MIDDLE, FINAL_CONSONANTS, FINAL_MODIFIERS]);
    const NUMBER_JOINER_TAIL: Expr = Seq(&[Star(&Seq(&[HN, N])), HN]);
    const NUMERAL_TAIL: Expr = Plus(&Seq(&[HN, N]));
    const SYMBOL_TAIL: Expr = Alt(&[
        Seq(&[Plus(&one(Cat::SMAbv)), Star(&one(Cat::SMBlw))]),
        Plus(&one(Cat::SMBlw)),
    ]);
    const VIRAMA_TAIL: Expr = Seq(&[CONSONANT_MODIFIERS, Alt(&[IS, RK])]);
    const SAKOT_TAIL: Expr = Seq(&[MIDDLE, SK]);
    const TAIL: Expr = Alt(&[COMPLEX_TAIL, SAKOT_TAIL, SYMBOL_TAIL, VIRAMA_TAIL]);

    pub(super) const VIRAMA_TERMINATED: Expr = Seq(&[START, VIRAMA_TAIL]);
    pub(super) const SAKOT_TERMINATED: Expr = Seq(&[START, SAKOT_TAIL]);
    pub(super) const STANDARD: Expr = Seq(&[START, COMPLEX_TAIL]);
    pub(super) const NUMBER_JOINER_TERMINATED: Expr = Seq(&[N, NUMBER_JOINER_TAIL]);
    pub(super) const NUMERAL: Expr = Seq(&[N, Opt(&NUMERAL_TAIL)]);
    pub(super) const SYMBOL: Expr = Seq(&[Alt(&[O, GB, SB]), Opt(&TAIL)]);
    const GLYPH: Expr = Seq(&[G, Opt(&HR), Opt(&HM), Star(&SE)]);
    pub(super) const HIEROGLYPH: Expr =
        Seq(&[Star(&SB), GLYPH, Star(&Seq(&[J, Star(&SB), Opt(&GLYPH)]))]);
    pub(super) const BROKEN: Expr = Seq(&[Opt(&R), Alt(&[TAIL, NUMBER_JOINER_TAIL, NUMERAL_TAIL])]);
}

/// The rules: each kind of cluster, with an optional ZWNJ after it.
static MACHINE: Machine = {
    use Expr::{Opt, Seq};
    use grammar::ZWNJ;
    Machine::new(&[
        (
            Seq(&[grammar::VIRAMA_TERMINATED, Opt(&ZWNJ)]),
            Syllable::ViramaTerminated as u8,
        ),
        (
            Seq(&[grammar::SAKOT_TERMINATED, Opt(&ZWNJ)]),
            Syllable::SakotTerminated as u8,
        ),
        (
            Seq(&[grammar::STANDARD, Opt(&ZWNJ)]),
            Syllable::Standard as u8,
        ),
        (
            Seq(&[grammar::NUMBER_JOINER_TERMINATED, Opt(&ZWNJ)]),
            Syllable::NumberJoinerTerminated as u8,
        ),
        (
            Seq(&[grammar::NUMERAL, Opt(&ZWNJ)]),
            Syllable::Numeral as u8,
        ),
        (Seq(&[grammar::SYMBOL, Opt(&ZWNJ)]), Syllable::Symbol as u8),
        (
            Seq(&[grammar::HIEROGLYPH, Opt(&ZWNJ)]),
            Syllable::Hieroglyph as u8,
        ),
        (grammar::FMPST, Syllable::NonCluster as u8),
        (Seq(&[grammar::BROKEN, Opt(&ZWNJ)]), Syllable::Broken as u8),
        (ANY, Syllable::NonCluster as u8),
    ])
};

fn category(info: &Info) -> Cat {
    Cat::from_index(info.shaper[0])
}

fn set_category(info: &mut Info, category: Cat) {
    info.shaper[0] = category as u8;
}

/// Scripts whose letters join as Arabic letters do.
fn has_arabic_joining(script: Script) -> bool {
    matches!(
        &script.to_bytes(),
        b"Adlm"
            | b"Arab"
            | b"Chrs"
            | b"Rohg"
            | b"Mand"
            | b"Mani"
            | b"Mong"
            | b"Nkoo"
            | b"Ougr"
            | b"Phag"
            | b"Phlp"
            | b"Sogd"
            | b"Syrc"
    )
}

/// Joining forms for scripts that join, then every character's USE category.
pub(super) fn setup_masks(plan: &Plan, buffer: &mut Buffer) {
    if has_arabic_joining(plan.script) {
        arabic::setup_masks(plan, buffer);
        if &plan.script.to_bytes() == b"Mong" {
            arabic::copy_to_mongolian_variation_selectors(plan, buffer);
        }
    }
    for info in buffer.infos_mut() {
        set_category(info, unicode::use_category(info.glyph));
    }
}

pub(super) fn pause(
    step: Step,
    plan: &Plan,
    buffer: &mut Buffer,
    font: &Scaled<'_>,
) -> Result<bool> {
    let rphf = plan.one_mask(b"rphf");
    match step {
        Step::SetupSyllables => {
            syllabic::find_syllables(&MACHINE, buffer, hidden_from_grammar)?;
            if rphf != 0 {
                for_each_syllable(buffer, |buffer, Range { start, end }| {
                    let infos = buffer.infos_mut();
                    let limit = if category(&infos[start]) == Cat::R {
                        1
                    } else {
                        3.min(end - start)
                    };
                    for info in &mut infos[start..start + limit] {
                        info.mask |= rphf;
                    }
                    Ok(())
                })?;
            }
            if !has_arabic_joining(plan.script) {
                setup_topographical_masks(plan, buffer)?;
            }
        }
        Step::ClearSubstitutionFlags => {
            for info in buffer.infos_mut() {
                info.glyph_props = info.glyph_props.without_substitution();
            }
        }
        Step::RecordRphf => {
            if rphf == 0 {
                return Ok(false);
            }
            // A substituted repha becomes R.
            for_each_syllable(buffer, |buffer, range| {
                if let Some(info) = buffer.infos_mut()[range]
                    .iter_mut()
                    .take_while(|info| info.mask & rphf != 0)
                    .find(|info| info.substituted())
                {
                    set_category(info, Cat::R);
                }
                Ok(())
            })?;
        }
        Step::RecordPref => {
            // A substituted pre-base form reorders as a pre-base vowel.
            for_each_syllable(buffer, |buffer, range| {
                if let Some(info) = buffer.infos_mut()[range]
                    .iter_mut()
                    .find(|info| info.substituted())
                {
                    set_category(info, Cat::VPre);
                }
                Ok(())
            })?;
        }
        Step::Reorder => {
            let circle = DottedCircle {
                broken: Syllable::Broken as u8,
                shaper: [Cat::B as u8, 0],
                after: Some(Cat::R as u8),
            };
            syllabic::insert_dotted_circles(buffer, font, circle)?;
            for_each_syllable(buffer, reorder_syllable)?;
        }
    }
    Ok(false)
}

/// Glyphs the grammar does not see: CGJ (and the other joiners and variation
/// selectors of that category), and ZWNJ before a mark.
fn hidden_from_grammar(infos: &[Info], i: usize) -> bool {
    match category(&infos[i]) {
        Cat::Cgj => true,
        Cat::Zwnj => infos[i + 1..]
            .iter()
            .find(|info| category(info) != Cat::Cgj)
            .is_some_and(Info::is_unicode_mark),
        _ => false,
    }
}

/// The joining forms of scripts without Arabic joining: each cluster is
/// isolated, or the final of a word whose earlier clusters become initial and
/// medial.
fn setup_topographical_masks(plan: &Plan, buffer: &mut Buffer) -> Result<()> {
    let masks = TOPOGRAPHICAL_FEATURES.map(|tag| {
        let mask = plan.one_mask(tag);
        if mask == plan.global_mask { 0 } else { mask }
    });
    let all = masks.iter().fold(0, |all, mask| all | mask);
    if all == 0 {
        return Ok(());
    }
    let mask = |form: Form| masks[form as usize];
    let mut last_start = 0;
    let mut last_form = None;
    for_each_syllable(buffer, |buffer, Range { start, end }| {
        let kind = syllable_type(&buffer.infos()[start]);
        let infos = buffer.infos_mut();
        if kind == Syllable::Hieroglyph as u8 || kind == Syllable::NonCluster as u8 {
            last_form = None;
        } else {
            let join = matches!(last_form, Some(Form::Fina | Form::Isol));
            if join {
                let form = if last_form == Some(Form::Fina) {
                    Form::Medi
                } else {
                    Form::Init
                };
                for info in &mut infos[last_start..start] {
                    info.mask = (info.mask & !all) | mask(form);
                }
            }
            let form = if join { Form::Fina } else { Form::Isol };
            for info in &mut infos[start..end] {
                info.mask = (info.mask & !all) | mask(form);
            }
            last_form = Some(form);
        }
        last_start = start;
        Ok(())
    })
}

fn is_halant(info: &Info) -> bool {
    matches!(category(info), Cat::H | Cat::Hvm | Cat::IS) && !info.ligated()
}

/// Whether a glyph belongs after the base: a final, medial, vowel or vowel
/// modifier.
fn is_post_base(info: &Info) -> bool {
    matches!(
        category(info),
        Cat::FAbv
            | Cat::FBlw
            | Cat::FPst
            | Cat::FMAbv
            | Cat::FMBlw
            | Cat::FMPst
            | Cat::MAbv
            | Cat::MBlw
            | Cat::MPst
            | Cat::MPre
            | Cat::VAbv
            | Cat::VBlw
            | Cat::VPst
            | Cat::VPre
            | Cat::VMAbv
            | Cat::VMBlw
            | Cat::VMPst
            | Cat::VMPre
    ) || is_halant(info)
}

/// Moves a repha to before the first post-base glyph (or to the end), and
/// pre-base vowels and vowel modifiers to the start of the cluster or after the
/// last halant before them.
fn reorder_syllable(buffer: &mut Buffer, range: Range<usize>) -> Result<()> {
    let Range { start, end } = range;
    let kind = syllable_type(&buffer.infos()[start]);
    if ![
        Syllable::ViramaTerminated,
        Syllable::SakotTerminated,
        Syllable::Standard,
        Syllable::Symbol,
        Syllable::Broken,
    ]
    .iter()
    .any(|&s| s as u8 == kind)
    {
        return Ok(());
    }
    if category(&buffer.infos()[start]) == Cat::R && end - start > 1 {
        let infos = buffer.infos();
        let target = (start + 1..end).find_map(|i| {
            if is_post_base(&infos[i]) {
                Some(i - 1)
            } else {
                (i == end - 1).then_some(i)
            }
        });
        if let Some(target) = target {
            buffer.merge_clusters(start, target + 1)?;
            buffer.infos_mut()[start..=target].rotate_left(1);
        }
    }
    let mut j = start;
    for i in start..end {
        let info = buffer.infos()[i];
        if is_halant(&info) {
            j = i + 1;
        } else if matches!(category(&info), Cat::VPre | Cat::VMPre)
            && info.lig.component() == 0
            && j < i
        {
            // Only the first component of a multiple substitution moves.
            buffer.merge_clusters(j, i + 1)?;
            buffer.infos_mut()[j..=i].rotate_right(1);
        }
    }
    Ok(())
}

/// USE composition: Unicode's, but never of a mark with what follows, so that
/// split vowel signs stay split.
pub(super) fn compose(a: u32, b: u32) -> Option<u32> {
    if unicode::general_category(a).is_mark() {
        return None;
    }
    unicode::compose(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &[Cat]) -> Vec<(usize, Syllable)> {
        let types = [
            Syllable::ViramaTerminated,
            Syllable::SakotTerminated,
            Syllable::Standard,
            Syllable::NumberJoinerTerminated,
            Syllable::Numeral,
            Syllable::Symbol,
            Syllable::Hieroglyph,
            Syllable::Broken,
            Syllable::NonCluster,
        ];
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
    fn clusters_follow_the_grammar() {
        use Cat::*;
        assert_eq!(kinds(&[B, H, B, VPre, VMPst]), [(5, Syllable::Standard)]);
        assert_eq!(kinds(&[R, B, IS]), [(3, Syllable::ViramaTerminated)]);
        assert_eq!(
            kinds(&[VPre, B]),
            [(1, Syllable::Broken), (1, Syllable::Standard)]
        );
        assert_eq!(
            kinds(&[N, HN, N, HN]),
            [(4, Syllable::NumberJoinerTerminated)]
        );
        assert_eq!(kinds(&[G, J, G]), [(3, Syllable::Hieroglyph)]);
        assert_eq!(kinds(&[O, SMAbv]), [(2, Syllable::Symbol)]);
        assert_eq!(
            kinds(&[FMPst, WJ]),
            [(1, Syllable::NonCluster), (1, Syllable::NonCluster)]
        );
        assert_eq!(kinds(&[B, Zwnj]), [(2, Syllable::Standard)]);
    }
}
