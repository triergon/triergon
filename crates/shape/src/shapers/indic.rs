//! The Indic scripts (Bengali, Devanagari, Gujarati, Gurmukhi, Kannada,
//! Malayalam, Oriya, Tamil, Telugu): syllables, reordering into visual order
//! around the base consonant, reph and pre-base forms, as HarfBuzz's Indic
//! shaper does it for both the old (`deva`) and new (`dev2`) OpenType script
//! specifications.

use super::Pause;
use super::syllabic::{self, ANY, DottedCircle, Machine, for_each_syllable, syllable_type};
use crate::buffer::{Buffer, Info, ShaperProps};
use crate::face::Face;
use crate::metrics::Scaled;
use crate::ot::would_apply::would_substitute;
use crate::plan::{Builder, FeatureFlags, LookupMap, Plan};
use crate::tag::Script;
use crate::unicode::{self, IndicCategory as Cat, IndicPosition as Pos};
use std::ops::Range;
use triergon_core::{Result, WorkBudget};

/// How reph is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RephMode {
    /// An initial Ra and halant.
    Implicit,
    /// An initial Ra, halant and ZWJ.
    Explicit,
    /// A repha character in logical order.
    Logical,
}

/// What each script does differently.
#[derive(Clone, Copy, Debug)]
struct Config {
    /// Whether the script has an old-specification OpenType tag (`deva`) besides
    /// the new one (`dev2`).
    has_old_spec: bool,
    virama: u32,
    /// Where reph goes: after main, before or after sub-base forms, or before or
    /// after post-base forms.
    reph_position: Pos,
    reph_mode: RephMode,
    /// Whether below-base forms apply before the base too.
    below_forms_before_base: bool,
}

impl Config {
    fn of(script: Script) -> Self {
        let (virama, reph_position, reph_mode) = match &script.to_bytes() {
            b"Deva" => (0x094D, Pos::BeforePost, RephMode::Implicit),
            b"Beng" => (0x09CD, Pos::AfterSub, RephMode::Implicit),
            b"Guru" => (0x0A4D, Pos::BeforeSub, RephMode::Implicit),
            b"Gujr" => (0x0ACD, Pos::BeforePost, RephMode::Implicit),
            b"Orya" => (0x0B4D, Pos::AfterMain, RephMode::Implicit),
            b"Taml" => (0x0BCD, Pos::AfterPost, RephMode::Implicit),
            b"Telu" => (0x0C4D, Pos::AfterPost, RephMode::Explicit),
            b"Knda" => (0x0CCD, Pos::AfterPost, RephMode::Implicit),
            b"Mlym" => (0x0D4D, Pos::AfterMain, RephMode::Logical),
            _ => {
                return Self {
                    has_old_spec: false,
                    virama: 0,
                    reph_position: Pos::BeforePost,
                    reph_mode: RephMode::Implicit,
                    below_forms_before_base: true,
                };
            }
        };
        Self {
            has_old_spec: true,
            virama,
            reph_position,
            reph_mode,
            below_forms_before_base: !matches!(&script.to_bytes(), b"Telu" | b"Knda"),
        }
    }
}

/// The features applied one at a time between the reorderings, then those
/// applied together after the final one, all within syllables; each with
/// whether it applies to every glyph (the others go where the reorderings put
/// their masks).
const FEATURES: [(&[u8; 4], bool); 17] = [
    (b"nukt", true),
    (b"akhn", true),
    (b"rphf", false),
    (b"rkrf", true),
    (b"pref", false),
    (b"blwf", false),
    (b"abvf", false),
    (b"half", false),
    (b"pstf", false),
    (b"vatu", true),
    (b"cjct", true),
    (b"init", false),
    (b"pres", true),
    (b"abvs", true),
    (b"blws", true),
    (b"psts", true),
    (b"haln", true),
];
const BASIC_FEATURES: usize = 11;

/// The masks of the features the reorderings give to some glyphs only.
#[derive(Clone, Copy, Debug)]
struct Masks {
    rphf: u32,
    pref: u32,
    blwf: u32,
    abvf: u32,
    half: u32,
    pstf: u32,
    init: u32,
}

impl Masks {
    fn of(plan: &Plan) -> Self {
        Self {
            rphf: plan.one_mask(b"rphf"),
            pref: plan.one_mask(b"pref"),
            blwf: plan.one_mask(b"blwf"),
            abvf: plan.one_mask(b"abvf"),
            half: plan.one_mask(b"half"),
            pstf: plan.one_mask(b"pstf"),
            init: plan.one_mask(b"init"),
        }
    }
}

/// The steps this shaper takes between lookup stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Finds the syllables.
    SetupSyllables,
    /// Puts each syllable in visual order before the basic forms.
    InitialReordering,
    /// Moves reph, pre-base forms and left matras after the basic forms.
    FinalReordering,
}

pub(super) fn collect_features(b: &mut Builder<'_>) {
    let per_syllable = |global| FeatureFlags {
        global,
        manual_zwnj: true,
        manual_zwj: true,
        per_syllable: true,
        ..FeatureFlags::NONE
    };
    b.add_gsub_pause(Some(Pause::Indic(Step::SetupSyllables)));
    let early = FeatureFlags {
        per_syllable: true,
        ..FeatureFlags::GLOBAL
    };
    b.add_feature(b"locl", early, 1);
    b.add_feature(b"ccmp", early, 1);
    b.add_gsub_pause(Some(Pause::Indic(Step::InitialReordering)));
    for &(tag, global) in &FEATURES[..BASIC_FEATURES] {
        b.add_feature(tag, per_syllable(global), 1);
        b.add_gsub_pause(None);
    }
    b.add_gsub_pause(Some(Pause::Indic(Step::FinalReordering)));
    for &(tag, global) in &FEATURES[BASIC_FEATURES..] {
        b.add_feature(tag, per_syllable(global), 1);
    }
}

pub(super) fn override_features(b: &mut Builder<'_>) {
    b.disable_feature(b"liga");
    // Syllables are not needed after this.
    b.add_gsub_pause(None);
}

/// The syllable types, in the order of the grammar's rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Syllable {
    Consonant,
    Vowel,
    Standalone,
    Symbol,
    Broken,
    NonIndic,
}

/// The syllable grammar, from the OpenType Indic script specifications as
/// HarfBuzz refines them.
mod grammar {
    use super::super::syllabic::{Expr, class};
    use super::Cat;
    use Expr::{Alt, Opt, Seq, Star};

    pub(super) const C: Expr = class(&[Cat::C as u8, Cat::Ra as u8]);
    const N: Expr = class(&[Cat::N as u8]);
    const H: Expr = class(&[Cat::H as u8]);
    const ZWJ: Expr = class(&[Cat::Zwj as u8]);
    const ZWNJ: Expr = class(&[Cat::Zwnj as u8]);
    const RS: Expr = class(&[Cat::RS as u8]);
    const V: Expr = class(&[Cat::V as u8]);
    const M: Expr = class(&[Cat::M as u8]);
    const MPST: Expr = class(&[Cat::MPst as u8]);
    const CM: Expr = class(&[Cat::CM as u8]);
    pub(super) const A: Expr = class(&[Cat::A as u8]);
    pub(super) const SM: Expr = class(&[Cat::SM as u8, Cat::SMPst as u8]);
    pub(super) const SMPST: Expr = class(&[Cat::SMPst as u8]);
    const REPHA: Expr = class(&[Cat::Repha as u8]);
    const CS: Expr = class(&[Cat::CS as u8]);
    const PLACEHOLDER: Expr = class(&[Cat::Placeholder as u8]);
    const DOTTED_CIRCLE: Expr = class(&[Cat::DottedCircle as u8]);
    const SYMBOL: Expr = class(&[Cat::Symbol as u8]);
    const JOINER: Expr = class(&[Cat::Zwj as u8, Cat::Zwnj as u8]);
    const RA: Expr = class(&[Cat::Ra as u8]);

    /// Consonant modifiers: an optional register shifter, then nuktas.
    const MODIFIERS: Expr = Seq(&[Opt(&Seq(&[Opt(&ZWNJ), RS])), Opt(&Seq(&[N, Opt(&N)]))]);
    const REPH: Expr = Alt(&[Seq(&[RA, H]), REPHA]);
    const CONSONANT: Expr = Seq(&[C, Opt(&ZWJ), Opt(&MODIFIERS)]);
    const SYMBOL_GROUP: Expr = Seq(&[SYMBOL, Opt(&N)]);
    const MATRA_GROUP: Expr = Seq(&[
        Star(&JOINER),
        Alt(&[M, Seq(&[Opt(&SM), MPST])]),
        Opt(&N),
        Opt(&H),
    ]);
    const SYLLABLE_TAIL: Expr = Seq(&[
        Opt(&Seq(&[Opt(&JOINER), SM, Opt(&SM), Opt(&ZWNJ)])),
        Star(&A),
    ]);
    const HALANT_GROUP: Expr = Seq(&[Opt(&JOINER), H, Opt(&Seq(&[ZWJ, Opt(&N)]))]);
    const FINAL_HALANT_GROUP: Expr = Alt(&[HALANT_GROUP, Seq(&[H, ZWNJ])]);
    const COMPLEX_TAIL: Expr = Seq(&[
        Star(&Seq(&[HALANT_GROUP, CONSONANT])),
        Opt(&CM),
        Alt(&[FINAL_HALANT_GROUP, Star(&MATRA_GROUP)]),
        SYLLABLE_TAIL,
    ]);

    pub(super) const CONSONANT_SYLLABLE: Expr =
        Seq(&[Opt(&Alt(&[REPHA, CS])), CONSONANT, COMPLEX_TAIL]);
    pub(super) const VOWEL_SYLLABLE: Expr =
        Seq(&[Opt(&REPH), V, Opt(&MODIFIERS), Alt(&[ZWJ, COMPLEX_TAIL])]);
    pub(super) const STANDALONE_CLUSTER: Expr = Seq(&[
        Alt(&[
            Seq(&[Opt(&Alt(&[REPHA, CS])), PLACEHOLDER]),
            Seq(&[Opt(&REPH), DOTTED_CIRCLE]),
        ]),
        Opt(&MODIFIERS),
        COMPLEX_TAIL,
    ]);
    pub(super) const SYMBOL_CLUSTER: Expr = Seq(&[SYMBOL_GROUP, SYLLABLE_TAIL]);
    pub(super) const BROKEN_CLUSTER: Expr = Seq(&[Opt(&REPH), Opt(&MODIFIERS), COMPLEX_TAIL]);
}

/// The scanner: its rules and the syllable type each finds.
static MACHINE: Machine = Machine::new(&[
    (grammar::CONSONANT_SYLLABLE, Syllable::Consonant as u8),
    (grammar::VOWEL_SYLLABLE, Syllable::Vowel as u8),
    (grammar::STANDALONE_CLUSTER, Syllable::Standalone as u8),
    (grammar::SYMBOL_CLUSTER, Syllable::Symbol as u8),
    (grammar::SMPST, Syllable::NonIndic as u8),
    (grammar::BROKEN_CLUSTER, Syllable::Broken as u8),
    (ANY, Syllable::NonIndic as u8),
]);

fn category(info: &Info) -> Cat {
    match info.shaper {
        ShaperProps::Indic(category, _) => category,
        _ => Cat::X,
    }
}

fn position(info: &Info) -> Pos {
    match info.shaper {
        ShaperProps::Indic(_, position) => position,
        _ => Pos::Start,
    }
}

fn set_position(info: &mut Info, position: Pos) {
    info.shaper = ShaperProps::Indic(category(info), position);
}

/// Whether the glyph is of one of `categories` and has not ligated (after
/// which its category says nothing).
fn is_one_of(info: &Info, categories: &[Cat]) -> bool {
    !info.ligated() && categories.contains(&category(info))
}

/// Consonants, with vowels and placeholders, which take a syllable's base the
/// same way.
fn is_consonant(info: &Info) -> bool {
    is_one_of(
        info,
        &[
            Cat::C,
            Cat::CS,
            Cat::Ra,
            Cat::CM,
            Cat::V,
            Cat::Placeholder,
            Cat::DottedCircle,
        ],
    )
}

fn is_joiner(info: &Info) -> bool {
    is_one_of(info, &[Cat::Zwj, Cat::Zwnj])
}

fn is_halant(info: &Info) -> bool {
    is_one_of(info, &[Cat::H])
}

fn ligated_and_didnt_multiply(info: &Info) -> bool {
    info.ligated() && !info.multiplied()
}

/// Gives every character its Indic category and position.
pub(super) fn setup_masks(buffer: &mut Buffer) {
    for info in buffer.infos_mut() {
        let (category, position) = unicode::indic_properties(info.glyph);
        info.shaper = ShaperProps::Indic(category, position);
    }
}

/// What the reorderings need from the plan and the face.
struct IndicPlan<'a, 'f> {
    face: &'a Face<'f>,
    script: Script,
    config: Config,
    /// Whether the font uses the old specification (`deva` rather than `dev2`).
    old_spec: bool,
    masks: Masks,
    /// Whether the forms below need no context in the font to substitute.
    zero_context: bool,
    rphf: &'a [LookupMap],
    pref: &'a [LookupMap],
    blwf: &'a [LookupMap],
    pstf: &'a [LookupMap],
    vatu: &'a [LookupMap],
}

impl<'a, 'f> IndicPlan<'a, 'f> {
    fn new(plan: &'a Plan, face: &'a Face<'f>) -> Self {
        let script = plan.script;
        let config = Config::of(script);
        let old_spec = config.has_old_spec
            && plan.chosen_script[0].is_none_or(|tag| tag.to_u32().to_be_bytes()[3] != b'2');
        Self {
            face,
            script,
            config,
            old_spec,
            masks: Masks::of(plan),
            zero_context: !old_spec && &script.to_bytes() != b"Mlym",
            rphf: plan.gsub_stage_lookups(b"rphf"),
            pref: plan.gsub_stage_lookups(b"pref"),
            blwf: plan.gsub_stage_lookups(b"blwf"),
            pstf: plan.gsub_stage_lookups(b"pstf"),
            vatu: plan.gsub_stage_lookups(b"vatu"),
        }
    }

    fn is(&self, script: &[u8; 4]) -> bool {
        &self.script.to_bytes() == script
    }

    /// Whether one of `lookups` would substitute `glyphs`, charged to `work`.
    fn would_substitute(
        &self,
        lookups: &[LookupMap],
        glyphs: &[u32],
        work: &mut WorkBudget,
    ) -> Result<bool> {
        for lookup in lookups {
            let gsub = &self.face.gsub;
            if would_substitute(
                gsub,
                lookup.index,
                &lookup.digest,
                glyphs,
                self.zero_context,
                work,
            )? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Where a consonant's form goes, by the forms the font has for it with a
    /// virama on either side (fonts copied from the old specification put the
    /// virama after).
    fn consonant_position(
        &self,
        consonant: u32,
        virama: u32,
        work: &mut WorkBudget,
    ) -> Result<Pos> {
        let glyphs = [virama, consonant, virama];
        let mut either = |lookups| -> Result<bool> {
            Ok(self.would_substitute(lookups, &glyphs[..2], work)?
                || self.would_substitute(lookups, &glyphs[1..], work)?)
        };
        Ok(if either(self.blwf)? || either(self.vatu)? {
            Pos::BelowC
        } else if either(self.pstf)? || either(self.pref)? {
            Pos::PostC
        } else {
            Pos::BaseC
        })
    }
}

pub(super) fn pause(
    step: Step,
    plan: &Plan,
    face: &Face<'_>,
    buffer: &mut Buffer,
    font: &Scaled<'_>,
) -> Result<bool> {
    let indic = IndicPlan::new(plan, face);
    match step {
        Step::SetupSyllables => {
            syllabic::find_syllables(&MACHINE, buffer, |_, _| false)?;
        }
        Step::InitialReordering => {
            let virama = virama_glyph(&indic, font);
            if virama != 0 {
                let (infos, work) = buffer.infos_and_work();
                for info in infos {
                    if position(info) == Pos::BaseC {
                        let position = indic.consonant_position(info.glyph, virama, work)?;
                        set_position(info, position);
                    }
                }
            }
            let circle = DottedCircle {
                broken: Syllable::Broken as u8,
                shaper: ShaperProps::Indic(Cat::DottedCircle, Pos::End),
                after: Some(Cat::Repha as u8),
            };
            syllabic::insert_dotted_circles(buffer, font, circle)?;
            for_each_syllable(buffer, |buffer, range| {
                let kind = syllable_type(&buffer.infos()[range.start]);
                if kind == Syllable::Symbol as u8 || kind == Syllable::NonIndic as u8 {
                    return Ok(());
                }
                initial_reordering(&indic, buffer, range)
            })?;
        }
        Step::FinalReordering => {
            let virama = virama_glyph(&indic, font);
            for_each_syllable(buffer, |buffer, range| {
                final_reordering(&indic, buffer, range, virama)
            })?;
        }
    }
    Ok(false)
}

/// The glyph of the script's virama, zero if it has none or the font lacks it.
fn virama_glyph(indic: &IndicPlan<'_, '_>, font: &Scaled<'_>) -> u32 {
    if indic.config.virama == 0 {
        return 0;
    }
    font.glyph(indic.config.virama).unwrap_or(0)
}

/// Puts a syllable in its visual order before the basic forms apply, and gives
/// its glyphs the masks of the forms they may take. Vowel, standalone and
/// broken syllables (with their dotted circle) take a base as consonant
/// syllables do.
fn initial_reordering(
    indic: &IndicPlan<'_, '_>,
    buffer: &mut Buffer,
    range: Range<usize>,
) -> Result<()> {
    let Range { start, end } = range;
    // Kannada Ra, halant, ZWJ acts as Ra, ZWJ, halant.
    if indic.is(b"Knda") && start + 3 <= end {
        let infos = buffer.infos();
        if is_one_of(&infos[start], &[Cat::Ra])
            && is_one_of(&infos[start + 1], &[Cat::H])
            && is_one_of(&infos[start + 2], &[Cat::Zwj])
        {
            buffer.merge_clusters(start + 1, start + 3)?;
            buffer.infos_mut().swap(start + 1, start + 2);
        }
    }
    let (infos, work) = buffer.infos_and_work();
    let (mut base, has_reph) = find_base(indic, infos, start, end, work)?;

    let infos = buffer.infos_mut();
    for info in &mut infos[start..base] {
        set_position(info, position(info).min(Pos::PreC));
    }
    if base < end {
        set_position(&mut infos[base], Pos::BaseC);
    }
    if has_reph {
        set_position(&mut infos[start], Pos::RaToBecomeReph);
    }
    if indic.old_spec {
        move_first_post_base_halant(indic, &mut infos[base.min(end)..end]);
    }
    attach_marks(&mut infos[start..end]);
    // Post-base consonants own what comes before them since the last consonant
    // or matra.
    let mut last = base;
    for i in base + 1..end {
        if is_consonant(&infos[i]) {
            let owner = position(&infos[i]);
            for info in &mut infos[last + 1..i] {
                if position(info) < Pos::Smvd {
                    set_position(info, owner);
                }
            }
            last = i;
        } else if matches!(category(&infos[i]), Cat::M | Cat::MPst) {
            last = i;
        }
    }

    base = sort_syllable(indic, buffer, start, end)?;
    let (infos, work) = buffer.infos_and_work();
    set_masks(indic, infos, start..end, base, work)
}

/// The base consonant of a syllable and whether it starts with a reph.
///
/// From the end, the base is the first consonant without a below-base or
/// post-base form (post-base forms only follow below-base ones), or the first
/// consonant. A Ra and halant at the start (with ZWJ for explicit reph) that
/// the font forms reph from is not a candidate, unless it is all there is.
fn find_base(
    indic: &IndicPlan<'_, '_>,
    infos: &[Info],
    start: usize,
    end: usize,
    work: &mut WorkBudget,
) -> Result<(usize, bool)> {
    let mut base = end;
    let mut has_reph = false;
    let mut limit = start;
    let mode = indic.config.reph_mode;
    let rphf = indic.masks.rphf;
    if rphf != 0
        && start + 3 <= end
        && ((mode == RephMode::Implicit && !is_joiner(&infos[start + 2]))
            || (mode == RephMode::Explicit && category(&infos[start + 2]) == Cat::Zwj))
    {
        let glyphs = [
            infos[start].glyph,
            infos[start + 1].glyph,
            if mode == RephMode::Explicit {
                infos[start + 2].glyph
            } else {
                0
            },
        ];
        if indic.would_substitute(indic.rphf, &glyphs[..2], work)?
            || (mode == RephMode::Explicit && indic.would_substitute(indic.rphf, &glyphs, work)?)
        {
            limit += 2;
            while limit < end && is_joiner(&infos[limit]) {
                limit += 1;
            }
            base = start;
            has_reph = true;
        }
    } else if mode == RephMode::Logical && category(&infos[start]) == Cat::Repha {
        limit += 1;
        while limit < end && is_joiner(&infos[limit]) {
            limit += 1;
        }
        base = start;
        has_reph = true;
    }

    let mut i = end;
    let mut seen_below = false;
    loop {
        i -= 1;
        let info = &infos[i];
        if is_consonant(info) {
            let position = position(info);
            if position != Pos::BelowC && (position != Pos::PostC || seen_below) {
                base = i;
                break;
            }
            seen_below |= position == Pos::BelowC;
            base = i;
        } else if start < i && category(info) == Cat::Zwj && category(&infos[i - 1]) == Cat::H {
            // A ZWJ after a halant asks for a half form and ends the search.
            break;
        }
        if i <= limit {
            break;
        }
    }
    // Reph needs another consonant.
    if has_reph && base == start && limit - base <= 2 {
        has_reph = false;
    }
    Ok((base, has_reph))
}

/// With the old specification, the first halant after the base moves after
/// the last consonant (Kannada keeps it before a halant already there).
fn move_first_post_base_halant(indic: &IndicPlan<'_, '_>, after_base: &mut [Info]) {
    let disallow_double_halants = indic.is(b"Knda");
    let Some(i) = after_base
        .iter()
        .skip(1)
        .position(|info| category(info) == Cat::H)
        .map(|i| i + 1)
    else {
        return;
    };
    let mut j = after_base.len() - 1;
    while j > i {
        let info = &after_base[j];
        if is_consonant(info) || (disallow_double_halants && category(info) == Cat::H) {
            break;
        }
        j -= 1;
    }
    if category(&after_base[j]) != Cat::H && j > i {
        after_base[i..=j].rotate_left(1);
    }
}

/// Joiners, nuktas, register shifters, medial consonants and halants move
/// with the character before them; a halant after a left matra stays.
fn attach_marks(syllable: &mut [Info]) {
    let mut last = Pos::Start;
    for i in 0..syllable.len() {
        let cat = category(&syllable[i]);
        if matches!(
            cat,
            Cat::Zwj | Cat::Zwnj | Cat::N | Cat::RS | Cat::CM | Cat::H
        ) {
            set_position(&mut syllable[i], last);
            if cat == Cat::H
                && last == Pos::PreM
                && let Some(before) = syllable[..i]
                    .iter()
                    .rev()
                    .map(position)
                    .find(|&p| p != Pos::PreM)
            {
                set_position(&mut syllable[i], before);
            }
        } else if position(&syllable[i]) != Pos::Smvd {
            if cat == Cat::MPst && i > 0 && category(&syllable[i - 1]) == Cat::SM {
                let after = position(&syllable[i]);
                set_position(&mut syllable[i - 1], after);
            }
            last = position(&syllable[i]);
        }
    }
}

/// Sorts the syllable by position, keeping left matras in their order, and
/// merges the clusters of what moved after the base; returns the new base.
fn sort_syllable(
    indic: &IndicPlan<'_, '_>,
    buffer: &mut Buffer,
    start: usize,
    end: usize,
) -> Result<usize> {
    let infos = buffer.infos_mut();
    // The syllable field remembers each glyph's place while sorting; syllables
    // are at most 64 glyphs, so places fit and 255 is free.
    let syllable = infos[start].syllable;
    for (place, info) in infos[start..end].iter_mut().enumerate() {
        info.syllable = place as u8;
    }
    infos[start..end].sort_by_key(position);

    let mut base = end;
    let mut left_matras: Option<(usize, usize)> = None;
    for (i, info) in infos.iter().enumerate().take(end).skip(start) {
        match position(info) {
            Pos::BaseC => {
                base = i;
                break;
            }
            Pos::PreM => {
                left_matras = Some((left_matras.map_or(i, |(first, _)| first), i));
            }
            _ => {}
        }
    }
    if let Some((first, last)) = left_matras.filter(|(first, last)| first < last) {
        // Several left matras keep their logical order, each with the nuktas
        // and the like after it.
        buffer.reverse_range(first, last + 1);
        let mut i = first;
        for j in first..=last {
            if matches!(category(&buffer.infos()[j]), Cat::M | Cat::MPst) {
                buffer.reverse_range(i, j + 1);
                i = j + 1;
            }
        }
    }

    if indic.old_spec || end - start > 127 {
        buffer.merge_clusters(base, end)?;
    } else {
        // Merge each cycle of the sort permutation that ends after the base.
        for i in base..end {
            if buffer.infos()[i].syllable == u8::MAX {
                continue;
            }
            let (mut min, mut max) = (i, i);
            let mut j = start + usize::from(buffer.infos()[i].syllable);
            while j != i && (start..end).contains(&j) {
                min = min.min(j);
                max = max.max(j);
                let infos = buffer.infos_mut();
                let next = start + usize::from(infos[j].syllable);
                infos[j].syllable = u8::MAX;
                j = next;
            }
            buffer.merge_clusters(base.max(min), max + 1)?;
        }
    }
    for info in &mut buffer.infos_mut()[start..end] {
        info.syllable = syllable;
    }
    Ok(base)
}

/// Gives the glyphs of a sorted syllable the masks of the forms they may take.
fn set_masks(
    indic: &IndicPlan<'_, '_>,
    infos: &mut [Info],
    Range { start, end }: Range<usize>,
    base: usize,
    work: &mut WorkBudget,
) -> Result<()> {
    for info in infos[start..end]
        .iter_mut()
        .take_while(|info| position(info) == Pos::RaToBecomeReph)
    {
        info.mask |= indic.masks.rphf;
    }
    let mut pre_base = indic.masks.half;
    if !indic.old_spec && indic.config.below_forms_before_base {
        pre_base |= indic.masks.blwf;
    }
    for info in &mut infos[start..base] {
        info.mask |= pre_base;
    }
    let post_base = indic.masks.blwf | indic.masks.abvf | indic.masks.pstf;
    for info in infos.iter_mut().take(end).skip(base + 1) {
        info.mask |= post_base;
    }

    if indic.old_spec && indic.is(b"Deva") {
        // The old specification forms eyelash Ra below half forms too, unless
        // a ZWJ asks for it explicitly.
        let blwf = indic.masks.blwf;
        for i in start..base.saturating_sub(1) {
            if category(&infos[i]) == Cat::Ra
                && category(&infos[i + 1]) == Cat::H
                && (i + 2 == base || category(&infos[i + 2]) != Cat::Zwj)
            {
                infos[i].mask |= blwf;
                infos[i + 1].mask |= blwf;
            }
        }
    }

    let pref = indic.masks.pref;
    if pref != 0 && base + 2 < end {
        // A halant and Ra after the base that form a pre-base form.
        for i in base + 1..end - 1 {
            let pair = [infos[i].glyph, infos[i + 1].glyph];
            if indic.would_substitute(indic.pref, &pair, work)? {
                infos[i].mask |= pref;
                infos[i + 1].mask |= pref;
                break;
            }
        }
    }

    // A ZWNJ prevents half forms back to the consonant before it.
    let half = indic.masks.half;
    for i in start + 1..end {
        if !is_joiner(&infos[i]) {
            continue;
        }
        let non_joiner = category(&infos[i]) == Cat::Zwnj;
        let mut j = i;
        loop {
            j -= 1;
            if non_joiner {
                infos[j].mask &= !half;
            }
            if j <= start || is_consonant(&infos[j]) {
                break;
            }
        }
    }
    Ok(())
}

/// After the basic forms: finds the base again, moves left matras, reph and
/// pre-base forms to their places by what the font formed, and marks a word
/// initial left matra for `init`.
fn final_reordering(
    indic: &IndicPlan<'_, '_>,
    buffer: &mut Buffer,
    range: Range<usize>,
    virama: u32,
) -> Result<()> {
    let Range { start, end } = range;
    if virama != 0 {
        // A virama that came out of a ligature and a multiple substitution is
        // still a halant.
        for info in &mut buffer.infos_mut()[start..end] {
            if info.glyph == virama && info.ligated() && info.multiplied() {
                info.shaper = ShaperProps::Indic(Cat::H, position(info));
                info.glyph_props = info.glyph_props.without_ligation();
            }
        }
    }
    let pref = indic.masks.pref;
    let (mut base, try_pref) = final_base(indic, buffer.infos_mut(), start, end);

    if start + 1 < end && start < base {
        base = move_left_matras(indic, buffer, start, base, end)?;
    }
    let infos = buffer.infos();
    if start + 1 < end
        && position(&infos[start]) == Pos::RaToBecomeReph
        && ((category(&infos[start]) == Cat::Repha) ^ ligated_and_didnt_multiply(&infos[start]))
    {
        let target = reph_target(indic, infos, start, base, end);
        buffer.merge_clusters(start, target + 1)?;
        buffer.infos_mut()[start..=target].rotate_left(1);
        if start < base && base <= target {
            base -= 1;
        }
    }

    if try_pref && base + 1 < end {
        let infos = buffer.infos();
        if let Some(i) = (base + 1..end).find(|&i| infos[i].mask & pref != 0) {
            // Only a pre-base form the font made moves.
            if ligated_and_didnt_multiply(&infos[i]) {
                let mut target = base;
                if !indic.is(b"Mlym") && !indic.is(b"Taml") {
                    while target > start
                        && !is_one_of(&infos[target - 1], &[Cat::M, Cat::MPst, Cat::H])
                    {
                        target -= 1;
                    }
                }
                if target > start
                    && is_halant(&infos[target - 1])
                    && target < end
                    && is_joiner(&infos[target])
                {
                    target += 1;
                }
                buffer.merge_clusters(target, i + 1)?;
                buffer.infos_mut()[target..=i].rotate_right(1);
            }
        }
    }

    let infos = buffer.infos_mut();
    if position(&infos[start]) == Pos::PreM
        && (start == 0 || !(1..=12).contains(&(infos[start - 1].category() as u8)))
    {
        // A left matra at the start of a word: not after a format character,
        // letter or mark (the general categories from Cf to Mn).
        infos[start].mask |= indic.masks.init;
    }
    Ok(())
}

/// The base of a syllable after the basic forms, and whether a pre-base form
/// may still need moving.
fn final_base(
    indic: &IndicPlan<'_, '_>,
    infos: &mut [Info],
    start: usize,
    end: usize,
) -> (usize, bool) {
    let pref = indic.masks.pref;
    let mut try_pref = pref != 0;
    let mut base = start;
    while base < end {
        if position(&infos[base]) < Pos::BaseC {
            base += 1;
            continue;
        }
        if try_pref && base + 1 < end {
            if let Some(i) = (base + 1..end).find(|&i| infos[i].mask & pref != 0) {
                let info = &infos[i];
                if !(info.substituted() && ligated_and_didnt_multiply(info)) {
                    // A pre-base candidate that formed nothing: the base is
                    // around here.
                    base = i;
                    while base < end && is_halant(&infos[base]) {
                        base += 1;
                    }
                    if base < end {
                        set_position(&mut infos[base], Pos::BaseC);
                    }
                    try_pref = false;
                }
            }
            if base == end {
                break;
            }
        }
        if indic.is(b"Mlym") {
            // Malayalam skips over below-base forms it did not form.
            let mut i = base + 1;
            while i < end {
                while i < end && is_joiner(&infos[i]) {
                    i += 1;
                }
                if i == end || !is_halant(&infos[i]) {
                    break;
                }
                i += 1;
                while i < end && is_joiner(&infos[i]) {
                    i += 1;
                }
                if i < end && is_consonant(&infos[i]) && position(&infos[i]) == Pos::BelowC {
                    base = i;
                    set_position(&mut infos[base], Pos::BaseC);
                }
                i += 1;
            }
        }
        if start < base && position(&infos[base]) > Pos::BaseC {
            base -= 1;
        }
        break;
    }
    if base == end && start < base && is_one_of(&infos[base - 1], &[Cat::Zwj]) {
        base -= 1;
    }
    if base < end {
        while start < base && is_one_of(&infos[base], &[Cat::N, Cat::H]) {
            base -= 1;
        }
    }
    (base, try_pref)
}

/// Moves pre-base matras from the start of the syllable to after the last
/// standalone halant before the base (Malayalam and Tamil, whose half forms
/// are chillus or explicit viramas, to right before the base), merging their
/// clusters up to the base; returns the base.
fn move_left_matras(
    indic: &IndicPlan<'_, '_>,
    buffer: &mut Buffer,
    start: usize,
    mut base: usize,
    end: usize,
) -> Result<usize> {
    let infos = buffer.infos();
    // Without a base, before the last glyph.
    let mut target = if base == end { base - 2 } else { base - 1 };
    if !indic.is(b"Mlym") && !indic.is(b"Taml") {
        loop {
            while target > start && !is_one_of(&infos[target], &[Cat::M, Cat::MPst, Cat::H]) {
                target -= 1;
            }
            // A halant that is not the matra's own; a ZWJ after it keeps the
            // matra searching further left.
            if is_halant(&infos[target]) && position(&infos[target]) != Pos::PreM {
                if target + 1 < end && category(&infos[target + 1]) == Cat::Zwj && target > start {
                    target -= 1;
                    continue;
                }
            } else {
                target = start;
            }
            break;
        }
    }
    if start < target && position(&infos[target]) != Pos::PreM {
        let mut i = target;
        while i > start {
            if position(&buffer.infos()[i - 1]) == Pos::PreM {
                let old = i - 1;
                if old < base && base <= target {
                    base -= 1;
                }
                buffer.infos_mut()[old..=target].rotate_left(1);
                buffer.merge_clusters(target, end.min(base + 1))?;
                target -= 1;
            }
            i -= 1;
        }
    } else if let Some(i) = (start..base).find(|&i| position(&infos[i]) == Pos::PreM) {
        buffer.merge_clusters(i, end.min(base + 1))?;
    }
    Ok(base)
}

/// Where reph goes, by the script's reph position and what formed.
fn reph_target(
    indic: &IndicPlan<'_, '_>,
    infos: &[Info],
    start: usize,
    base: usize,
    end: usize,
) -> usize {
    // After the first explicit halant between reph and the base (or the ZWJ or
    // ZWNJ after it).
    let after_explicit_halant = || {
        let halant = (start + 1..base).find(|&i| is_halant(&infos[i]))?;
        Some(if halant + 1 < base && is_joiner(&infos[halant + 1]) {
            halant + 1
        } else {
            halant
        })
    };
    let position_after = |stop: &dyn Fn(Pos) -> bool| {
        let mut target = base;
        while target + 1 < end && !stop(position(&infos[target + 1])) {
            target += 1;
        }
        (target < end).then_some(target)
    };
    let reph = indic.config.reph_position;
    let found = if reph == Pos::AfterPost {
        None
    } else {
        after_explicit_halant()
            .or_else(|| {
                (reph == Pos::AfterMain)
                    .then(|| position_after(&|p| p > Pos::AfterMain))
                    .flatten()
            })
            .or_else(|| {
                (reph == Pos::AfterSub)
                    .then(|| {
                        position_after(&|p| matches!(p, Pos::PostC | Pos::AfterPost | Pos::Smvd))
                    })
                    .flatten()
            })
    };
    found.or_else(after_explicit_halant).unwrap_or_else(|| {
        // The end of the syllable, before syllable modifiers; before a final
        // halant that follows a matra, so that it can interact with it.
        let mut target = end - 1;
        while target > start && position(&infos[target]) == Pos::Smvd {
            target -= 1;
        }
        if is_halant(&infos[target]) {
            let mut i = base + 1;
            while i < target {
                if matches!(category(&infos[i]), Cat::M | Cat::MPst) {
                    target -= 1;
                }
                i += 1;
            }
        }
        target
    })
}

/// Indic decomposition: Unicode's, except for letters fonts expect whole.
pub(super) fn decompose(ab: u32) -> Option<(u32, u32)> {
    match ab {
        // Devanagari RRA, Bengali RRA and RHA, Tamil AU.
        0x0931 | 0x09DC | 0x09DD | 0x0B94 => None,
        _ => unicode::decompose(ab),
    }
}

/// Indic composition: Unicode's, but never of a mark with what follows (split
/// matras stay split), and Bengali YYA despite its composition exclusion.
pub(super) fn compose(a: u32, b: u32) -> Option<u32> {
    if unicode::general_category(a).is_mark() {
        return None;
    }
    if (a, b) == (0x09AF, 0x09BC) {
        return Some(0x09DF);
    }
    unicode::compose(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &[Cat]) -> Vec<(usize, Syllable)> {
        const ALL: [Syllable; 6] = [
            Syllable::Consonant,
            Syllable::Vowel,
            Syllable::Standalone,
            Syllable::Symbol,
            Syllable::Broken,
            Syllable::NonIndic,
        ];
        let mut out = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let (found, _) = MACHINE.longest(text[start..].iter().map(|&c| c as u8));
            let (len, rule) = found.unwrap();
            out.push((len, ALL[usize::from(rule)]));
            start += len;
        }
        out
    }

    #[test]
    fn syllables_follow_the_grammar() {
        use Cat::*;
        // Ka, virama, Ssa, vowel sign I: one consonant syllable.
        assert_eq!(kinds(&[C, H, C, M]), [(4, Syllable::Consonant)]);
        // Ra, virama, Ka, anusvara.
        assert_eq!(kinds(&[Ra, H, C, SM]), [(4, Syllable::Consonant)]);
        // A matra alone is broken; a ZWNJ after a halant ends the syllable.
        assert_eq!(
            kinds(&[M, C, H, Zwnj, C]),
            [
                (1, Syllable::Broken),
                (3, Syllable::Consonant),
                (1, Syllable::Consonant)
            ]
        );
        // Independent vowel with a nukta, then a symbol with a tail.
        assert_eq!(
            kinds(&[V, N, Symbol, SM]),
            [(2, Syllable::Vowel), (2, Syllable::Symbol)]
        );
        assert_eq!(
            kinds(&[X, SMPst]),
            [(1, Syllable::NonIndic), (1, Syllable::NonIndic)]
        );
        assert_eq!(
            kinds(&[DottedCircle, M, Placeholder]),
            [(2, Syllable::Standalone), (1, Syllable::Standalone)]
        );
    }

    #[test]
    fn configs_match_the_scripts() {
        let telugu = Config::of(Script::from_code(*b"Telu"));
        assert_eq!(telugu.reph_mode, RephMode::Explicit);
        assert!(!telugu.below_forms_before_base);
        assert_eq!(
            Config::of(Script::from_code(*b"Mlym")).reph_mode,
            RephMode::Logical
        );
        assert!(!Config::of(Script::from_code(*b"Latn")).has_old_spec);
    }

    #[test]
    fn decomposition_keeps_whole_letters() {
        assert_eq!(decompose(0x0931), None);
        assert_eq!(decompose(0x0929), Some((0x0928, 0x093C)));
        assert_eq!(compose(0x09AF, 0x09BC), Some(0x09DF));
        assert_eq!(compose(0x093F, 0x0902), None);
    }
}
