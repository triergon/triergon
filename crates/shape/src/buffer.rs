//! The glyph buffer: the shaping input and output, and the editing operations
//! substitution and positioning perform on it.
//!
//! The operations keep HarfBuzz's semantics exactly, because they decide clusters:
//! a lookup reads glyphs at `idx` and writes its output after `out_len`, either in
//! place (while the output is no longer than what was read) or into a separate
//! vector, and [`Buffer::sync`] makes the output the new content.

use crate::ot::digest::Digest;
use crate::shapers::{ArabicAction, Jamo};
use crate::unicode::{GeneralCategory, IndicCategory, IndicPosition, UseCategory};
use crate::work::{self, Limits};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};
use triergon_font::GlyphId;

/// One shaped glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyph {
    /// The glyph's index in the face.
    pub id: GlyphId,
    /// The index in the text of the first character the glyph belongs to.
    pub cluster: u32,
}

/// Where a glyph goes, in the units of the face's scale (font units by default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// How far the pen moves horizontally after the glyph.
    pub x_advance: i32,
    /// How far the pen moves vertically after the glyph (zero in horizontal
    /// text).
    pub y_advance: i32,
    /// How far the glyph is drawn to the right of the pen.
    pub x_offset: i32,
    /// How far the glyph is drawn above the pen.
    pub y_offset: i32,
}

/// What a glyph is attached to during positioning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Attachment {
    #[default]
    None,
    /// A mark attached to a base, ligature or mark this many glyphs away.
    Mark(i16),
    /// A cursive attachment to the glyph this many glyphs away.
    Cursive(i16),
}

impl Attachment {
    pub(crate) const fn chain(self) -> i16 {
        match self {
            Self::None => 0,
            Self::Mark(d) | Self::Cursive(d) => d,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pos {
    pub(crate) position: Position,
    pub(crate) attachment: Attachment,
}

/// Character properties a glyph keeps from its character, as HarfBuzz packs them:
/// the general category, flags, and the modified combining class of a mark (or
/// the fallback type of a space) in the high byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UnicodeProps(pub(crate) u16);

impl UnicodeProps {
    const CATEGORY: u16 = 0x1f;
    pub(crate) const IGNORABLE: u16 = 0x20;
    pub(crate) const HIDDEN: u16 = 0x40;
    pub(crate) const CONTINUATION: u16 = 0x80;
    pub(crate) const ZWJ: u16 = 0x100;
    pub(crate) const ZWNJ: u16 = 0x200;
    pub(crate) const VARIATION_SELECTOR: u16 = 0x400;

    pub(crate) fn category(self) -> GeneralCategory {
        GeneralCategory::from_index(u8::try_from(self.0 & Self::CATEGORY).unwrap_or(0))
    }

    pub(crate) fn set_category(&mut self, category: GeneralCategory) {
        self.0 = (self.0 & !Self::CATEGORY) | u16::from(category as u8);
    }

    pub(crate) const fn has(self, flag: u16) -> bool {
        self.0 & flag != 0
    }

    pub(crate) fn set(&mut self, flag: u16, on: bool) {
        if on {
            self.0 |= flag;
        } else {
            self.0 &= !flag;
        }
    }

    /// The high byte: a mark's modified combining class or a space's fallback type.
    pub(crate) const fn high(self) -> u8 {
        (self.0 >> 8) as u8
    }

    pub(crate) fn set_high(&mut self, value: u8) {
        self.0 = (u16::from(value) << 8) | (self.0 & 0xff);
    }
}

/// A glyph's class, from GDEF or guessed when the font has no classes. Each
/// class's discriminant is the lookup flag bit that skips it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum GlyphClass {
    /// No class, or GDEF's component class, which no lookup flag skips.
    #[default]
    Unclassified = 0,
    Base = 0x02,
    Ligature = 0x04,
    Mark = 0x08,
}

/// How a substitution produced a glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Substitution {
    /// One glyph for another.
    Single,
    /// A ligature of several glyphs.
    Ligature,
    /// One of the glyphs a multiple substitution produced.
    Component,
}

/// A glyph's class, a mark's attachment class and what substitutions made the
/// glyph: HarfBuzz's glyph props, kept in its two bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GlyphProps(u16);

impl GlyphProps {
    const CLASS: u16 = 0x0e;
    const SUBSTITUTED: u16 = 0x10;
    const LIGATED: u16 = 0x20;
    const MULTIPLIED: u16 = 0x40;
    const HISTORY: u16 = Self::SUBSTITUTED | Self::LIGATED | Self::MULTIPLIED;

    /// A glyph of `class` that no substitution has touched; marks carry their
    /// attachment class.
    pub(crate) const fn new(class: GlyphClass, mark_attachment_class: u8) -> Self {
        let attachment = match class {
            GlyphClass::Mark => (mark_attachment_class as u16) << 8,
            _ => 0,
        };
        Self(class as u16 | attachment)
    }

    pub(crate) const fn class(self) -> GlyphClass {
        match self.0 & Self::CLASS {
            0x02 => GlyphClass::Base,
            0x04 => GlyphClass::Ligature,
            0x08 => GlyphClass::Mark,
            _ => GlyphClass::Unclassified,
        }
    }

    /// A mark's attachment class (GDEF's MarkAttachClassDef), zero otherwise.
    pub(crate) const fn mark_attachment_class(self) -> u8 {
        (self.0 >> 8) as u8
    }

    pub(crate) const fn substituted(self) -> bool {
        self.0 & Self::SUBSTITUTED != 0
    }

    pub(crate) const fn ligated(self) -> bool {
        self.0 & Self::LIGATED != 0
    }

    pub(crate) const fn multiplied(self) -> bool {
        self.0 & Self::MULTIPLIED != 0
    }

    /// The props of what `how` makes of this glyph: its history extended,
    /// with the class of `class` if given and its own otherwise.
    #[must_use]
    pub(crate) const fn substituted_by(self, how: Substitution, class: Option<Self>) -> Self {
        let mut history = self.0 | Self::SUBSTITUTED;
        match how {
            Substitution::Single => {}
            Substitution::Ligature => history = (history | Self::LIGATED) & !Self::MULTIPLIED,
            Substitution::Component => history |= Self::MULTIPLIED,
        }
        match class {
            Some(class) => Self((history & Self::HISTORY) | class.0),
            None => Self(history),
        }
    }

    /// The props without the record of any substitution.
    #[must_use]
    pub(crate) const fn without_substitution(self) -> Self {
        Self(self.0 & !Self::SUBSTITUTED)
    }

    /// The props without the record of ligation and multiplication.
    #[must_use]
    pub(crate) const fn without_ligation(self) -> Self {
        Self(self.0 & !(Self::LIGATED | Self::MULTIPLIED))
    }
}

/// Ligature identity of a glyph: a ligature's id and component count, or for a
/// mark or component the ligature id and which component it belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LigProps(u8);

impl LigProps {
    const IS_BASE: u8 = 0x10;
    const COUNT: u8 = 0x0f;

    pub(crate) const fn ligature(id: u8, components: u8) -> Self {
        Self((id << 5) | Self::IS_BASE | (components & Self::COUNT))
    }

    pub(crate) const fn mark(id: u8, component: u8) -> Self {
        Self((id << 5) | (component & Self::COUNT))
    }

    pub(crate) const fn id(self) -> u8 {
        self.0 >> 5
    }

    const fn is_ligature_base(self) -> bool {
        self.0 & Self::IS_BASE != 0
    }

    /// The component a mark or component glyph belongs to; zero for a ligature.
    pub(crate) const fn component(self) -> u8 {
        if self.is_ligature_base() {
            0
        } else {
            self.0 & Self::COUNT
        }
    }

    /// A ligature's component count, if this is a ligature's identity.
    const fn components(self) -> Option<u8> {
        if self.is_ligature_base() {
            Some(self.0 & Self::COUNT)
        } else {
            None
        }
    }
}

/// A complex shaper's values for a glyph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ShaperProps {
    #[default]
    None,
    /// The Arabic shaper's joining action, also for the joining scripts of the
    /// Universal Shaping Engine until their categories are set.
    Arabic(ArabicAction),
    Hangul(Jamo),
    /// The Indic, Khmer and Myanmar shapers' category and position.
    Indic(IndicCategory, IndicPosition),
    /// The Universal Shaping Engine's category.
    Use(UseCategory),
}

impl ShaperProps {
    /// The category a syllabic shaper's grammar reads, as its discriminant.
    pub(crate) const fn syllabic_category(self) -> u8 {
        match self {
            Self::Indic(category, _) => category as u8,
            Self::Use(category) => category as u8,
            _ => 0,
        }
    }
}

/// The working record of one glyph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Info {
    /// The character, and once glyphs are mapped the glyph index.
    pub(crate) glyph: u32,
    pub(crate) cluster: u32,
    pub(crate) mask: u32,
    pub(crate) unicode: UnicodeProps,
    pub(crate) glyph_props: GlyphProps,
    pub(crate) lig: LigProps,
    pub(crate) syllable: u8,
    /// The nominal glyph the normaliser found for the character.
    pub(crate) normalized: u32,
    /// A complex shaper's values.
    pub(crate) shaper: ShaperProps,
}

// The record stays as compact as HarfBuzz's, whatever the shapers keep in it.
const _: () = assert!(size_of::<Info>() == 24);

impl Info {
    pub(crate) fn category(&self) -> GeneralCategory {
        self.unicode.category()
    }

    pub(crate) fn is_unicode_mark(&self) -> bool {
        self.category().is_mark()
    }

    /// The modified combining class of a mark, zero otherwise.
    pub(crate) fn combining_class(&self) -> u8 {
        if self.is_unicode_mark() {
            self.unicode.high()
        } else {
            0
        }
    }

    pub(crate) fn set_combining_class(&mut self, class: u8) {
        if self.is_unicode_mark() {
            self.unicode.set_high(class);
        }
    }

    pub(crate) fn is_default_ignorable(&self) -> bool {
        self.unicode.has(UnicodeProps::IGNORABLE) && !self.substituted()
    }

    pub(crate) fn is_hidden(&self) -> bool {
        self.unicode.has(UnicodeProps::HIDDEN)
    }

    pub(crate) fn is_continuation(&self) -> bool {
        self.unicode.has(UnicodeProps::CONTINUATION)
    }

    fn is_format(&self) -> bool {
        self.category() == GeneralCategory::Format
    }

    pub(crate) fn is_zwnj(&self) -> bool {
        self.is_format() && self.unicode.has(UnicodeProps::ZWNJ)
    }

    pub(crate) fn is_zwj(&self) -> bool {
        self.is_format() && self.unicode.has(UnicodeProps::ZWJ)
    }

    pub(crate) const fn is_base_glyph(&self) -> bool {
        matches!(self.glyph_props.class(), GlyphClass::Base)
    }

    pub(crate) const fn is_ligature(&self) -> bool {
        matches!(self.glyph_props.class(), GlyphClass::Ligature)
    }

    pub(crate) const fn is_mark(&self) -> bool {
        matches!(self.glyph_props.class(), GlyphClass::Mark)
    }

    pub(crate) const fn substituted(&self) -> bool {
        self.glyph_props.substituted()
    }

    pub(crate) const fn ligated(&self) -> bool {
        self.glyph_props.ligated()
    }

    pub(crate) const fn multiplied(&self) -> bool {
        self.glyph_props.multiplied()
    }

    /// The component count of a ligature glyph, one for anything else.
    pub(crate) const fn lig_components(&self) -> u8 {
        match self.lig.components() {
            Some(components) if self.is_ligature() => components,
            _ => 1,
        }
    }

    /// The components this glyph contributes to a new ligature: none for a
    /// component of a multiple substitution after its first.
    pub(crate) const fn lig_components_in_ligation(&self) -> u8 {
        if self.multiplied() && self.lig.component() != 0 {
            0
        } else {
            self.lig_components()
        }
    }
}

/// Flags the shaping steps raise to tell later steps what to look for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Scratch {
    pub(crate) default_ignorables: bool,
    pub(crate) continuations: bool,
    pub(crate) fraction_slash: bool,
    pub(crate) cgj: bool,
    pub(crate) space_fallback: bool,
    pub(crate) variation_selector_fallback: bool,
    pub(crate) arabic_stch: bool,
}

/// Up to five characters of context on either side of the item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Context {
    chars: [u32; Self::LENGTH],
    len: u8,
}

impl Context {
    pub(crate) const LENGTH: usize = 5;

    /// Context characters, nearest to the item first.
    pub(crate) fn chars(&self) -> &[u32] {
        &self.chars[..usize::from(self.len)]
    }

    fn push(&mut self, c: u32) {
        if usize::from(self.len) < Self::LENGTH {
            self.chars[usize::from(self.len)] = c;
            self.len += 1;
        }
    }
}

/// Reusable storage for shaping one item: its characters on input, its glyphs and
/// positions on output. Capacity is charged to the [`Budget`] it was created with
/// and kept across items; the work of each shaping call is bounded by its
/// [`Limits`].
#[derive(Debug)]
pub struct Buffer {
    pub(crate) info: BudgetVec<Info>,
    out: BudgetVec<Info>,
    pub(crate) pos: BudgetVec<Pos>,
    pub(crate) idx: usize,
    pub(crate) out_len: usize,
    have_output: bool,
    /// Whether the output lives in `out` rather than in place in `info`.
    separate: bool,
    /// `[before, after]` the item.
    pub(crate) context: [Context; 2],
    pub(crate) scratch: Scratch,
    serial: u32,
    random_state: u32,
    limits: Limits,
    /// The work left before the current call stops with a policy error.
    work: WorkBudget,
    /// The most glyphs the current call may hold.
    max_glyphs: usize,
    /// At least the glyphs in the buffer, for skipping lookups that cannot apply.
    pub(crate) digest: Digest,
}

impl Buffer {
    /// A buffer with the default [`Limits`].
    pub fn new(budget: &Budget) -> Self {
        Self::with_limits(budget, Limits::default())
    }

    /// A buffer whose shaping calls are each bounded by `limits`.
    pub fn with_limits(budget: &Budget, limits: Limits) -> Self {
        Self {
            info: BudgetVec::new(budget),
            out: BudgetVec::new(budget),
            pos: BudgetVec::new(budget),
            idx: 0,
            out_len: 0,
            have_output: false,
            separate: false,
            context: [Context::default(); 2],
            scratch: Scratch::default(),
            serial: 0,
            random_state: 1,
            limits,
            work: WorkBudget::new(0),
            max_glyphs: 0,
            digest: Digest::EMPTY,
        }
    }

    /// The work units the last shaping call spent of its [`Limits`].
    pub fn work_spent(&self) -> u64 {
        self.work.spent()
    }

    /// The number of glyphs.
    pub fn len(&self) -> usize {
        self.info.len()
    }

    /// Whether the buffer holds no glyphs.
    pub fn is_empty(&self) -> bool {
        self.info.is_empty()
    }

    /// The shaped glyphs with their positions, in visual order.
    pub fn glyphs(&self) -> impl ExactSizeIterator<Item = (Glyph, Position)> + '_ {
        self.info
            .as_slice()
            .iter()
            .zip(self.pos.as_slice())
            .map(|(info, pos)| {
                let glyph = Glyph {
                    // Shaped glyphs are glyph indices of the face.
                    id: GlyphId(u16::try_from(info.glyph).unwrap_or(0)),
                    cluster: info.cluster,
                };
                (glyph, pos.position)
            })
    }

    /// Starts a new item: `text[item]` with the characters around it as context.
    /// Clusters are indices into `text`.
    pub(crate) fn load(&mut self, text: &[char], item: std::ops::Range<usize>) -> Result<()> {
        let chars = text
            .get(item.clone())
            .ok_or(Error::Invalid("item outside the text"))?;
        self.info.clear();
        self.out.clear();
        self.pos.clear();
        self.idx = 0;
        self.out_len = 0;
        self.have_output = false;
        self.separate = false;
        self.scratch = Scratch::default();
        self.serial = 0;
        self.random_state = 1;
        self.work = self.limits.budget(chars.len());
        self.max_glyphs = self.limits.max_glyphs(chars.len());
        self.context = [Context::default(); 2];
        for &c in text[..item.start].iter().rev().take(Context::LENGTH) {
            self.context[0].push(u32::from(c));
        }
        for &c in text[item.end..].iter().take(Context::LENGTH) {
            self.context[1].push(u32::from(c));
        }
        self.info.try_reserve_exact(chars.len())?;
        for (i, &c) in chars.iter().enumerate() {
            let cluster = u32::try_from(item.start + i).map_err(|_| Error::Overflow)?;
            let info = Info {
                glyph: u32::from(c),
                cluster,
                ..Info::default()
            };
            self.info.try_push(info).map_err(|e| e.error)?;
        }
        Ok(())
    }

    /// Recomputes the digest from the glyphs.
    pub(crate) fn update_digest(&mut self) {
        let mut digest = Digest::EMPTY;
        for info in self.info.as_slice() {
            digest.add(info.glyph);
        }
        self.digest = digest;
    }

    /// Charges `units` of work against the call's [`Limits`].
    pub(crate) fn charge(&mut self, units: usize) -> Result<()> {
        work::charge(&mut self.work, units)
    }

    /// The work left of the current call, for steps that charge it themselves.
    pub(crate) fn work(&mut self) -> &mut WorkBudget {
        &mut self.work
    }

    pub(crate) fn infos(&self) -> &[Info] {
        self.info.as_slice()
    }

    pub(crate) fn infos_mut(&mut self) -> &mut [Info] {
        self.info.as_mut_slice()
    }

    /// The glyphs and the work left, for a step that charges as it edits.
    pub(crate) fn infos_and_work(&mut self) -> (&mut [Info], &mut WorkBudget) {
        (self.info.as_mut_slice(), &mut self.work)
    }

    pub(crate) fn cur(&self) -> &Info {
        &self.info.as_slice()[self.idx]
    }

    pub(crate) fn cur_mut(&mut self) -> &mut Info {
        let idx = self.idx;
        &mut self.info.as_mut_slice()[idx]
    }

    /// The input glyph `offset` after the current one.
    pub(crate) fn peek(&self, offset: usize) -> Option<&Info> {
        self.info.as_slice().get(self.idx + offset)
    }

    /// The output glyph at `i`.
    pub(crate) fn out_info(&self, i: usize) -> &Info {
        if self.separate {
            &self.out.as_slice()[i]
        } else {
            &self.info.as_slice()[i]
        }
    }

    pub(crate) fn out_info_mut(&mut self, i: usize) -> &mut Info {
        if self.separate {
            &mut self.out.as_mut_slice()[i]
        } else {
            &mut self.info.as_mut_slice()[i]
        }
    }

    /// The output glyphs so far.
    pub(crate) fn out_infos_mut(&mut self) -> &mut [Info] {
        let len = self.out_len;
        if self.separate {
            &mut self.out.as_mut_slice()[..len]
        } else {
            &mut self.info.as_mut_slice()[..len]
        }
    }

    /// The last output glyph.
    pub(crate) fn prev(&self) -> &Info {
        self.out_info(self.out_len - 1)
    }

    /// Glyphs before the current one: the output so far, or the input before
    /// `idx` when no output is being written.
    pub(crate) fn backtrack_len(&self) -> usize {
        if self.have_output {
            self.out_len
        } else {
            self.idx
        }
    }

    pub(crate) fn lookahead_len(&self) -> usize {
        self.len() - self.idx
    }

    /// The glyph `i` of the backtrack: the output when writing one, else the input.
    pub(crate) fn backtrack_info(&self, i: usize) -> &Info {
        if self.have_output {
            self.out_info(i)
        } else {
            &self.info.as_slice()[i]
        }
    }

    pub(crate) fn clear_output(&mut self) {
        self.have_output = true;
        self.separate = false;
        self.idx = 0;
        self.out_len = 0;
        self.out.clear();
    }

    /// Whether the buffer may hold `len` glyphs under its [`Limits`].
    fn check_glyphs(&self, len: usize) -> Result<()> {
        if len > self.max_glyphs {
            return Err(Error::Policy("glyphs in the buffer"));
        }
        Ok(())
    }

    /// Prepares positions for every glyph, all zero.
    pub(crate) fn clear_positions(&mut self) -> Result<()> {
        self.have_output = false;
        self.separate = false;
        self.out_len = 0;
        self.pos.clear();
        self.pos.try_resize(self.len(), Pos::default())
    }

    /// Copies the rest of the input to the output and makes the output the
    /// content.
    pub(crate) fn sync(&mut self) -> Result<()> {
        debug_assert!(self.have_output);
        self.next_glyphs(self.len() - self.idx)?;
        if self.separate {
            std::mem::swap(&mut self.info, &mut self.out);
            self.out.clear();
        } else {
            self.info.try_resize(self.out_len, Info::default())?;
        }
        self.have_output = false;
        self.separate = false;
        self.out_len = 0;
        self.idx = 0;
        Ok(())
    }

    /// Makes room to write `num_out` glyphs in place of `num_in`, leaving the
    /// in-place output once it would overtake the input.
    fn make_room_for(&mut self, num_in: usize, num_out: usize) -> Result<()> {
        self.check_glyphs(self.out_len + num_out)?;
        if !self.separate && self.out_len + num_out > self.idx + num_in {
            self.out.clear();
            reserve(&mut self.out, self.out_len + num_out)?;
            self.out
                .try_extend_from_slice(&self.info.as_slice()[..self.out_len])?;
            self.separate = true;
        }
        if self.separate {
            reserve(&mut self.out, self.out_len + num_out)?;
        }
        Ok(())
    }

    /// Writes `info` as the next output glyph; room must have been made.
    fn write_out(&mut self, info: Info) -> Result<()> {
        if self.separate {
            self.out.try_push(info).map_err(|e| e.error)?;
        } else {
            self.info.as_mut_slice()[self.out_len] = info;
        }
        self.out_len += 1;
        Ok(())
    }

    /// Moves the current glyph to the output.
    pub(crate) fn next_glyph(&mut self) -> Result<()> {
        if self.have_output {
            let info = self.info.as_slice()[self.idx];
            if self.separate {
                self.out.try_push(info).map_err(|e| e.error)?;
            } else if self.out_len != self.idx {
                self.info.as_mut_slice()[self.out_len] = info;
            }
            self.out_len += 1;
        }
        self.idx += 1;
        Ok(())
    }

    pub(crate) fn next_glyphs(&mut self, count: usize) -> Result<()> {
        if self.have_output {
            let range = self.idx..self.idx + count;
            if self.separate {
                self.out
                    .try_extend_from_slice(&self.info.as_slice()[range])?;
            } else if self.out_len != self.idx {
                self.info.as_mut_slice().copy_within(range, self.out_len);
            }
            self.out_len += count;
        }
        self.idx += count;
        Ok(())
    }

    /// Drops the last output glyph.
    pub(crate) fn pop_out(&mut self) {
        self.out_len -= 1;
        if self.separate {
            self.out.pop();
        }
    }

    pub(crate) fn skip_glyph(&mut self) {
        self.idx += 1;
    }

    /// Replaces `num_in` input glyphs by `glyphs`, each a copy of the current glyph
    /// (or of the last output glyph at the end) with a new glyph index.
    pub(crate) fn replace_glyphs(&mut self, num_in: usize, glyphs: &[u32]) -> Result<()> {
        self.make_room_for(num_in, glyphs.len())?;
        self.merge_clusters(self.idx, self.idx + num_in)?;
        let orig = if self.idx < self.len() {
            *self.cur()
        } else {
            *self.prev()
        };
        for &glyph in glyphs {
            self.write_out(Info { glyph, ..orig })?;
        }
        self.idx += num_in;
        Ok(())
    }

    pub(crate) fn replace_glyph(&mut self, glyph: u32) -> Result<()> {
        self.replace_glyphs(1, &[glyph])
    }

    pub(crate) fn output_glyph(&mut self, glyph: u32) -> Result<()> {
        self.replace_glyphs(0, &[glyph])
    }

    /// Moves the boundary between output and input so that the output holds `i`
    /// glyphs.
    pub(crate) fn move_to(&mut self, i: usize) -> Result<()> {
        if !self.have_output {
            self.idx = i;
            return Ok(());
        }
        if self.out_len < i {
            let count = i - self.out_len;
            self.charge(count)?;
            self.make_room_for(count, count)?;
            self.next_glyphs(count)?;
        } else if self.out_len > i {
            let count = self.out_len - i;
            self.charge(count)?;
            if self.idx < count {
                self.shift_forward(count - self.idx)?;
            }
            self.idx -= count;
            self.out_len -= count;
            if self.separate {
                let start = self.idx;
                let moved = &self.out.as_slice()[self.out_len..self.out_len + count];
                self.info.as_mut_slice()[start..start + count].copy_from_slice(moved);
                self.out.try_resize(self.out_len, Info::default())?;
            } else {
                let range = self.out_len..self.out_len + count;
                self.info.as_mut_slice().copy_within(range, self.idx);
            }
        }
        Ok(())
    }

    /// Opens a gap of `count` glyphs before the input (only with separate output).
    fn shift_forward(&mut self, count: usize) -> Result<()> {
        let len = self.len();
        self.check_glyphs(len + count)?;
        reserve(&mut self.info, len + count)?;
        self.info.try_resize(len + count, Info::default())?;
        self.info
            .as_mut_slice()
            .copy_within(self.idx..len, self.idx + count);
        self.idx += count;
        Ok(())
    }

    /// Gives `info[start..end]` one cluster, the smallest, extending the range over
    /// glyphs that share a cluster with its ends.
    pub(crate) fn merge_clusters(&mut self, mut start: usize, mut end: usize) -> Result<()> {
        if end.saturating_sub(start) < 2 {
            return Ok(());
        }
        self.charge(end - start)?;
        let len = self.len();
        let info = self.info.as_mut_slice();
        let cluster = info[start..end]
            .iter()
            .map(|i| i.cluster)
            .min()
            .unwrap_or(0);
        if cluster != info[end - 1].cluster {
            while end < len && info[end - 1].cluster == info[end].cluster {
                work::charge(&mut self.work, 1)?;
                end += 1;
            }
        }
        if cluster != info[start].cluster {
            while self.idx < start && info[start - 1].cluster == info[start].cluster {
                work::charge(&mut self.work, 1)?;
                start -= 1;
            }
        }
        if self.idx == start && info[start].cluster != cluster {
            let old = info[start].cluster;
            let mut i = self.out_len;
            while i > 0 && self.out_info(i - 1).cluster == old {
                self.charge(1)?;
                self.out_info_mut(i - 1).cluster = cluster;
                i -= 1;
            }
        }
        for info in &mut self.info.as_mut_slice()[start..end] {
            info.cluster = cluster;
        }
        Ok(())
    }

    /// [`Self::merge_clusters`] over output glyphs.
    pub(crate) fn merge_out_clusters(&mut self, mut start: usize, mut end: usize) -> Result<()> {
        if end.saturating_sub(start) < 2 {
            return Ok(());
        }
        self.charge(end - start)?;
        let cluster = (start..end)
            .map(|i| self.out_info(i).cluster)
            .min()
            .unwrap_or(0);
        while start > 0 && self.out_info(start - 1).cluster == self.out_info(start).cluster {
            self.charge(1)?;
            start -= 1;
        }
        while end < self.out_len && self.out_info(end - 1).cluster == self.out_info(end).cluster {
            self.charge(1)?;
            end += 1;
        }
        if end == self.out_len {
            let last = self.out_info(end - 1).cluster;
            let idx = self.idx;
            for info in &mut self.info.as_mut_slice()[idx..] {
                if info.cluster != last {
                    break;
                }
                work::charge(&mut self.work, 1)?;
                info.cluster = cluster;
            }
        }
        for i in start..end {
            self.out_info_mut(i).cluster = cluster;
        }
        Ok(())
    }

    /// Drops the current glyph, giving its cluster to a neighbour if no other glyph
    /// keeps it.
    pub(crate) fn delete_glyph(&mut self) -> Result<()> {
        let cluster = self.cur().cluster;
        let next_shares = self.peek(1).is_some_and(|n| n.cluster == cluster);
        let prev_shares = self.out_len > 0 && self.prev().cluster == cluster;
        if !next_shares && !prev_shares {
            if self.out_len > 0 {
                let old = self.prev().cluster;
                if cluster < old {
                    let mut i = self.out_len;
                    while i > 0 && self.out_info(i - 1).cluster == old {
                        self.charge(1)?;
                        self.out_info_mut(i - 1).cluster = cluster;
                        i -= 1;
                    }
                }
            } else if self.idx + 1 < self.len() {
                self.merge_clusters(self.idx, self.idx + 2)?;
            }
        }
        self.skip_glyph();
        Ok(())
    }

    /// Removes the glyphs `remove` selects, without an output pass, handing their
    /// clusters on as [`Self::delete_glyph`] does.
    pub(crate) fn delete_glyphs_inplace(&mut self, remove: impl Fn(&Info) -> bool) -> Result<()> {
        let count = self.len();
        let has_pos = self.pos.len() == count;
        let mut j = 0;
        for i in 0..count {
            let info = self.info.as_slice()[i];
            if remove(&info) {
                let cluster = info.cluster;
                if i + 1 < count && cluster == self.info.as_slice()[i + 1].cluster {
                    continue;
                }
                if j > 0 {
                    let infos = self.info.as_mut_slice();
                    let old = infos[j - 1].cluster;
                    if cluster < old {
                        let mut k = j;
                        while k > 0 && infos[k - 1].cluster == old {
                            work::charge(&mut self.work, 1)?;
                            infos[k - 1].cluster = cluster;
                            k -= 1;
                        }
                    }
                    continue;
                }
                if i + 1 < count {
                    self.merge_clusters(i, i + 2)?;
                }
                continue;
            }
            if j != i {
                self.info.as_mut_slice()[j] = self.info.as_slice()[i];
                if has_pos {
                    self.pos.as_mut_slice()[j] = self.pos.as_slice()[i];
                }
            }
            j += 1;
        }
        self.info.try_resize(j, Info::default())?;
        if has_pos {
            self.pos.try_resize(j, Pos::default())?;
        }
        Ok(())
    }

    /// Stable insertion sort of `info[start..end]` by `key`, merging the clusters
    /// of every glyph that moves with those it passes.
    pub(crate) fn sort_by_key(
        &mut self,
        start: usize,
        end: usize,
        key: impl Fn(&Info) -> u8,
    ) -> Result<()> {
        for i in start + 1..end {
            let k = key(&self.info.as_slice()[i]);
            let mut j = i;
            while j > start && key(&self.info.as_slice()[j - 1]) > k {
                self.charge(1)?;
                j -= 1;
            }
            if i == j {
                continue;
            }
            self.merge_clusters(j, i + 1)?;
            self.info.as_mut_slice()[j..=i].rotate_right(1);
        }
        Ok(())
    }

    pub(crate) fn reverse_range(&mut self, start: usize, end: usize) {
        self.info.as_mut_slice()[start..end].reverse();
        if self.pos.len() == self.info.len() {
            self.pos.as_mut_slice()[start..end].reverse();
        }
    }

    pub(crate) fn reverse(&mut self) {
        self.reverse_range(0, self.len());
    }

    /// Reverses the order of groups of glyphs, keeping each group's own order;
    /// `same_group(a, b)` says whether `b` continues the group of `a`.
    pub(crate) fn reverse_groups(&mut self, same_group: impl Fn(&Info, &Info) -> bool) {
        let len = self.len();
        if len == 0 {
            return;
        }
        let mut start = 0;
        for i in 1..len {
            let infos = self.info.as_slice();
            if !same_group(&infos[i - 1], &infos[i]) {
                self.reverse_range(start, i);
                start = i;
            }
        }
        self.reverse_range(start, len);
        self.reverse();
    }

    /// The end of the group that starts at `start`.
    pub(crate) fn group_end(
        &self,
        start: usize,
        same_group: impl Fn(&Info, &Info) -> bool,
    ) -> usize {
        let infos = self.info.as_slice();
        let mut end = start + 1;
        while end < infos.len() && same_group(&infos[end - 1], &infos[end]) {
            end += 1;
        }
        end
    }

    /// A new ligature id, never zero (three bits, as HarfBuzz allocates them).
    pub(crate) fn allocate_lig_id(&mut self) -> u8 {
        loop {
            let serial = if self.serial == u32::MAX {
                self.serial = 1;
                1
            } else {
                self.serial += 1;
                self.serial - 1
            };
            let id = (serial & 7) as u8;
            if id != 0 {
                return id;
            }
        }
    }

    /// The next number of the minimal standard generator `rand` features use.
    pub(crate) fn random_number(&mut self) -> u32 {
        self.random_state = ((u64::from(self.random_state) * 48271) % 2_147_483_647) as u32;
        self.random_state
    }
}

/// Grows `vec`'s capacity to at least `len` elements, doubling to keep pushes
/// amortised.
fn reserve<T>(vec: &mut BudgetVec<T>, len: usize) -> Result<()> {
    if len > vec.capacity() {
        let target = len.max(vec.capacity().saturating_mul(2)).max(16);
        vec.try_reserve_exact(target - vec.len())?;
    }
    Ok(())
}
