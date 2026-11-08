//! The shape plan: which features apply, the mask bits they get, the lookups of
//! each stage in order, and which positioning methods run — HarfBuzz's map
//! builder and shape planner.

use crate::face::Face;
use crate::ot::digest::Digest;
use crate::ot::layout::LayoutTable;
use crate::shapers::Shaper;
use crate::tag::{Direction, Language, Script, Tag};
use triergon_core::{Budget, BudgetVec, Error, Result};

/// A feature the caller sets for the whole item, such as `kern=0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feature {
    pub tag: Tag,
    pub value: u32,
}

/// How a feature is applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FeatureFlags {
    /// Applies to every glyph; with value one it needs no mask bits of its own.
    pub(crate) global: bool,
    /// Has a fallback implementation, so it gets a mask even when the font lacks it.
    pub(crate) has_fallback: bool,
    /// Do not skip ZWNJ when matching context.
    pub(crate) manual_zwnj: bool,
    /// Do not skip ZWJ when matching input.
    pub(crate) manual_zwj: bool,
    /// Look for the feature in the whole feature list if the language system lacks it.
    pub(crate) global_search: bool,
    /// Pick alternates at random.
    pub(crate) random: bool,
    /// Keep matches within a syllable.
    pub(crate) per_syllable: bool,
}

impl FeatureFlags {
    pub(crate) const NONE: Self = Self {
        global: false,
        has_fallback: false,
        manual_zwnj: false,
        manual_zwj: false,
        global_search: false,
        random: false,
        per_syllable: false,
    };
    pub(crate) const GLOBAL: Self = Self {
        global: true,
        ..Self::NONE
    };
    pub(crate) const GLOBAL_MANUAL_JOINERS: Self = Self {
        manual_zwnj: true,
        manual_zwj: true,
        ..Self::GLOBAL
    };
    pub(crate) const GLOBAL_HAS_FALLBACK: Self = Self {
        has_fallback: true,
        ..Self::GLOBAL
    };
}

/// Work done between two lookup stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pause {
    /// A shaper's own step.
    Shaper(u8),
}

#[derive(Clone, Copy, Debug)]
struct FeatureInfo {
    tag: Tag,
    seq: usize,
    max_value: u32,
    flags: FeatureFlags,
    default_value: u32,
    stage: [usize; 2],
}

/// Collects features and stages before the plan is compiled.
#[derive(Debug)]
pub(crate) struct Builder<'a> {
    pub(crate) tables: [LayoutTable<'a>; 2],
    pub(crate) script: [Option<u16>; 2],
    pub(crate) chosen_script: [Option<Tag>; 2],
    language: [Option<u16>; 2],
    features: BudgetVec<FeatureInfo>,
    stage: [usize; 2],
    pauses: [BudgetVec<(usize, Option<Pause>)>; 2],
    /// The first storage error of an addition, reported when the plan compiles.
    error: Option<Error>,
}

impl<'a> Builder<'a> {
    fn new(face: &Face<'a>, script: Script, language: &Language, budget: &Budget) -> Self {
        let tables = [face.gsub, face.gpos];
        let (script_tags, script_count) = script.ot_tags();
        let mut builder = Self {
            tables,
            script: [None; 2],
            chosen_script: [None; 2],
            language: [None; 2],
            features: BudgetVec::new(budget),
            stage: [0; 2],
            pauses: [BudgetVec::new(budget), BudgetVec::new(budget)],
            error: None,
        };
        for (t, table) in tables.iter().enumerate() {
            let (index, chosen) = select_script(table, &script_tags[..script_count]);
            builder.script[t] = index;
            builder.chosen_script[t] = chosen;
            let script = table.script(index);
            builder.language[t] = language
                .ot_tags()
                .iter()
                .find_map(|&tag| script.find_lang_sys(tag))
                .or_else(|| script.find_lang_sys(Tag::new(b"dflt")));
        }
        builder
    }

    pub(crate) fn add_feature(&mut self, tag: &[u8; 4], flags: FeatureFlags, value: u32) {
        self.add(Tag::new(tag), flags, value);
    }

    pub(crate) fn enable_feature(&mut self, tag: &[u8; 4]) {
        self.add(Tag::new(tag), FeatureFlags::GLOBAL, 1);
    }

    pub(crate) fn disable_feature(&mut self, tag: &[u8; 4]) {
        self.add(Tag::new(tag), FeatureFlags::GLOBAL, 0);
    }

    fn add(&mut self, tag: Tag, flags: FeatureFlags, value: u32) {
        if tag.0 == 0 {
            return;
        }
        let info = FeatureInfo {
            tag,
            seq: self.features.len() + 1,
            max_value: value,
            flags,
            default_value: if flags.global { value } else { 0 },
            stage: self.stage,
        };
        if let Err(e) = self.features.try_push(info) {
            self.error.get_or_insert(e.error);
        }
    }

    pub(crate) fn add_gsub_pause(&mut self, pause: Option<Pause>) {
        self.add_pause(0, pause);
    }

    pub(crate) fn add_gpos_pause(&mut self, pause: Option<Pause>) {
        self.add_pause(1, pause);
    }

    fn add_pause(&mut self, table: usize, pause: Option<Pause>) {
        if let Err(e) = self.pauses[table].try_push((self.stage[table], pause)) {
            self.error.get_or_insert(e.error);
        }
        self.stage[table] += 1;
    }

    /// Whether the selected language systems list `tag`.
    pub(crate) fn has_feature(&self, tag: &[u8; 4]) -> bool {
        let tag = Tag::new(tag);
        (0..2).any(|t| {
            let table = &self.tables[t];
            table
                .script(self.script[t])
                .lang_sys(self.language[t])
                .feature_indices()
                .any(|i| table.feature_tag(i) == tag)
        })
    }
}

/// HarfBuzz's script selection: the first of `tags` the table has, else `DFLT`,
/// `dflt` or `latn`, with the tag chosen.
fn select_script(table: &LayoutTable<'_>, tags: &[Tag]) -> (Option<u16>, Option<Tag>) {
    let fallbacks = [Tag::new(b"DFLT"), Tag::new(b"dflt"), Tag::new(b"latn")];
    tags.iter()
        .chain(&fallbacks)
        .find_map(|&tag| table.find_script(tag).map(|i| (Some(i), Some(tag))))
        .unwrap_or((None, None))
}

/// One feature with its mask.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FeatureMap {
    pub(crate) tag: Tag,
    index: [Option<u16>; 2],
    stage: [usize; 2],
    pub(crate) shift: u32,
    pub(crate) mask: u32,
    one_mask: u32,
    auto_zwnj: bool,
    auto_zwj: bool,
    random: bool,
    per_syllable: bool,
    /// Not found in the font: a fallback implements it.
    needs_fallback: bool,
}

/// One lookup to apply, with the mask of the features that want it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LookupMap {
    pub(crate) index: u16,
    pub(crate) mask: u32,
    pub(crate) auto_zwnj: bool,
    pub(crate) auto_zwj: bool,
    pub(crate) random: bool,
    pub(crate) per_syllable: bool,
    /// The glyphs the lookup can apply at.
    pub(crate) digest: Digest,
}

/// The lookups up to `last_lookup`, then an optional pause.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StageMap {
    pub(crate) last_lookup: usize,
    pub(crate) pause: Option<Pause>,
}

/// Everything decided before shaping a run of one face, direction, script,
/// language and feature set. Build it once and reuse it for every item.
#[derive(Debug)]
pub struct Plan {
    pub(crate) direction: Direction,
    pub(crate) script: Script,
    pub(crate) shaper: Shaper,
    pub(crate) global_mask: u32,
    pub(crate) features: BudgetVec<FeatureMap>,
    pub(crate) lookups: [BudgetVec<LookupMap>; 2],
    pub(crate) stages: [BudgetVec<StageMap>; 2],
    pub(crate) chosen_script: [Option<Tag>; 2],
    pub(crate) fallback_glyph_classes: bool,
    pub(crate) apply_gpos: bool,
    pub(crate) apply_kern: bool,
    pub(crate) apply_fallback_kern: bool,
    pub(crate) zero_marks: bool,
    pub(crate) has_gpos_mark: bool,
    pub(crate) adjust_mark_positioning_when_zeroing: bool,
    pub(crate) fallback_mark_positioning: bool,
    pub(crate) kern_mask: u32,
    pub(crate) rtlm_mask: u32,
    pub(crate) frac_mask: u32,
    pub(crate) numr_mask: u32,
    pub(crate) dnom_mask: u32,
    pub(crate) has_frac: bool,
}

const GLOBAL_BIT_SHIFT: u32 = 31;
const MAX_BITS: u32 = 8;

impl Plan {
    /// Plans shaping `face` in `direction` for `script` and `language` with the
    /// global `features` (later ones win), as libass asks HarfBuzz to.
    ///
    /// # Errors
    ///
    /// `Unsupported` for a face with Apple Advanced Typography tables HarfBuzz
    /// would use; the budget's errors.
    pub fn new(
        face: &Face<'_>,
        direction: Direction,
        script: Script,
        language: &Language,
        features: &[Feature],
        budget: &Budget,
    ) -> Result<Self> {
        if face.has_aat_layout() {
            return Err(Error::Unsupported("AAT layout (morx, kerx or trak)"));
        }
        let mut builder = Builder::new(face, script, language, budget);
        let shaper = Shaper::categorize(script, direction, builder.chosen_script[0]);
        builder.enable_feature(b"rvrn");
        builder.add_gsub_pause(None);
        match direction {
            Direction::LeftToRight => {
                builder.enable_feature(b"ltra");
                builder.enable_feature(b"ltrm");
            }
            Direction::RightToLeft => {
                builder.enable_feature(b"rtla");
                builder.add_feature(b"rtlm", FeatureFlags::NONE, 1);
            }
        }
        for tag in [b"frac", b"numr", b"dnom"] {
            builder.add_feature(tag, FeatureFlags::NONE, 1);
        }
        builder.add_feature(
            b"rand",
            FeatureFlags {
                random: true,
                ..FeatureFlags::GLOBAL
            },
            crate::ot::gsub::MAX_VALUE,
        );
        builder.enable_feature(b"Harf");
        builder.enable_feature(b"HARF");
        shaper.collect_features(&mut builder, script);
        builder.enable_feature(b"Buzz");
        builder.enable_feature(b"BUZZ");
        for (tag, flags) in [
            (b"abvm", FeatureFlags::GLOBAL),
            (b"blwm", FeatureFlags::GLOBAL),
            (b"ccmp", FeatureFlags::GLOBAL),
            (b"locl", FeatureFlags::GLOBAL),
            (b"mark", FeatureFlags::GLOBAL_MANUAL_JOINERS),
            (b"mkmk", FeatureFlags::GLOBAL_MANUAL_JOINERS),
            (b"rlig", FeatureFlags::GLOBAL),
            (b"calt", FeatureFlags::GLOBAL),
            (b"clig", FeatureFlags::GLOBAL),
            (b"curs", FeatureFlags::GLOBAL),
            (b"dist", FeatureFlags::GLOBAL),
            (b"kern", FeatureFlags::GLOBAL_HAS_FALLBACK),
            (b"liga", FeatureFlags::GLOBAL),
            (b"rclt", FeatureFlags::GLOBAL),
        ] {
            builder.add_feature(tag, flags, 1);
        }
        for feature in features {
            builder.add(feature.tag, FeatureFlags::GLOBAL, feature.value);
        }
        shaper.override_features(&mut builder);
        let mut plan = compile(builder, budget, direction, script, shaper)?;
        plan.decide(face);
        Ok(plan)
    }

    /// The mask and shift of feature `tag`, zero if it has none.
    pub(crate) fn mask(&self, tag: &[u8; 4]) -> (u32, u32) {
        self.feature(tag).map_or((0, 0), |f| (f.mask, f.shift))
    }

    /// The mask bit for value one of feature `tag`.
    pub(crate) fn one_mask(&self, tag: &[u8; 4]) -> u32 {
        self.feature(tag).map_or(0, |f| f.one_mask)
    }

    fn feature(&self, tag: &[u8; 4]) -> Option<&FeatureMap> {
        let tag = Tag::new(tag);
        let features = self.features.as_slice();
        crate::ot::data::bsearch(features.len(), |i| features[i].tag.cmp(&tag))
            .map(|i| &features[i])
    }

    /// Whether feature `tag` has a mask but no lookups in the font.
    pub(crate) fn needs_fallback(&self, tag: &[u8; 4]) -> bool {
        self.feature(tag).is_some_and(|f| f.needs_fallback)
    }

    /// The GSUB lookups of the stage that applies feature `tag`, with those of
    /// any feature sharing the stage; none if the plan lacks the feature.
    pub(crate) fn gsub_stage_lookups(&self, tag: &[u8; 4]) -> &[LookupMap] {
        let Some(stage) = self.feature(tag).map(|f| f.stage[0]) else {
            return &[];
        };
        let stages = &self.stages[0];
        let lookups = self.lookups[0].as_slice();
        let start = stage
            .checked_sub(1)
            .and_then(|s| stages.get(s))
            .map_or(0, |s| s.last_lookup);
        let end = stages.get(stage).map_or(lookups.len(), |s| s.last_lookup);
        lookups.get(start..end).unwrap_or_default()
    }

    fn feature_index(&self, table: usize, tag: &[u8; 4]) -> Option<u16> {
        self.feature(tag).and_then(|f| f.index[table])
    }

    /// Chooses glyph classes, substitution and positioning as HarfBuzz's planner.
    fn decide(&mut self, face: &Face<'_>) {
        self.frac_mask = self.one_mask(b"frac");
        self.numr_mask = self.one_mask(b"numr");
        self.dnom_mask = self.one_mask(b"dnom");
        self.has_frac = self.frac_mask != 0 || (self.numr_mask != 0 && self.dnom_mask != 0);
        self.rtlm_mask = self.one_mask(b"rtlm");
        self.kern_mask = self.mask(b"kern").0;
        let has_gpos_kern = self.feature_index(1, b"kern").is_some();
        let disable_gpos = self
            .shaper
            .gpos_tag()
            .is_some_and(|tag| Some(tag) != self.chosen_script[1]);
        self.fallback_glyph_classes = !face.gdef.has_glyph_classes();
        let fallback_position = self.shaper.fallback_position();
        let has_gpos = !disable_gpos && face.gpos.has_data();
        self.apply_gpos = has_gpos;
        if !has_gpos_kern || !self.apply_gpos {
            self.apply_kern = face.kern.has_data() && fallback_position;
        }
        self.apply_fallback_kern = fallback_position && !(self.apply_gpos || self.apply_kern);
        self.zero_marks = self.shaper.zero_width_marks().is_some()
            && (!self.apply_kern || !face.kern.has_state_machine());
        self.has_gpos_mark = self.one_mask(b"mark") != 0;
        self.adjust_mark_positioning_when_zeroing =
            !self.apply_gpos && (!self.apply_kern || !face.kern.has_cross_stream());
        self.fallback_mark_positioning =
            self.adjust_mark_positioning_when_zeroing && fallback_position;
    }
}

/// Merges duplicate features, allocates mask bits and collects each stage's
/// lookups, as `hb_ot_map_builder_t::compile` does.
fn compile(
    mut b: Builder<'_>,
    budget: &Budget,
    direction: Direction,
    script: Script,
    shaper: Shaper,
) -> Result<Plan> {
    if let Some(error) = b.error.take() {
        return Err(error);
    }
    let mut required = [None; 2];
    let mut required_stage = [0; 2];
    for t in 0..2 {
        required[t] = b.tables[t]
            .script(b.script[t])
            .lang_sys(b.language[t])
            .required_feature();
    }
    // A stable sort by tag keeps the order features were added in.
    b.features.sort_by_key(|f| (f.tag, f.seq));
    let mut merged = BudgetVec::<FeatureInfo>::new(budget);
    merged.try_reserve_exact(b.features.len())?;
    for f in b.features.iter().copied() {
        match merged.last_mut() {
            Some(last) if last.tag == f.tag => {
                if f.flags.global {
                    last.flags.global = true;
                    last.max_value = f.max_value;
                    last.default_value = f.default_value;
                } else {
                    last.flags.global = false;
                    last.max_value = last.max_value.max(f.max_value);
                }
                last.flags.has_fallback |= f.flags.has_fallback;
                last.stage = [last.stage[0].min(f.stage[0]), last.stage[1].min(f.stage[1])];
            }
            _ => merged.try_push(f).map_err(|e| e.error)?,
        }
    }
    // The language systems' features, by tag; earlier entries win.
    let lang_features = |t: usize, tag: Tag| {
        let table = &b.tables[t];
        table
            .script(b.script[t])
            .lang_sys(b.language[t])
            .feature_indices()
            .find(|&i| table.feature_tag(i) == tag)
    };
    let mut global_mask = 1 << GLOBAL_BIT_SHIFT;
    let mut next_bit = 4; // After the three glyph flag bits HarfBuzz reserves.
    let mut features = BudgetVec::new(budget);
    for info in merged.iter() {
        let bits = if info.flags.global && info.max_value == 1 {
            0
        } else {
            MAX_BITS.min(u32::BITS - info.max_value.leading_zeros())
        };
        if info.max_value == 0 || next_bit + bits >= GLOBAL_BIT_SHIFT {
            continue;
        }
        let mut index = [None; 2];
        for t in 0..2 {
            if required[t].is_some_and(|r| b.tables[t].feature_tag(r) == info.tag) {
                required_stage[t] = info.stage[t];
            }
            index[t] = lang_features(t, info.tag);
        }
        let mut found = index.iter().any(Option::is_some);
        if !found && info.flags.global_search {
            for t in 0..2 {
                index[t] = b.tables[t].find_feature(info.tag);
            }
            found = index.iter().any(Option::is_some);
        }
        if !found && !info.flags.has_fallback {
            continue;
        }
        let (shift, mask) = if info.flags.global && info.max_value == 1 {
            (GLOBAL_BIT_SHIFT, 1 << GLOBAL_BIT_SHIFT)
        } else {
            let mask = (1_u32 << (next_bit + bits)) - (1 << next_bit);
            let shift = next_bit;
            next_bit += bits;
            global_mask |= (info.default_value << shift) & mask;
            (shift, mask)
        };
        let map = FeatureMap {
            tag: info.tag,
            index,
            stage: info.stage,
            shift,
            mask,
            one_mask: (1 << shift) & mask,
            auto_zwnj: !info.flags.manual_zwnj,
            auto_zwj: !info.flags.manual_zwj,
            random: info.flags.random,
            per_syllable: info.flags.per_syllable,
            needs_fallback: !found,
        };
        features.try_push(map).map_err(|e| e.error)?;
    }
    b.add_gsub_pause(None);
    b.add_gpos_pause(None);
    if let Some(error) = b.error.take() {
        return Err(error);
    }
    let mut lookups = [BudgetVec::new(budget), BudgetVec::new(budget)];
    let mut stages = [BudgetVec::new(budget), BudgetVec::new(budget)];
    for t in 0..2 {
        let table = b.tables[t];
        let list = &mut lookups[t];
        let mut pauses = b.pauses[t].iter().peekable();
        let mut last = 0;
        for stage in 0..b.stage[t] {
            let mut add = |feature: Option<u16>, map: LookupMap| -> Result<()> {
                let Some(feature) = feature else {
                    return Ok(());
                };
                for index in table.feature_lookups(feature) {
                    if let Some(lookup) = table.lookup(index) {
                        let digest = Digest::of_lookup(&lookup, if t == 0 { 7 } else { 9 });
                        list.try_push(LookupMap {
                            index,
                            digest,
                            ..map
                        })
                        .map_err(|e| e.error)?;
                    }
                }
                Ok(())
            };
            if required[t].is_some() && required_stage[t] == stage {
                let map = LookupMap {
                    index: 0,
                    mask: 1 << GLOBAL_BIT_SHIFT,
                    auto_zwnj: true,
                    auto_zwj: true,
                    random: false,
                    per_syllable: false,
                    digest: Digest::EMPTY,
                };
                add(required[t], map)?;
            }
            for f in features.as_slice().iter().filter(|f| f.stage[t] == stage) {
                let map = LookupMap {
                    index: 0,
                    mask: f.mask,
                    auto_zwnj: f.auto_zwnj,
                    auto_zwj: f.auto_zwj,
                    random: f.random,
                    per_syllable: f.per_syllable,
                    digest: Digest::EMPTY,
                };
                add(f.index[t], map)?;
            }
            merge_stage_lookups(list, last);
            last = list.len();
            if let Some(&&(at, pause)) = pauses.peek().filter(|p| p.0 == stage) {
                debug_assert_eq!(at, stage);
                stages[t]
                    .try_push(StageMap {
                        last_lookup: last,
                        pause,
                    })
                    .map_err(|e| e.error)?;
                pauses.next();
            }
        }
    }
    Ok(Plan {
        direction,
        script,
        shaper,
        global_mask,
        features,
        lookups,
        stages,
        chosen_script: b.chosen_script,
        fallback_glyph_classes: false,
        apply_gpos: false,
        apply_kern: false,
        apply_fallback_kern: false,
        zero_marks: false,
        has_gpos_mark: false,
        adjust_mark_positioning_when_zeroing: false,
        fallback_mark_positioning: false,
        kern_mask: 0,
        rtlm_mask: 0,
        frac_mask: 0,
        numr_mask: 0,
        dnom_mask: 0,
        has_frac: false,
    })
}

/// Sorts the lookups a stage added by index and merges duplicates, joining their
/// masks.
fn merge_stage_lookups(list: &mut BudgetVec<LookupMap>, start: usize) {
    let stage = &mut list.as_mut_slice()[start..];
    if stage.len() < 2 {
        return;
    }
    stage.sort_by_key(|l| l.index);
    let mut j = 0;
    for i in 1..stage.len() {
        if stage[i].index == stage[j].index {
            stage[j].mask |= stage[i].mask;
            stage[j].auto_zwnj &= stage[i].auto_zwnj;
            stage[j].auto_zwj &= stage[i].auto_zwj;
        } else {
            j += 1;
            stage[j] = stage[i];
        }
    }
    let keep = start + j + 1;
    while list.len() > keep {
        list.pop();
    }
}
