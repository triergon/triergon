//! tx3g sample modifiers resolved into style runs and effect intents for a
//! renderer, following ETSI TS 126 245. Nothing is rendered and no ASS is
//! written; the source sample stays attached, and the requested style is kept
//! apart from effects whose presentation the renderer chooses. The rules are in
//! `src/text/TX3G-SEMANTICS.md`.
use super::{
    Cue, Limits,
    tx3g_config::{ConfigLimits, Configuration, StyleRecord},
};
use std::ops::Range;
use triergon_core::{BudgetVec, Bytes, Time};

mod job;
mod parse;
mod query;
pub use job::SemanticJob;
pub use query::PointQuery;

/// Resource bounds of one [`SemanticJob`].
#[derive(Clone, Copy, Debug)]
pub struct SemanticLimits {
    /// Bounds of the tx3g sample extraction.
    pub text: Limits,
    /// Bounds of the sample description parse.
    pub config: ConfigLimits,
    /// Records read from the sample's modifier boxes, each `krok` header
    /// counting as one; more is `Error::Limit`. Default 65536.
    pub maximum_records: usize,
    /// Resolved runs; more is `Error::Limit`. Default 65536.
    pub maximum_runs: usize,
}
impl Default for SemanticLimits {
    fn default() -> Self {
        Self {
            text: Limits::default(),
            config: ConfigLimits::default(),
            maximum_records: 65536,
            maximum_runs: 65536,
        }
    }
}
/// What a [`SemanticJob::poll`] reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticStatus {
    /// The work budget ran out; poll again.
    Pending,
    /// The memory budget could not hold the next allocation; poll again once
    /// memory is free.
    NeedOutput,
    /// The semantics are complete; take them with
    /// [`SemanticJob::take_output`].
    Ready,
}
/// Where a record was read in the sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordSource {
    /// Index of the modifier box in the cue's `modifiers`.
    pub modifier: usize,
    /// Byte range in the sample payload of the record's fields: one `styl` or
    /// `krok` entry, the `krok` start time and count, or the whole body of any
    /// other box.
    pub bytes: Range<usize>,
}
/// Character range of a record, as declared and as applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CharacterSpan {
    /// Start offset the record declares, in characters.
    pub declared_start: u16,
    /// Exclusive end offset the record declares, in characters.
    pub declared_end: u16,
    /// Characters the record applies to, half-open, counted in Unicode scalars
    /// with the UTF-16 BOM excluded.
    pub characters: Range<usize>,
    /// Whether `declared_end` is the character count plus one, which `hlit`
    /// and `krok` records may declare; `characters` then ends at the count.
    pub end_of_text_sentinel: bool,
}
/// Requested text style: a `styl` record or the description's default style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextStyle {
    /// Font identifier, an entry of the description's `ftab`.
    pub font_id: u16,
    /// Face style flags: bit 0 bold, bit 1 italic, bit 2 underline; other bits
    /// are kept.
    pub face_flags: u8,
    /// Font size in pixels.
    pub font_size: u8,
    /// Text colour as red, green, blue and alpha bytes.
    pub rgba: [u8; 4],
}
impl TextStyle {
    /// Whether face flag bit 0, bold, is set.
    pub fn bold(self) -> bool {
        self.face_flags & 1 != 0
    }
    /// Whether face flag bit 1, italic, is set.
    pub fn italic(self) -> bool {
        self.face_flags & 2 != 0
    }
    /// Whether face flag bit 2, underline, is set.
    pub fn underline(self) -> bool {
        self.face_flags & 4 != 0
    }
    /// The face flag bits other than bold, italic and underline.
    pub fn uninterpreted_face_bits(self) -> u8 {
        self.face_flags & !7
    }
}
impl From<StyleRecord> for TextStyle {
    fn from(value: StyleRecord) -> Self {
        Self {
            font_id: value.font_id,
            face_flags: value.face_flags,
            font_size: value.font_size,
            rgba: value.rgba,
        }
    }
}
/// A `styl` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleSpan {
    /// Where the record was read.
    pub source: RecordSource,
    /// Characters the style applies to.
    pub span: CharacterSpan,
    /// Style the record requests.
    pub style: TextStyle,
}
/// An `hlit` or `blnk` record: a highlighted or blinking character range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanEffect {
    /// Where the record was read.
    pub source: RecordSource,
    /// Characters the effect applies to.
    pub span: CharacterSpan,
}
/// An `href` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hyperlink {
    /// Where the record was read.
    pub source: RecordSource,
    /// Characters the link covers.
    pub span: CharacterSpan,
    /// Byte range of the URL in the sample payload
    /// (`Semantics::source().input.payload`), checked as UTF-8 and not
    /// otherwise interpreted.
    pub url: Range<usize>,
    /// Byte range of the alternate text in the sample payload, checked as
    /// UTF-8; empty when the box has none.
    pub alternate: Range<usize>,
}
/// The header of a `krok` box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KaraokeHeader {
    /// Where the header was read.
    pub source: RecordSource,
    /// Highlight start in track ticks from the start of the sample; the first
    /// entry starts here.
    pub start_tick: u32,
    /// Entry count; the box holds exactly this many entries.
    pub entries: u16,
}
/// One `krok` entry: characters highlighted from `start_tick` until
/// `end_tick`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KaraokeEvent {
    /// Where the entry was read.
    pub source: RecordSource,
    /// Characters the entry highlights.
    pub span: CharacterSpan,
    /// Start in track ticks from the start of the sample: the previous entry's
    /// end, or the header's start for the first entry.
    pub start_tick: u32,
    /// End in track ticks from the start of the sample; never before
    /// `start_tick`.
    pub end_tick: u32,
    /// `start_tick` in seconds; `None` without a track timescale.
    pub start_time: Option<Time>,
    /// `end_tick` in seconds; `None` without a track timescale.
    pub end_time: Option<Time>,
}
/// Text wrapping a `twrp` box requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapMode {
    /// Flag 0: no automatic wrapping.
    NoWrap,
    /// Flag 1: automatic soft wrapping.
    Soft,
}
/// Scroll direction, bits 7 and 8 of the display flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDirection {
    /// Value 0: text scrolls up.
    Up,
    /// Value 1: text scrolls left.
    Left,
    /// Value 2: text scrolls down.
    Down,
    /// Value 3: text scrolls right.
    Right,
}
/// The sample description's display flags, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayIntent {
    /// The flags as written.
    pub raw_flags: u32,
    /// Bit `0x20`: text scrolls in.
    pub scroll_in: bool,
    /// Bit `0x40`: text scrolls out.
    pub scroll_out: bool,
    /// Bits `0x180`: the direction text scrolls in and out.
    pub scroll_direction: ScrollDirection,
    /// Bit `0x800`: karaoke highlighting also covers the text before the
    /// current entry.
    pub continuous_karaoke: bool,
    /// Bit `0x20000`: text is written vertically.
    pub vertical: bool,
    /// Bit `0x40000`: fill text region.
    pub fill_region: bool,
    /// The bits of `raw_flags` that none of the fields above decode.
    pub unknown_bits: u32,
}
impl DisplayIntent {
    pub(super) fn new(flags: u32) -> Self {
        Self {
            raw_flags: flags,
            scroll_in: flags & 0x20 != 0,
            scroll_out: flags & 0x40 != 0,
            scroll_direction: match (flags >> 7) & 3 {
                0 => ScrollDirection::Up,
                1 => ScrollDirection::Left,
                2 => ScrollDirection::Down,
                _ => ScrollDirection::Right,
            },
            continuous_karaoke: flags & 0x800 != 0,
            vertical: flags & 0x20000 != 0,
            fill_region: flags & 0x40000 != 0,
            unknown_bits: flags & !0x609e0,
        }
    }
}
/// A value read from a sample modifier box, with where it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleValue<T> {
    /// The value as the box gives it.
    pub value: T,
    /// Where the box's fields were read.
    pub source: RecordSource,
}
/// Where a resolved value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueOrigin {
    /// A modifier box of the sample.
    Sample,
    /// The sample description.
    Description,
    /// Neither carries it; the default TS 126 245 specifies applies.
    SpecifiedAbsentDefault,
}
/// A resolved value and where it comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DerivedValue<T> {
    /// The value in effect.
    pub value: T,
    /// Where `value` comes from.
    pub origin: ValueOrigin,
}
/// Sample-wide effects: the boxes a sample carries at most once, and the values
/// resolved from them and the description.
#[derive(Clone, Debug, Default)]
pub struct GlobalEffects {
    /// `hclr` highlight colour as red, green, blue and alpha bytes; `None`
    /// without `hclr`.
    pub highlight_colour: Option<SampleValue<[u8; 4]>>,
    /// `dlay` scroll delay in track ticks; `None` without `dlay`.
    pub scroll_delay: Option<SampleValue<u32>>,
    /// `tbox` text box: top, left, bottom and right as written; `None` without
    /// `tbox`.
    pub text_box: Option<SampleValue<[i16; 4]>>,
    /// `twrp` wrap mode; `None` without `twrp`.
    pub wrap: Option<SampleValue<WrapMode>>,
    /// The sample's `disp` disparity in signed 1/16 pixels; `None` without
    /// `disp`.
    pub disparity: Option<SampleValue<i16>>,
    /// The description's `disp` disparity in signed 1/16 pixels; `None`
    /// without one.
    pub description_disparity: Option<i16>,
    /// Text box in effect: the sample's `tbox`, else the description's default
    /// box; `None` without either.
    pub resolved_text_box: Option<DerivedValue<[i16; 4]>>,
    /// Disparity in effect, in signed 1/16 pixels: the sample's, else the
    /// description's, else zero when a description was parsed; `None`
    /// otherwise.
    pub resolved_disparity: Option<DerivedValue<i16>>,
    /// Display flags of the description; `None` without one.
    pub display: Option<DisplayIntent>,
}
impl GlobalEffects {
    /// Scroll delay in track ticks: the `dlay` value, or the specified default
    /// of zero without `dlay`.
    pub fn scroll_delay_ticks(&self) -> DerivedValue<u32> {
        self.scroll_delay.as_ref().map_or(
            DerivedValue {
                value: 0,
                origin: ValueOrigin::SpecifiedAbsentDefault,
            },
            |v| DerivedValue {
                value: v.value,
                origin: ValueOrigin::Sample,
            },
        )
    }
    /// Resolved disparity as whole-pixel shifts `(-d, d)`, where `d` is the
    /// 1/16-pixel value divided by 16 and truncated toward zero; `None` when
    /// `resolved_disparity` is.
    pub fn disparity_pixels(&self) -> Option<(i32, i32)> {
        self.resolved_disparity.map(|v| {
            let shift = i32::from(v.value) / 16;
            (-shift, shift)
        })
    }
}
/// A font of the description's `ftab`, with its name converted to UTF-8.
#[derive(Debug)]
pub struct SemanticFont {
    /// Font identifier.
    pub id: u16,
    /// Byte range of the name in the description bytes, `Configuration::raw`,
    /// byte-order mark included.
    pub raw_name: Range<usize>,
    /// The name in UTF-8, without the byte-order mark and not trimmed.
    pub name_utf8: Bytes,
    /// Whether the name is UTF-16BE after a FEFF byte-order mark; otherwise it
    /// is UTF-8.
    pub utf16_bom: bool,
}
/// Where a run's style comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleOrigin {
    /// The description's default style.
    Description,
    /// The `styl` record at this index of [`Semantics::styles`].
    Sample(usize),
}
/// Characters under the same style, highlight, hyperlink and blink records.
/// Runs cover the text in order, each character once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRun {
    /// Character range, half-open.
    pub characters: Range<usize>,
    /// Byte range of the run's text in the sample payload.
    pub source_bytes: Range<usize>,
    /// Byte range of the run's text in the cue's `decoded_utf8`.
    pub utf8_bytes: Range<usize>,
    /// Requested style; `None` when no `styl` record covers the run and there
    /// is no description.
    pub style: Option<TextStyle>,
    /// Where `style` comes from; `None` when `style` is.
    pub style_origin: Option<StyleOrigin>,
    /// Index in [`Semantics::fonts`] of the style's font; `None` without a
    /// description.
    pub font_index: Option<usize>,
    /// Index in [`Semantics::highlights`] of the `hlit` record covering the
    /// run.
    pub highlight: Option<usize>,
    /// Index in [`Semantics::hyperlinks`] of the `href` record covering the
    /// run.
    pub hyperlink: Option<usize>,
    /// Index in [`Semantics::blink`] of the `blnk` record covering the run.
    pub blink: Option<usize>,
}
/// Whether an effect applies, where the answer can depend on what is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truth {
    /// It does not apply.
    No,
    /// It applies.
    Yes,
    /// It depends on a time or a display flag that is unknown.
    Unknown,
}
/// How a renderer is asked to paint a character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaintIntent {
    /// The requested style, unchanged.
    RequestedStyle,
    /// Static highlight with the `hclr` colour, red, green, blue and alpha, as
    /// background.
    StaticBackground([u8; 4]),
    /// Static highlight without `hclr`: an inverted presentation the renderer
    /// chooses.
    StaticTerminalInvert,
    /// Active karaoke highlight as text colour: the `hclr` colour, or the
    /// renderer's choice when `None`.
    KaraokeForeground(Option<[u8; 4]>),
    /// Part of a hyperlink, presented as the renderer chooses.
    HyperlinkTerminalStyle,
    /// Karaoke highlighting may apply, but the time or the continuous-karaoke
    /// flag needed to decide is unknown.
    UnresolvedDynamicExtent,
}
/// What applies at one character and time, as [`PointQuery`] finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointStyle {
    /// Requested style of the run; `None` as for [`ResolvedRun`].
    pub base_style: Option<TextStyle>,
    /// Where `base_style` comes from; `None` when `base_style` is.
    pub style_origin: Option<StyleOrigin>,
    /// Index in [`Semantics::fonts`] of the style's font; `None` without a
    /// description.
    pub font_index: Option<usize>,
    /// Index in [`Semantics::hyperlinks`] of the `href` record covering the
    /// character.
    pub hyperlink: Option<usize>,
    /// Whether a `blnk` record covers the character.
    pub blink_requested: bool,
    /// Whether an `hlit` record covers the character.
    pub static_highlight: bool,
    /// Whether karaoke highlighting applies at the queried time.
    pub karaoke: Truth,
    /// Paint intent: unknown karaoke first, then hyperlink, active karaoke,
    /// static highlight and the requested style.
    pub paint: PaintIntent,
}
/// What the job could check with the inputs it had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidationContext {
    /// Whether a sample description was parsed.
    pub description_parsed: bool,
    /// Whether the input had no `codec_private` description bytes.
    pub description_bytes_absent: bool,
    /// Track timescale given to the job, in ticks per second.
    pub explicit_track_timescale: Option<u32>,
    /// Whether the input timing has a duration.
    pub sample_duration_known: bool,
    /// Whether karaoke and scroll delay times were checked against the sample
    /// duration, which needs both the timescale and the duration.
    pub duration_bounds_checked: bool,
    /// `false` when a static highlight or hyperlink may overlap continuous
    /// karaoke highlighting and no description says whether karaoke is
    /// continuous.
    pub conditional_combinations_checked: bool,
}
#[derive(Debug)]
pub(super) struct FontPage {
    entries: [u16; 256],
}
#[derive(Debug)]
pub(super) struct FontIndex {
    pages: BudgetVec<FontPage>,
    mapping: [Option<u8>; 256],
}
impl FontIndex {
    fn new(pool: &triergon_core::Budget) -> Self {
        Self {
            pages: BudgetVec::new(pool),
            mapping: [None; 256],
        }
    }
    fn get(&self, id: u16) -> Option<usize> {
        let page = usize::from(self.mapping[usize::from(id >> 8)]?);
        let value = self.pages.as_slice().get(page)?.entries[usize::from(id & 255)];
        value.checked_sub(1).map(usize::from)
    }
}

/// A resolved tx3g sample: its source cue and description, and the records,
/// effects and runs derived from them. Storage stays charged to the job's
/// budget.
#[derive(Debug)]
pub struct Semantics {
    source: Cue,
    description: Option<Configuration>,
    fonts: BudgetVec<SemanticFont>,
    styles: BudgetVec<StyleSpan>,
    highlights: BudgetVec<SpanEffect>,
    hyperlinks: BudgetVec<Hyperlink>,
    blink: BudgetVec<SpanEffect>,
    karaoke_header: Option<KaraokeHeader>,
    karaoke: BudgetVec<KaraokeEvent>,
    /// Maximum end of a positive-duration event, built only while
    /// committing validated records. This is a possible continuous prefix.
    possible_karaoke_end: Option<usize>,
    globals: GlobalEffects,
    runs: BudgetVec<ResolvedRun>,
    characters: usize,
    font_index: FontIndex,
    context: ValidationContext,
}
impl Semantics {
    /// The sample as the tx3g extractor read it, with its input and modifier
    /// boxes.
    pub fn source(&self) -> &Cue {
        &self.source
    }
    /// The parsed sample description; `None` when the job had no layout.
    pub fn description(&self) -> Option<&Configuration> {
        self.description.as_ref()
    }
    /// The description's fonts in `ftab` order.
    pub fn fonts(&self) -> &[SemanticFont] {
        self.fonts.as_slice()
    }
    /// The font with identifier `id`; `None` when no description font has it.
    pub fn font(&self, id: u16) -> Option<&SemanticFont> {
        self.font_index
            .get(id)
            .and_then(|i| self.fonts.as_slice().get(i))
    }
    /// `styl` records, ordered and not overlapping.
    pub fn styles(&self) -> &[StyleSpan] {
        self.styles.as_slice()
    }
    /// `hlit` records, ordered and not overlapping.
    pub fn highlights(&self) -> &[SpanEffect] {
        self.highlights.as_slice()
    }
    /// `href` records, ordered and not overlapping.
    pub fn hyperlinks(&self) -> &[Hyperlink] {
        self.hyperlinks.as_slice()
    }
    /// `blnk` records, ordered and not overlapping.
    pub fn blink(&self) -> &[SpanEffect] {
        self.blink.as_slice()
    }
    /// The `krok` header; `None` without `krok`.
    pub fn karaoke_header(&self) -> Option<&KaraokeHeader> {
        self.karaoke_header.as_ref()
    }
    /// `krok` entries, ordered by character range and time.
    pub fn karaoke(&self) -> &[KaraokeEvent] {
        self.karaoke.as_slice()
    }
    /// Sample-wide effects.
    pub fn globals(&self) -> &GlobalEffects {
        &self.globals
    }
    /// Runs covering every character once, in order.
    pub fn runs(&self) -> &[ResolvedRun] {
        self.runs.as_slice()
    }
    /// Characters in the text: Unicode scalars, line breaks included and the
    /// UTF-16 BOM excluded.
    pub fn character_count(&self) -> usize {
        self.characters
    }
    /// What the job could check with the inputs it had.
    pub fn validation_context(&self) -> ValidationContext {
        self.context
    }
}
