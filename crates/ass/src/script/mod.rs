//! The script model and libass's line-oriented reader for it.

mod event;
mod fonts;
mod info;
mod overrides;
mod read_order;
mod style;

pub use event::Event;
pub use fonts::EmbeddedFont;
pub use info::{Info, ScriptType, WrapStyle, YCbCrMatrix};
pub use style::{
    Alignment, Bold, BorderStyle, ColourSlot, Colours, HAlign, Italic, Justify, Margins, Rgba,
    Style, VAlign,
};

use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

use crate::number::header_int;
use crate::scan::{next_token, starts_with_ignore_case, trim_start, until_nul};
use crate::{Limits, charge};
use fonts::PendingFont;
use info::{Header, Headers};
use read_order::ReadOrder;
use style::Field;

const ASS_STYLE_FORMAT: &[u8] = b"Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, \
OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, \
BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding";
const ASS_EVENT_FORMAT: &[u8] =
    b"Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";
const SSA_STYLE_FORMAT: &[u8] = b"Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, \
TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, \
MarginR, MarginV, AlphaLevel, Encoding";
const SSA_EVENT_FORMAT: &[u8] =
    b"Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";

/// The section a line belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    None,
    Info,
    Styles,
    Events,
    Fonts,
}

/// How a script is read.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options<'a> {
    /// Bounds on the work of each call and on what the script holds.
    pub limits: Limits,
    /// Keep the decoded fonts of `[Fonts]` (libass's `ass_set_extract_fonts`).
    pub extract_fonts: bool,
    /// `[Style.]Field=Value` overrides (libass's `ass_set_style_overrides`),
    /// applied after a whole script or a codec private is read.
    pub style_overrides: &'a [&'a [u8]],
}

/// A parsed ASS or SSA script: header values, styles, events, embedded fonts
/// and the parser state libass keeps between packets of a stream.
#[derive(Debug)]
pub struct Script {
    budget: Budget,
    limits: Limits,
    extract_fonts: bool,
    overrides: BudgetVec<Bytes>,
    info: Info,
    styles: BudgetVec<Style>,
    events: BudgetVec<Event>,
    default_style: usize,
    style_format: Option<Bytes>,
    event_format: Option<Bytes>,
    fonts: BudgetVec<EmbeddedFont>,
    section: Section,
    headers: Headers,
    pending_font: Option<PendingFont>,
    read_order: ReadOrder,
    check_read_order: bool,
    /// No event held ends before this: [`Script::prune_events`] has nothing
    /// to remove for an earlier deadline.
    earliest_end: i64,
}

impl Script {
    /// An empty script for a stream (`ass_new_track`): libass's default style
    /// and nothing else. Feed it with [`Script::process_codec_private`] and
    /// [`Script::process_chunk`].
    pub fn new(budget: &Budget, options: &Options<'_>) -> Result<Self> {
        let mut overrides = BudgetVec::new(budget);
        for &item in options.style_overrides {
            let item = Bytes::copy_from_slice(budget, until_nul(item))?;
            overrides.try_push(item).map_err(|e| e.error)?;
        }
        let mut styles = BudgetVec::new(budget);
        styles
            .try_push(Style::libass_default(budget)?)
            .map_err(|e| e.error)?;
        Ok(Self {
            budget: budget.clone(),
            limits: options.limits,
            extract_fonts: options.extract_fonts,
            overrides,
            info: Info::default(),
            styles,
            events: BudgetVec::new(budget),
            default_style: 0,
            style_format: None,
            event_format: None,
            fonts: BudgetVec::new(budget),
            section: Section::None,
            headers: Headers::default(),
            pending_font: None,
            read_order: ReadOrder::new(budget),
            check_read_order: true,
            earliest_end: i64::MAX,
        })
    }

    /// A whole script in memory (`ass_read_memory` without a codepage).
    /// Events are numbered in file order and the style overrides applied.
    ///
    /// # Errors
    ///
    /// `Invalid` for a script that declares no script type, for which libass
    /// returns no track; `Policy` past a [`Limits`] bound; `Limit` or
    /// `Backpressure` when the budget is exhausted.
    pub fn parse(budget: &Budget, data: &[u8], options: &Options<'_>) -> Result<Self> {
        let mut script = Self::new(budget, options)?;
        let mut work = options.limits.work();
        script.process_text(until_nul(data), &mut work)?;
        for (i, event) in script.events.as_mut_slice().iter_mut().enumerate() {
            event.read_order = i as i32;
        }
        if script.info.script_type == ScriptType::Unknown {
            return Err(Error::Invalid("no script type"));
        }
        script.apply_overrides(&mut work)?;
        Ok(script)
    }

    /// More script text (`ass_process_data`): header, style and event lines
    /// in the current section.
    ///
    /// # Errors
    ///
    /// As [`Script::parse`], except that no script type is required.
    pub fn process_data(&mut self, data: &[u8]) -> Result<()> {
        let mut work = self.limits.work();
        self.process_text(until_nul(data), &mut work)
    }

    /// A Matroska codec private (`ass_process_codec_private`): the header and
    /// styles, a fallback event format when it has none, then the overrides.
    ///
    /// # Errors
    ///
    /// As [`Script::process_data`].
    pub fn process_codec_private(&mut self, data: &[u8]) -> Result<()> {
        let mut work = self.limits.work();
        self.process_text(until_nul(data), &mut work)?;
        if self.event_format.is_none() {
            self.event_format_fallback()?;
        }
        self.apply_overrides(&mut work)
    }

    /// Style lines from another script (`ass_read_styles` on memory): every
    /// line is read in the styles section, which is restored afterwards.
    ///
    /// # Errors
    ///
    /// As [`Script::process_data`].
    pub fn process_styles(&mut self, data: &[u8]) -> Result<()> {
        let mut work = self.limits.work();
        let section = std::mem::replace(&mut self.section, Section::Styles);
        let result = self.process_text(until_nul(data), &mut work);
        self.section = section;
        result
    }

    /// One Matroska packet (`ass_process_chunk`): `ReadOrder, Layer`, then the
    /// event format after its first three fields, timed by the container in
    /// milliseconds. A packet whose ReadOrder was seen is dropped while the
    /// check is on, as is one that ends before its text or arrives before an
    /// event format.
    ///
    /// # Errors
    ///
    /// As [`Script::process_data`].
    pub fn process_chunk(&mut self, data: &[u8], start: i64, duration: i64) -> Result<()> {
        let mut work = self.limits.work();
        let check = self.check_read_order;
        if check && !self.read_order.is_active() {
            charge(&mut work, self.events.len())?;
            for event in self.events.as_slice() {
                if self.read_order.test_and_set(event.read_order)?.is_none() {
                    break;
                }
            }
        }
        if self.event_format.is_none() {
            return Ok(());
        }
        let data = until_nul(data);
        charge(&mut work, data.len())?;
        let mut rest = data;
        let Some(token) = next_token(&mut rest, false) else {
            return Ok(());
        };
        let read_order = crate::c_long::atoi(token);
        if check && self.is_duplicate(read_order, &mut work)? {
            return Ok(());
        }
        let Some(token) = next_token(&mut rest, false) else {
            return Ok(());
        };
        let layer = header_int(token);
        if let Some(mut event) = self.event_tail(rest, 3, &mut work)? {
            event.read_order = read_order;
            event.layer = layer;
            event.start = start;
            event.duration = duration;
            self.push_event(event)?;
        }
        Ok(())
    }

    /// Whether [`Script::process_chunk`] drops packets whose ReadOrder was
    /// seen (`ass_set_check_readorder`); on by default.
    pub fn set_check_read_order(&mut self, check: bool) {
        self.check_read_order = check;
    }

    /// Removes every event (`ass_flush_events`), as after a seek.
    pub fn flush_events(&mut self) {
        while self.events.pop().is_some() {}
        self.read_order.drop_bitmap();
    }

    /// Removes the events that end before `deadline` (`ass_prune_events`),
    /// keeping the order of the rest; their ReadOrder may be seen again. The
    /// events are scanned only when one may end before the deadline, so a
    /// player can prune at every frame; a scan is bounded by
    /// [`Limits::events`].
    pub fn prune_events(&mut self, deadline: i64) {
        if deadline < self.earliest_end {
            return;
        }
        self.earliest_end = i64::MAX;
        let events = self.events.as_mut_slice();
        let mut kept = 0;
        for i in 0..events.len() {
            let end = events[i].end();
            if end < deadline {
                if self.check_read_order {
                    self.read_order.clear(events[i].read_order);
                }
            } else {
                self.earliest_end = self.earliest_end.min(end);
                events.swap(kept, i);
                kept += 1;
            }
        }
        while self.events.len() > kept {
            self.events.pop();
        }
    }

    /// The `[Script Info]` values.
    pub fn info(&self) -> &Info {
        &self.info
    }

    /// The styles: libass's default style, then the script's in order.
    pub fn styles(&self) -> &[Style] {
        self.styles.as_slice()
    }

    /// The events held, in the order they were read.
    pub fn events(&self) -> &[Event] {
        self.events.as_slice()
    }

    /// The events, for the renderer's records.
    pub(crate) fn events_mut(&mut self) -> &mut [Event] {
        self.events.as_mut_slice()
    }

    /// The index into [`Script::styles`] of the style named `Default` last,
    /// else 0.
    pub const fn default_style(&self) -> usize {
        self.default_style
    }

    /// The style `Format:` line, without its prefix.
    pub fn style_format(&self) -> Option<&[u8]> {
        self.style_format.as_deref()
    }

    /// The event `Format:` line, without its prefix.
    pub fn event_format(&self) -> Option<&[u8]> {
        self.event_format.as_deref()
    }

    /// Fonts decoded from `[Fonts]` when [`Options::extract_fonts`] is set.
    pub fn fonts(&self) -> &[EmbeddedFont] {
        self.fonts.as_slice()
    }

    /// The bounds the script was read with.
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The index into [`Script::styles`] of the style an event or `\r` names
    /// (`ass_lookup_style`): leading `*` removed, `Default` in any case, the
    /// last style with exactly that name, else the default style.
    pub fn lookup_style(&self, name: &[u8]) -> usize {
        let start = name.iter().position(|&c| c != b'*').unwrap_or(name.len());
        let name = &name[start..];
        let name: &[u8] = if name.eq_ignore_ascii_case(b"Default") {
            b"Default"
        } else {
            name
        };
        self.styles
            .as_slice()
            .iter()
            .rposition(|s| s.name() == name)
            .unwrap_or(self.default_style)
    }

    fn apply_overrides(&mut self, work: &mut WorkBudget) -> Result<()> {
        for i in 0..self.overrides.len() {
            let item = self.overrides.as_slice()[i].clone();
            self.apply_override(&item, work)?;
        }
        Ok(())
    }

    /// libass's `process_text`: lines split at CR and LF, with byte order
    /// marks skipped at line starts, then any pending embedded font decoded.
    fn process_text(&mut self, text: &[u8], work: &mut WorkBudget) -> Result<()> {
        let mut rest = text;
        loop {
            loop {
                if let [b'\r' | b'\n', tail @ ..] = rest {
                    rest = tail;
                } else if let [0xef, 0xbb, 0xbf, tail @ ..] = rest {
                    rest = tail;
                } else {
                    break;
                }
            }
            if rest.is_empty() {
                break;
            }
            let end = rest
                .iter()
                .position(|&c| c == b'\r' || c == b'\n')
                .unwrap_or(rest.len());
            charge(work, end + 1)?;
            self.process_line(&rest[..end], work)?;
            rest = &rest[end..];
        }
        if self.pending_font.is_some() {
            self.decode_font()?;
        }
        Ok(())
    }

    fn process_line(&mut self, line: &[u8], work: &mut WorkBudget) -> Result<()> {
        let line = trim_start(line);
        let header = |name: &[u8]| starts_with_ignore_case(line, name);
        if header(b"[Script Info]") {
            self.section = Section::Info;
        } else if header(b"[V4 Styles]") {
            self.section = Section::Styles;
            self.info.script_type = ScriptType::Ssa;
        } else if header(b"[V4+ Styles]") {
            self.section = Section::Styles;
            self.info.script_type = ScriptType::Ass;
        } else if header(b"[Events]") {
            self.section = Section::Events;
        } else if header(b"[Fonts]") {
            self.section = Section::Fonts;
        } else {
            match self.section {
                Section::Info => self.info_line(line),
                Section::Styles => self.styles_line(line, work)?,
                Section::Events => self.events_line(line, work)?,
                Section::Fonts => self.fonts_line(line)?,
                Section::None => {}
            }
        }
        Ok(())
    }

    fn info_line(&mut self, line: &[u8]) {
        if line.starts_with(b"; Script generated by FFmpeg/Lavc") {
            self.headers.insert(Header::Ffmpeg);
        } else if let Some(header) = self.info.read_line(line) {
            self.headers.insert(header);
        }
    }

    fn styles_line(&mut self, line: &[u8], work: &mut WorkBudget) -> Result<()> {
        if let Some(format) = line.strip_prefix(b"Format:") {
            let format = trim_start(format);
            self.style_format = Some(Bytes::copy_from_slice(&self.budget, format)?);
            let standard = match self.info.script_type {
                ScriptType::Ass => ASS_STYLE_FORMAT,
                _ => SSA_STYLE_FORMAT,
            };
            self.custom_format_compatibility(format, standard);
        } else if let Some(style) = line.strip_prefix(b"Style:") {
            self.style_line(trim_start(style), work)?;
        }
        Ok(())
    }

    /// libass's `process_style`: the fields in format order, normalised.
    fn style_line(&mut self, line: &[u8], work: &mut WorkBudget) -> Result<()> {
        if self.styles.len() >= self.limits.styles {
            return Err(Error::Policy("ass styles"));
        }
        // Without a format line, an ancient script: the dialect's standard one.
        let format = if let Some(format) = &self.style_format {
            format.clone()
        } else {
            let format = match self.info.script_type {
                ScriptType::Ssa => SSA_STYLE_FORMAT,
                _ => ASS_STYLE_FORMAT,
            };
            let format = Bytes::copy_from_slice(&self.budget, format)?;
            self.style_format = Some(format.clone());
            format
        };
        let ssa = self.info.script_type == ScriptType::Ssa;
        let ass = self.info.script_type == ScriptType::Ass;
        let mut style = Style::empty(&self.budget, b"", b"")?;
        style.scale_x = 100.0;
        style.scale_y = 100.0;
        let (mut name, mut font_name) = (None, None);
        let mut ssa_alpha = 0;
        let (mut names, mut values) = (&format[..], line);
        while let Some(field) = next_token(&mut names, true) {
            let Some(token) = next_token(&mut values, false) else {
                break;
            };
            charge(work, 1)?;
            match Field::named(field) {
                Some(Field::Name) => {
                    let start = token.iter().position(|&c| c != b'*').unwrap_or(token.len());
                    name = Some(&token[start..]);
                }
                Some(Field::FontName) => font_name = Some(token),
                Some(Field::AlphaLevel) => ssa_alpha = header_int(token),
                Some(Field::Colour(ColourSlot::Back)) => {
                    style.assign(Field::Colour(ColourSlot::Back), token);
                    // SSA's back colour is its outline colour too.
                    if ssa {
                        style.colours.outline = style.colours.back;
                    }
                }
                Some(Field::Alignment) => {
                    let value = header_int(token);
                    style.alignment = if ass {
                        Alignment::from_numpad(value)
                    } else {
                        // VSFilter reads SSA's 8 and 4 as 3 and 11.
                        Alignment::from_bits(match value {
                            8 => 3,
                            4 => 11,
                            value => value,
                        })
                    };
                }
                Some(Field::Blur | Field::Justify) | None => {}
                Some(field) => style.assign(field, token),
            }
        }
        if ssa {
            style.set_alpha(ssa_alpha, 0x80);
        }
        style.finish_line();
        let name = match name {
            Some(name) if !name.is_empty() => name,
            _ => b"Default",
        };
        style.set_names(&self.budget, name, font_name.unwrap_or(b"Arial"))?;
        if style.name() == b"Default" {
            self.default_style = self.styles.len();
        }
        self.styles.try_push(style).map_err(|e| e.error)
    }

    /// libass's `custom_format_line_compatibility`: a format line other than
    /// the standard one makes `ScaledBorderAndShadow` default to yes.
    fn custom_format_compatibility(&mut self, format: &[u8], standard: &[u8]) {
        if !self.headers.contains(Header::ScaledBorderAndShadow) && !same_format(format, standard) {
            self.info.scaled_border_and_shadow = true;
        }
    }

    fn events_line(&mut self, line: &[u8], work: &mut WorkBudget) -> Result<()> {
        if let Some(format) = line.strip_prefix(b"Format:") {
            let format = trim_start(format);
            self.event_format = Some(Bytes::copy_from_slice(&self.budget, format)?);
            let standard = match self.info.script_type {
                ScriptType::Ass => ASS_EVENT_FORMAT,
                _ => SSA_EVENT_FORMAT,
            };
            self.custom_format_compatibility(format, standard);
            if self.is_legacy_ffmpeg() {
                self.info.scaled_border_and_shadow = true;
            }
        } else if let Some(dialogue) = line.strip_prefix(b"Dialogue:") {
            if self.event_format.is_none() {
                self.event_format_fallback()?;
            }
            if let Some(event) = self.event_tail(trim_start(dialogue), 0, work)? {
                self.push_event(event)?;
            }
        }
        Ok(())
    }

    /// libass's `detect_legacy_conv_subs`: FFmpeg's converted subtitles from
    /// before it wrote `ScaledBorderAndShadow`, which expect it to be yes.
    fn is_legacy_ffmpeg(&self) -> bool {
        self.headers.is_legacy_ffmpeg()
            && self.styles.len() == 2
            && self.styles.as_slice()[1].name().starts_with(b"Default")
    }

    fn event_format_fallback(&mut self) -> Result<()> {
        self.section = Section::Events;
        let format = match self.info.script_type {
            ScriptType::Ssa => SSA_EVENT_FORMAT,
            _ => ASS_EVENT_FORMAT,
        };
        self.event_format = Some(Bytes::copy_from_slice(&self.budget, format)?);
        Ok(())
    }

    fn push_event(&mut self, event: Event) -> Result<()> {
        if self.events.len() >= self.limits.events {
            return Err(Error::Policy("ass events"));
        }
        self.earliest_end = self.earliest_end.min(event.end());
        self.events.try_push(event).map_err(|e| e.error)
    }

    /// libass's `check_duplicate_event`: by the bitmap while it is active,
    /// else by scanning the events held.
    fn is_duplicate(&mut self, read_order: i32, work: &mut WorkBudget) -> Result<bool> {
        if self.read_order.is_active() {
            return Ok(self.read_order.test_and_set(read_order)? == Some(true));
        }
        charge(work, self.events.len())?;
        Ok(self.events().iter().any(|e| e.read_order == read_order))
    }
}

/// libass's `format_line_compare`: the same field names, ignoring ASCII case
/// and blanks around them, with `Actor` read as `Name`.
fn same_format(mut a: &[u8], mut b: &[u8]) -> bool {
    loop {
        match (format_field(&mut a), format_field(&mut b)) {
            (Some(x), Some(y)) if x.eq_ignore_ascii_case(y) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// The next field name of a format line, with `Actor` read as `Name`.
fn format_field<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let token = next_token(rest, true)?;
    Some(if token == b"Actor" { b"Name" } else { token })
}

#[cfg(test)]
mod tests {
    use super::{Options, Script, same_format};
    use triergon_core::Budget;

    #[test]
    fn format_lines_compare_by_field() {
        assert!(same_format(b"Layer,  start ,End", b"Layer, Start, End"));
        assert!(same_format(b"Actor, Text,", b"Name, Text"));
        assert!(!same_format(b"actor, Text", b"Name, Text"));
        assert!(!same_format(b"Layer, Start", b"Layer, Start, End"));
    }

    #[test]
    fn pruning_keeps_what_ends_at_the_deadline() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut script = Script::new(&budget, &Options::default()).unwrap();
        script
            .process_codec_private(b"[Script Info]\nScriptType: v4.00+\n")
            .unwrap();
        script
            .process_chunk(b"1,0,Default,,0,0,0,,a", 0, 100)
            .unwrap();
        script
            .process_chunk(b"2,0,Default,,0,0,0,,b", 50, 100)
            .unwrap();
        script.prune_events(99);
        assert_eq!(script.events().len(), 2);
        script.prune_events(101);
        assert_eq!(script.events().len(), 1);
        // The pruned ReadOrder is free again; the held one is not.
        script
            .process_chunk(b"1,0,Default,,0,0,0,,a", 0, 100)
            .unwrap();
        script
            .process_chunk(b"2,0,Default,,0,0,0,,b", 0, 100)
            .unwrap();
        assert_eq!(script.events().len(), 2);
    }
}
