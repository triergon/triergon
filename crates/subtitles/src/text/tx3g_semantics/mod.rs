//! Bounded tx3g semantic intents, not glyph rendering or a native ASS adapter.
//! Source carriage, requested attributes and terminal-dependent effects remain
//! separate. See TX3G-SEMANTICS.md for the pinned grammar and strict profile.
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

#[derive(Clone, Copy, Debug)]
pub struct SemanticLimits {
    pub text: Limits,
    pub config: ConfigLimits,
    pub maximum_records: usize,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticStatus {
    Pending,
    NeedOutput,
    Ready,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordSource {
    pub modifier: usize,
    pub bytes: Range<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CharacterSpan {
    pub declared_start: u16,
    pub declared_end: u16,
    pub characters: Range<usize>,
    /// The permitted N+1 end sentinel is retained, not silently clamped.
    pub end_of_text_sentinel: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextStyle {
    pub font_id: u16,
    pub face_flags: u8,
    pub font_size: u8,
    pub rgba: [u8; 4],
}
impl TextStyle {
    pub fn bold(self) -> bool {
        self.face_flags & 1 != 0
    }
    pub fn italic(self) -> bool {
        self.face_flags & 2 != 0
    }
    pub fn underline(self) -> bool {
        self.face_flags & 4 != 0
    }
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleSpan {
    pub source: RecordSource,
    pub span: CharacterSpan,
    pub style: TextStyle,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanEffect {
    pub source: RecordSource,
    pub span: CharacterSpan,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hyperlink {
    pub source: RecordSource,
    pub span: CharacterSpan,
    /// Exact UTF8 byte spans into source().input.payload, without URI fetching,
    /// normalization, invented absolute context, or terminal link styling.
    pub url: Range<usize>,
    pub alternate: Range<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KaraokeHeader {
    pub source: RecordSource,
    pub start_tick: u32,
    pub entries: u16,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KaraokeEvent {
    pub source: RecordSource,
    pub span: CharacterSpan,
    pub start_tick: u32,
    pub end_tick: u32,
    pub start_time: Option<Time>,
    pub end_time: Option<Time>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapMode {
    NoWrap,
    Soft,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Left,
    Down,
    Right,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayIntent {
    pub raw_flags: u32,
    pub scroll_in: bool,
    pub scroll_out: bool,
    pub scroll_direction: ScrollDirection,
    pub continuous_karaoke: bool,
    pub vertical: bool,
    pub fill_region: bool,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleValue<T> {
    pub value: T,
    pub source: RecordSource,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueOrigin {
    Sample,
    Description,
    SpecifiedAbsentDefault,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DerivedValue<T> {
    pub value: T,
    pub origin: ValueOrigin,
}
#[derive(Clone, Debug, Default)]
pub struct GlobalEffects {
    pub highlight_colour: Option<SampleValue<[u8; 4]>>,
    pub scroll_delay: Option<SampleValue<u32>>,
    pub text_box: Option<SampleValue<[i16; 4]>>,
    pub wrap: Option<SampleValue<WrapMode>>,
    pub disparity: Option<SampleValue<i16>>,
    pub description_disparity: Option<i16>,
    pub resolved_text_box: Option<DerivedValue<[i16; 4]>>,
    pub resolved_disparity: Option<DerivedValue<i16>>,
    pub display: Option<DisplayIntent>,
}
impl GlobalEffects {
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
    /// Integer pixel shifts use truncation toward zero. Sixteenth-pixel source
    /// values stay exact; negating i16::MIN is widened before arithmetic.
    pub fn disparity_pixels(&self) -> Option<(i32, i32)> {
        self.resolved_disparity.map(|v| {
            let shift = i32::from(v.value) / 16;
            (-shift, shift)
        })
    }
}
#[derive(Debug)]
pub struct SemanticFont {
    pub id: u16,
    pub raw_name: Range<usize>,
    pub name_utf8: Bytes,
    pub utf16_bom: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleOrigin {
    Description,
    Sample(usize),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRun {
    pub characters: Range<usize>,
    pub source_bytes: Range<usize>,
    pub utf8_bytes: Range<usize>,
    pub style: Option<TextStyle>,
    pub style_origin: Option<StyleOrigin>,
    pub font_index: Option<usize>,
    pub highlight: Option<usize>,
    pub hyperlink: Option<usize>,
    pub blink: Option<usize>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truth {
    No,
    Yes,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaintIntent {
    RequestedStyle,
    StaticBackground([u8; 4]),
    StaticTerminalInvert,
    KaraokeForeground(Option<[u8; 4]>),
    HyperlinkTerminalStyle,
    UnresolvedDynamicExtent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointStyle {
    pub base_style: Option<TextStyle>,
    pub style_origin: Option<StyleOrigin>,
    pub font_index: Option<usize>,
    pub hyperlink: Option<usize>,
    pub blink_requested: bool,
    pub static_highlight: bool,
    pub karaoke: Truth,
    pub paint: PaintIntent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidationContext {
    pub description_parsed: bool,
    pub description_bytes_absent: bool,
    pub explicit_track_timescale: Option<u32>,
    pub sample_duration_known: bool,
    pub duration_bounds_checked: bool,
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

/// Complete source sample, description and derived semantic views. All vectors
/// are charged and exposed read-only; a public Cue is never a validation token.
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
    pub fn source(&self) -> &Cue {
        &self.source
    }
    pub fn description(&self) -> Option<&Configuration> {
        self.description.as_ref()
    }
    pub fn fonts(&self) -> &[SemanticFont] {
        self.fonts.as_slice()
    }
    pub fn font(&self, id: u16) -> Option<&SemanticFont> {
        self.font_index
            .get(id)
            .and_then(|i| self.fonts.as_slice().get(i))
    }
    pub fn styles(&self) -> &[StyleSpan] {
        self.styles.as_slice()
    }
    pub fn highlights(&self) -> &[SpanEffect] {
        self.highlights.as_slice()
    }
    pub fn hyperlinks(&self) -> &[Hyperlink] {
        self.hyperlinks.as_slice()
    }
    pub fn blink(&self) -> &[SpanEffect] {
        self.blink.as_slice()
    }
    pub fn karaoke_header(&self) -> Option<&KaraokeHeader> {
        self.karaoke_header.as_ref()
    }
    pub fn karaoke(&self) -> &[KaraokeEvent] {
        self.karaoke.as_slice()
    }
    pub fn globals(&self) -> &GlobalEffects {
        &self.globals
    }
    pub fn runs(&self) -> &[ResolvedRun] {
        self.runs.as_slice()
    }
    pub fn character_count(&self) -> usize {
        self.characters
    }
    pub fn validation_context(&self) -> ValidationContext {
        self.context
    }
}
