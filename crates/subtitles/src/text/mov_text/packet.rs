//! Incremental native-view box selection and forward ASS emission.
use super::{
    Clock, Funding, Limits, Options, Output, Subtitle,
    config::{Configuration, Style},
    write::{Fixed, Writer},
};
use crate::text::{Input, unicode::cancelled};
use std::{fmt::Write, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

const INVALID_DATA: i32 = -1094995529;
#[derive(Debug, Clone, Copy)]
struct Span {
    start: u16,
    end: u16,
    style: Style,
}
#[derive(Debug, Clone, Copy)]
enum Phase {
    Begin,
    BoxHeader,
    BoxBody,
    Styles,
    Skip,
    Validate,
    Prefix,
    Render,
    Freeze,
    Ready,
}
/// The emission stages at a character where a style or the highlight starts or
/// ends, in the order the reference writes them; each is one unit of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The end of the selected style; also the stage between characters.
    Reset,
    Face,
    Size,
    Font,
    Colour,
    Highlight,
    Text,
}

#[derive(Debug)]
pub(super) struct PacketJob {
    input: Input,
    clock: Clock,
    phase: Phase,
    returned: i32,
    advance: bool,
    text_end: usize,
    visible_end: usize,
    at: usize,
    box_end: usize,
    box_kind: [u8; 4],
    box_count: usize,
    style_count: usize,
    style_read: usize,
    styles: BudgetVec<Span>,
    highlight: Option<(u16, u16)>,
    highlight_color: Option<[u8; 4]>,
    wrap: Option<u8>,
    text_at: usize,
    character: usize,
    selected: usize,
    stage: Stage,
    font_at: usize,
    color: u32,
    writer: Writer,
}
impl PacketJob {
    pub(super) fn new(input: Input, clock: Clock, pool: &Budget) -> Self {
        Self {
            input,
            clock,
            phase: Phase::Begin,
            returned: 0,
            advance: false,
            text_end: 0,
            visible_end: 0,
            at: 0,
            box_end: 0,
            box_kind: [0; 4],
            box_count: 0,
            style_count: 0,
            style_read: 0,
            styles: BudgetVec::new(pool),
            highlight: None,
            highlight_color: None,
            wrap: None,
            text_at: 2,
            character: 0,
            selected: 0,
            stage: Stage::Reset,
            font_at: 0,
            color: 0,
            writer: Writer::new(pool),
        }
    }
    pub(super) fn ready(&self) -> bool {
        matches!(self.phase, Phase::Ready)
    }
    pub(super) fn advances_read_order(&self) -> bool {
        self.advance
    }
    fn bytes(&self) -> &[u8] {
        self.input.payload.as_ref()
    }
    fn refuse(&mut self, advance: bool) {
        self.returned = INVALID_DATA;
        self.advance = advance;
        self.phase = Phase::Ready;
    }
    fn validation(&mut self) {
        self.at = 2;
        self.visible_end = self.text_end;
        self.phase = Phase::Validate;
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Explicit immutable context and caller funding avoid hidden global state."
    )]
    pub(super) fn poll(
        &mut self,
        config: &Configuration,
        _options: Options,
        limits: Limits,
        read_order: i32,
        funding: &mut Funding,
        caller: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            cancelled(cancel)?;
            match self.phase {
                Phase::Ready => return Ok(true),
                Phase::Begin => {
                    let size = self.bytes().len();
                    if !funding.pay(size.clamp(1, 2), caller)? {
                        return Ok(false);
                    }
                    if size == 0 {
                        self.phase = Phase::Ready;
                    } else if size < 2 {
                        self.refuse(false);
                    } else {
                        let length =
                            usize::from(u16::from_be_bytes([self.bytes()[0], self.bytes()[1]]));
                        if size == 2 {
                            if length == 0 {
                                self.phase = Phase::Ready;
                            } else {
                                self.refuse(false);
                            }
                        } else {
                            self.returned = i32::try_from(size).map_err(|_| Error::Overflow)?;
                            self.text_end = (length + 2).min(size);
                            self.at = self.text_end;
                            self.phase = Phase::BoxHeader;
                        }
                    }
                    funding.commit();
                }
                Phase::BoxHeader => {
                    let remaining = self.bytes().len() - self.at;
                    if remaining < 8 {
                        if !funding.pay(remaining.max(1), caller)? {
                            return Ok(false);
                        }
                        funding.commit();
                        self.validation();
                        continue;
                    }
                    let small = u32::from_be_bytes(
                        self.bytes()[self.at..self.at + 4]
                            .try_into()
                            .map_err(|_| Error::Truncated)?,
                    );
                    let header = if small == 1 { 16 } else { 8 };
                    if remaining < header {
                        if !funding.pay(remaining, caller)? {
                            return Ok(false);
                        }
                        funding.commit();
                        self.validation();
                        continue;
                    }
                    if !funding.pay(header, caller)? {
                        return Ok(false);
                    }
                    let size = if header == 16 {
                        u64::from_be_bytes(
                            self.bytes()[self.at + 8..self.at + 16]
                                .try_into()
                                .map_err(|_| Error::Truncated)?,
                        )
                    } else {
                        u64::from(small)
                    };
                    if size < header as u64 {
                        funding.commit();
                        self.refuse(false);
                        continue;
                    }
                    if size > remaining as u64 {
                        funding.commit();
                        self.validation();
                        continue;
                    }
                    if self.box_count >= limits.maximum_boxes {
                        return Err(Error::Limit {
                            requested: self.box_count.saturating_add(1),
                            maximum: limits.maximum_boxes,
                        });
                    }
                    self.box_kind = self.bytes()[self.at + 4..self.at + 8]
                        .try_into()
                        .map_err(|_| Error::Truncated)?;
                    self.box_end = self.at + usize::try_from(size).map_err(|_| Error::Overflow)?;
                    self.at += header;
                    self.box_count += 1;
                    self.phase = Phase::BoxBody;
                    funding.commit();
                }
                Phase::BoxBody => {
                    let available = self.box_end - self.at;
                    let needed = match &self.box_kind {
                        b"styl" => 2,
                        b"hlit" | b"hclr" => 4,
                        b"twrp" => 1,
                        _ => 0,
                    };
                    if needed == 0 || available < needed {
                        self.phase = Phase::Skip;
                        continue;
                    }
                    if !funding.pay(needed, caller)? {
                        return Ok(false);
                    }
                    let bytes = &self.bytes()[self.at..self.at + needed];
                    match &self.box_kind {
                        b"styl" => {
                            let count = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
                            if count
                                .checked_mul(12)
                                .and_then(|n| n.checked_add(2))
                                .is_some_and(|n| n <= available)
                            {
                                if count > limits.maximum_styles {
                                    return Err(Error::Limit {
                                        requested: count,
                                        maximum: limits.maximum_styles,
                                    });
                                }
                                self.styles.clear();
                                self.style_count = count;
                                self.style_read = 0;
                                self.phase = Phase::Styles;
                            } else {
                                self.phase = Phase::Skip;
                            }
                        }
                        b"hlit" => {
                            self.highlight = Some((
                                u16::from_be_bytes([bytes[0], bytes[1]]),
                                u16::from_be_bytes([bytes[2], bytes[3]]),
                            ));
                            self.phase = Phase::Skip;
                        }
                        b"hclr" => {
                            self.highlight_color =
                                Some(bytes.try_into().map_err(|_| Error::Truncated)?);
                            self.phase = Phase::Skip;
                        }
                        b"twrp" => {
                            self.wrap = Some(bytes[0]);
                            self.phase = Phase::Skip;
                        }
                        _ => return Err(Error::Invalid("native mov_text box dispatch")),
                    }
                    self.at += needed;
                    funding.commit();
                }
                Phase::Styles => {
                    if self.style_read == self.style_count {
                        self.phase = Phase::Skip;
                        continue;
                    }
                    let moving = if self.styles.len() == self.styles.capacity() {
                        self.styles
                            .len()
                            .checked_mul(size_of::<Span>())
                            .ok_or(Error::Overflow)?
                    } else {
                        0
                    };
                    if !funding.pay(12usize.checked_add(moving).ok_or(Error::Overflow)?, caller)? {
                        return Ok(false);
                    }
                    let b = &self.bytes()[self.at..self.at + 12];
                    let span = Span {
                        start: u16::from_be_bytes([b[0], b[1]]),
                        end: u16::from_be_bytes([b[2], b[3]]),
                        style: Style::read(&b[4..12]),
                    };
                    let previous = self.styles.as_slice().last().copied();
                    if span.end < span.start || previous.is_some_and(|p| span.start < p.end) {
                        self.styles.clear();
                        self.phase = Phase::Skip;
                    } else if span.start != span.end && span.style != config.style {
                        if previous.is_some_and(|p| p.end == span.start && p.style == span.style) {
                            self.styles
                                .as_mut_slice()
                                .last_mut()
                                .ok_or(Error::Invalid("native style merge"))?
                                .end = span.end;
                        } else {
                            self.styles.try_push(span).map_err(|e| e.error)?;
                        }
                    }
                    self.style_read += 1;
                    self.at += 12;
                    funding.commit();
                }
                Phase::Skip => {
                    if !funding.pay((self.box_end - self.at).max(1), caller)? {
                        return Ok(false);
                    }
                    self.at = self.box_end;
                    self.phase = Phase::BoxHeader;
                    funding.commit();
                }
                Phase::Validate => {
                    // The visible text runs to the first NUL; it must be UTF-8
                    // without U+FFFE.
                    if !funding.pay((self.text_end - self.at).max(1), caller)? {
                        return Ok(false);
                    }
                    funding.commit();
                    let text = &self.bytes()[self.at..self.text_end];
                    let visible = text.iter().position(|&b| b == 0).unwrap_or(text.len());
                    if is_valid_text(&text[..visible]) {
                        self.visible_end = self.at + visible;
                        self.advance = true;
                        self.phase = Phase::Prefix;
                    } else {
                        self.refuse(true);
                    }
                }
                Phase::Prefix => {
                    if !funding.pay(1, caller)? {
                        return Ok(false);
                    }
                    write!(self.writer.pending, "{read_order},0,Default,,0,0,0,,")
                        .map_err(|_| Error::Overflow)?;
                    if self.text_end > 2
                        && let Some(wrap) = self.wrap
                    {
                        self.writer.pending.append(if wrap == 1 {
                            b"{\\q1}"
                        } else {
                            b"{\\q2}"
                        })?;
                    }
                    self.color = config.style.color;
                    // The prefix and the text, as most events are; tags and
                    // line breaks grow it further.
                    self.writer.reserve(
                        64 + (self.visible_end - self.text_at).min(limits.maximum_ass_bytes),
                    )?;
                    self.phase = Phase::Render;
                    funding.commit();
                }
                Phase::Render => {
                    if !self
                        .writer
                        .drain(limits.maximum_ass_bytes, funding, caller)?
                    {
                        return Ok(false);
                    }
                    let boundary = self.next_boundary();
                    let transition = self.stage != Stage::Reset || boundary == self.character;
                    // A NUL ends the visible text after the transitions at
                    // it; the end of the text has none.
                    if self.text_at == self.visible_end
                        && !(transition && self.visible_end < self.text_end)
                    {
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    if transition {
                        if !self.render_transition(config, limits, funding, caller)? {
                            return Ok(false);
                        }
                    } else if !self.render_run(boundary, limits, funding, caller)? {
                        return Ok(false);
                    }
                }
                Phase::Freeze => {
                    if !self.writer.freeze(funding, caller)? {
                        return Ok(false);
                    }
                    self.phase = Phase::Ready;
                }
            }
        }
    }
    /// The first character from the current one at which a style or the
    /// highlight starts or ends.
    fn next_boundary(&self) -> usize {
        let style = self
            .styles
            .as_slice()
            .get(self.selected)
            .map(|s| [s.start, s.end]);
        let highlight = self.highlight.map(|(start, end)| [start, end]);
        style
            .into_iter()
            .chain(highlight)
            .flatten()
            .map(usize::from)
            .filter(|&at| at >= self.character)
            .min()
            .unwrap_or(usize::MAX)
    }
    /// Writes the characters before `boundary`, where no tag changes: one
    /// unit for each emission stage and output byte of every character. A
    /// line break is one step; the characters between line breaks are copied
    /// as they are, up to [`RUN_BYTES`] bytes a step.
    fn render_run(
        &mut self,
        boundary: usize,
        limits: Limits,
        funding: &mut Funding,
        caller: &mut WorkBudget,
    ) -> Result<bool> {
        let bytes = self.input.payload.as_ref();
        while self.character < boundary && self.text_at < self.visible_end {
            let rest = &bytes[self.text_at..self.visible_end];
            let (output, length, count): (&[u8], usize, usize) = match rest[0] {
                b'\n' => (b"\\N", 1, 1),
                b'\r' => (b"", 1, 1),
                _ => {
                    let most = boundary - self.character;
                    let (mut length, mut count) = (0, 0);
                    for &b in rest {
                        // Each character ends the step at its first byte;
                        // the visible text is UTF-8, so none is split.
                        if b & 0xc0 != 0x80 {
                            if count == most || b == b'\n' || b == b'\r' || length >= RUN_BYTES {
                                break;
                            }
                            count += 1;
                        }
                        length += 1;
                    }
                    (&rest[..length], length, count)
                }
            };
            if !funding.pay(7 * count + output.len(), caller)? {
                return Ok(false);
            }
            self.writer.push(output, limits.maximum_ass_bytes)?;
            funding.commit();
            self.text_at += length;
            self.character += count;
        }
        Ok(true)
    }
    fn at_start(&self) -> Option<Style> {
        self.styles
            .as_slice()
            .get(self.selected)
            .filter(|s| usize::from(s.start) == self.character)
            .map(|s| s.style)
    }
    /// Writes one [`Stage`] at a character where a style or the highlight
    /// starts or ends, in this order, each tag in its own braces:
    ///
    /// - `{\r}` where the selected style ends. The colour a highlight end
    ///   restores returns to the default's.
    /// - Where a style starts, a tag for each property that differs from the
    ///   default style (the sample description's, or all zeros without one):
    ///   `{\b1}` or `{\b0}`, then `{\i…}` and `{\u…}`, for each face flag that
    ///   differs; `{\fs22}`; `{\fnName}` for every font table entry with the
    ///   style's font ID, in table order (`{\fnC}{\fnD}` for two entries with
    ///   one ID, `{\fn}` for an empty name, nothing for an ID the table lacks,
    ///   the name's bytes up to a NUL, unescaped); `{\1c&H563412&}` for RGB
    ///   12 34 56, BGR in uppercase hex without leading zeros (`&HA&`,
    ///   `&H0&`); `{\1a&H87&}` for alpha 0x78, 255 − alpha in two uppercase
    ///   digits (`&H00&`, `&H0F&`).
    /// - Where the highlight starts, `{\2c&Hccbbaa&}` for a highlight colour
    ///   AA BB CC (BGR in six lowercase digits, its alpha unused), or else
    ///   `{\1c&H000000&}{\2c&HFFFFFF&}`. Where it ends, `{\2c&HFFFFFF&}` with
    ///   the default colour, or else `{\1c&H332211&}{\2c&HFFFFFF&}` with the
    ///   colour of the style in effect and the default colour, both as `\1c`
    ///   writes them. An inverted range ends at its end offset and starts at
    ///   its start offset; an empty one starts and ends at the same character.
    /// - The character itself, unless it is the NUL that ends the visible
    ///   text, where the rendering ends.
    ///
    /// A font name that is not UTF-8 or holds U+FFFE refuses the sample with
    /// -1094995529, as such text does; the call still takes a read order.
    /// U+FFFF is written. Each stage, and each font table entry it examines,
    /// is funded on its own: one unit and one per byte it writes.
    fn render_transition(
        &mut self,
        config: &Configuration,
        limits: Limits,
        funding: &mut Funding,
        caller: &mut WorkBudget,
    ) -> Result<bool> {
        let default = config.style;
        let mut tags = Fixed::<64>::new();
        loop {
            let starting = self.at_start();
            let ends = self
                .styles
                .as_slice()
                .get(self.selected)
                .is_some_and(|span| usize::from(span.end) == self.character);
            tags.clear();
            // While the font stage has entries to examine: whether this one
            // has the starting style's font ID.
            let mut font = None;
            let mut name = None;
            let mut ascii = true;
            let mut width = 0;
            match self.stage {
                Stage::Reset => {
                    if ends {
                        tags.append(b"{\\r}")?;
                    }
                }
                Stage::Face => {
                    if let Some(style) = starting {
                        for (flag, tag) in [(1, b'b'), (2, b'i'), (4, b'u')] {
                            if (style.face ^ default.face) & flag != 0 {
                                let on = b'0' + u8::from(style.face & flag != 0);
                                tags.append(&[b'{', b'\\', tag, on, b'}'])?;
                            }
                        }
                    }
                }
                Stage::Size => {
                    if let Some(style) = starting.filter(|style| style.size != default.size) {
                        tags.append(b"{\\fs")?;
                        decimal(&mut tags, style.size)?;
                        tags.append(b"}")?;
                    }
                }
                Stage::Font => {
                    font = starting
                        .filter(|style| style.font != default.font)
                        .and_then(|style| {
                            let font = config.fonts.as_slice().get(self.font_at)?;
                            Some(font.id == style.font)
                        });
                    if font == Some(true) {
                        let found;
                        (found, ascii) = config.font_name(self.font_at)?;
                        name = Some(found);
                    }
                }
                Stage::Colour => {
                    if let Some(style) = starting {
                        if style.color != default.color {
                            hex_tag(&mut tags, b"{\\1c&H", style.color, 1, UPPER)?;
                        }
                        if style.alpha != default.alpha {
                            let alpha = u32::from(255 - style.alpha);
                            hex_tag(&mut tags, b"{\\1a&H", alpha, 2, UPPER)?;
                        }
                    }
                }
                Stage::Highlight => {
                    if let Some((start, end)) = self.highlight {
                        if usize::from(start) == self.character {
                            match self.highlight_color {
                                Some([r, g, b, _]) => {
                                    let bgr = u32::from_le_bytes([r, g, b, 0]);
                                    hex_tag(&mut tags, b"{\\2c&H", bgr, 6, LOWER)?;
                                }
                                None => tags.append(b"{\\1c&H000000&}{\\2c&HFFFFFF&}")?,
                            }
                        }
                        if usize::from(end) == self.character {
                            if self.highlight_color.is_none() {
                                hex_tag(&mut tags, b"{\\1c&H", self.color, 1, UPPER)?;
                            }
                            hex_tag(&mut tags, b"{\\2c&H", default.color, 1, UPPER)?;
                        }
                    }
                }
                Stage::Text => {
                    if self.text_at < self.visible_end {
                        let output;
                        (output, width) = written(self.input.payload.as_ref(), self.text_at);
                        tags.append(output)?;
                    }
                }
            }
            let parts: [&[u8]; 4] = match name {
                Some(name) => [tags.as_bytes(), b"{\\fn", name, b"}"],
                None => [tags.as_bytes(), &[], &[], &[]],
            };
            let size = parts.iter().map(|part| part.len()).sum::<usize>();
            if !funding.pay(1 + size, caller)? {
                return Ok(false);
            }
            if name.is_some_and(|name| !ascii && !is_valid_text(name)) {
                funding.commit();
                self.refuse(true);
                return Ok(true);
            }
            if size > 0 {
                self.writer
                    .push_parts(&parts, size, limits.maximum_ass_bytes)?;
            }
            self.stage = match self.stage {
                Stage::Reset => {
                    if ends {
                        self.color = default.color;
                        self.selected += 1;
                    }
                    Stage::Face
                }
                Stage::Face => Stage::Size,
                Stage::Size => Stage::Font,
                Stage::Font if font.is_some() => {
                    self.font_at += 1;
                    Stage::Font
                }
                Stage::Font => {
                    self.font_at = 0;
                    Stage::Colour
                }
                Stage::Colour => {
                    if let Some(style) = starting {
                        self.color = style.color;
                    }
                    Stage::Highlight
                }
                Stage::Highlight => Stage::Text,
                Stage::Text => {
                    if self.text_at == self.visible_end {
                        self.phase = Phase::Freeze;
                    } else {
                        self.text_at += width;
                        self.character += 1;
                    }
                    Stage::Reset
                }
            };
            funding.commit();
            if self.stage == Stage::Reset {
                return Ok(true);
            }
        }
    }
    pub(super) fn take_output(mut self, options: Options) -> Result<Output> {
        let subtitle = if self.returned > 0 && self.advance {
            let ass = self
                .writer
                .frozen
                .take()
                .ok_or(Error::Invalid("native ASS missing owner"))?;
            Some(Subtitle::at(ass, self.clock, options.packet_timebase)?)
        } else {
            None
        };
        Ok(Output {
            input: self.input,
            clock: self.clock,
            returned: self.returned,
            subtitle,
        })
    }
}
/// The most text bytes [`PacketJob::render_run`] copies in one funded step.
const RUN_BYTES: usize = 4096;
const UPPER: &[u8; 16] = b"0123456789ABCDEF";
const LOWER: &[u8; 16] = b"0123456789abcdef";
/// Appends `prefix`, `value` in hexadecimal with at least `width` digits, and
/// `&}`.
fn hex_tag(
    tags: &mut Fixed<64>,
    prefix: &[u8],
    value: u32,
    width: u32,
    digits: &[u8; 16],
) -> Result<()> {
    tags.append(prefix)?;
    let count = (u32::BITS - value.leading_zeros()).div_ceil(4).max(width);
    for shift in (0..count).rev() {
        tags.append(&[digits[(value >> (4 * shift) & 0xf) as usize]])?;
    }
    tags.append(b"&}")
}
/// Appends `value` in decimal.
fn decimal(tags: &mut Fixed<64>, value: u8) -> Result<()> {
    if value >= 100 {
        tags.append(&[b'0' + value / 100])?;
    }
    if value >= 10 {
        tags.append(&[b'0' + value / 10 % 10])?;
    }
    tags.append(&[b'0' + value % 10])
}
/// Whether text the reference writes is accepted: UTF-8 without U+FFFE.
fn is_valid_text(bytes: &[u8]) -> bool {
    bytes.is_ascii() || std::str::from_utf8(bytes).is_ok_and(|text| !text.contains('\u{fffe}'))
}
/// The output of the character at `at` and its width in bytes: a line feed
/// writes `\N` and a carriage return nothing.
fn written(bytes: &[u8], at: usize) -> (&[u8], usize) {
    let width = utf8_width(bytes[at]);
    let output: &[u8] = match bytes[at] {
        b'\n' => b"\\N",
        b'\r' => b"",
        _ => &bytes[at..at + width],
    };
    (output, width)
}
fn utf8_width(b: u8) -> usize {
    match b {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}
