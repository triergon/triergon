//! Incremental native-view box selection and forward ASS emission.
use super::{
    Clock, Funding, Limits, Options, Output, Subtitle,
    config::{Configuration, Style},
    write::Writer,
};
use crate::text::{Input, unicode::cancelled};
use std::{fmt::Write, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Error, Result, Rounding, TimeBase, WorkBudget};

const INVALID_DATA: i32 = -1094995529;
#[derive(Clone, Copy)]
struct Span {
    start: u16,
    end: u16,
    style: Style,
}
#[derive(Clone, Copy)]
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
    stage: u8,
    font_at: usize,
    color: u32,
    writer: Writer,
}
impl PacketJob {
    pub fn new(input: Input, clock: Clock, pool: &Budget) -> Self {
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
            stage: 0,
            font_at: 0,
            color: 0,
            writer: Writer::new(pool),
        }
    }
    pub fn ready(&self) -> bool {
        matches!(self.phase, Phase::Ready)
    }
    pub fn advances_read_order(&self) -> bool {
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
    pub fn poll(
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
                    if self.at == self.text_end {
                        self.advance = true;
                        self.phase = Phase::Prefix;
                        continue;
                    }
                    if self.bytes()[self.at] == 0 {
                        if !funding.pay(self.text_end - self.at, caller)? {
                            return Ok(false);
                        }
                        self.visible_end = self.at;
                        self.advance = true;
                        self.phase = Phase::Prefix;
                        funding.commit();
                        continue;
                    }
                    let n = utf8_width(self.bytes()[self.at]);
                    if !funding.pay(n, caller)? {
                        return Ok(false);
                    }
                    let valid = self
                        .bytes()
                        .get(self.at..self.at + n)
                        .filter(|_| self.at + n <= self.text_end)
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .and_then(|s| s.chars().next())
                        .is_some_and(|ch| ch != '\u{fffe}');
                    funding.commit();
                    if valid {
                        self.at += n;
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
                    if self.text_at == self.visible_end {
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    if !self.render_transition(config, funding, caller)? {
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
    fn at_start(&self) -> Option<Style> {
        self.styles
            .as_slice()
            .get(self.selected)
            .filter(|s| usize::from(s.start) == self.character)
            .map(|s| s.style)
    }
    fn render_transition(
        &mut self,
        config: &Configuration,
        funding: &mut Funding,
        caller: &mut WorkBudget,
    ) -> Result<bool> {
        if !funding.pay(1, caller)? {
            return Ok(false);
        }
        match self.stage {
            0 => {
                if self
                    .styles
                    .as_slice()
                    .get(self.selected)
                    .is_some_and(|s| usize::from(s.end) == self.character)
                {
                    self.writer.pending.append(b"{\\r}")?;
                    self.color = config.style.color;
                    self.selected += 1;
                }
                self.stage = 1;
            }
            1 => {
                if let Some(s) = self.at_start() {
                    for (bit, tag) in [(1, "b"), (2, "i"), (4, "u")] {
                        if (s.face ^ config.style.face) & bit != 0 {
                            write!(
                                self.writer.pending,
                                "{{\\{}{}}}",
                                tag,
                                u8::from(s.face & bit != 0)
                            )
                            .map_err(|_| Error::Overflow)?;
                        }
                    }
                    if s.size != config.style.size {
                        write!(self.writer.pending, "{{\\fs{}}}", s.size)
                            .map_err(|_| Error::Overflow)?;
                    }
                }
                self.font_at = 0;
                self.stage = 2;
            }
            2 => {
                if let Some(s) = self.at_start().filter(|s| s.font != config.style.font) {
                    if self.font_at < config.fonts.len() {
                        if config.fonts.as_slice()[self.font_at].id == s.font {
                            self.writer.pending.append(b"{\\fn")?;
                            self.writer
                                .pending
                                .append(config.font_name(self.font_at)?)?;
                            self.writer.pending.append(b"}")?;
                        }
                        self.font_at += 1;
                    } else {
                        self.stage = 3;
                    }
                } else {
                    self.stage = 3;
                }
            }
            3 => {
                if let Some(s) = self.at_start() {
                    if s.color != config.style.color {
                        self.color = s.color;
                        write!(self.writer.pending, "{{\\1c&H{:X}&}}", s.color)
                            .map_err(|_| Error::Overflow)?;
                    }
                    if s.alpha != config.style.alpha {
                        write!(self.writer.pending, "{{\\1a&H{:02X}&}}", 255 - s.alpha)
                            .map_err(|_| Error::Overflow)?;
                    }
                }
                self.stage = 4;
            }
            4 => {
                if self
                    .highlight
                    .is_some_and(|(start, _)| usize::from(start) == self.character)
                {
                    if let Some([c_0, c_1, c_2, ..]) = self.highlight_color {
                        write!(
                            self.writer.pending,
                            "{{\\2c&H{c_2:02x}{c_1:02x}{c_0:02x}&}}"
                        )
                        .map_err(|_| Error::Overflow)?;
                    } else {
                        self.writer
                            .pending
                            .append(b"{\\1c&H000000&}{\\2c&HFFFFFF&}")?;
                    }
                }
                self.stage = 5;
            }
            5 => {
                if self
                    .highlight
                    .is_some_and(|(_, end)| usize::from(end) == self.character)
                {
                    if self.highlight_color.is_some() {
                        write!(self.writer.pending, "{{\\2c&H{:X}&}}", config.style.color)
                            .map_err(|_| Error::Overflow)?;
                    } else {
                        write!(
                            self.writer.pending,
                            "{{\\1c&H{:X}&}}{{\\2c&H{:X}&}}",
                            self.color, config.style.color
                        )
                        .map_err(|_| Error::Overflow)?;
                    }
                }
                self.stage = 6;
            }
            6 => {
                let b = self.bytes()[self.text_at];
                let n = utf8_width(b);
                if b == b'\n' {
                    self.writer.pending.append(b"\\N")?;
                } else if b != b'\r' {
                    let mut bytes = [0; 4];
                    bytes[..n].copy_from_slice(&self.bytes()[self.text_at..self.text_at + n]);
                    self.writer.pending.append(&bytes[..n])?;
                }
                self.text_at += n;
                self.character += 1;
                self.stage = 0;
            }
            _ => return Err(Error::Invalid("native ASS emission stage")),
        }
        funding.commit();
        Ok(true)
    }
    pub fn take_output(mut self, options: Options) -> Result<Output> {
        let subtitle = if self.returned > 0 && self.advance {
            let pts = self
                .clock
                .pts
                .filter(|&p| p != i64::MIN)
                .and_then(|pts| options.packet_timebase.map(|b| rescale(pts, b, 1_000_000)))
                .filter(|&p| p != i64::MIN);
            let duration = self
                .clock
                .duration
                .filter(|&d| d != 0)
                .and_then(|d| options.packet_timebase.map(|b| rescale(d, b, 1000)))
                .unwrap_or(0);
            let end = u32::try_from((duration as u64) & u64::from(u32::MAX))
                .map_err(|_| Error::Overflow)?;
            Some(Subtitle {
                pts_microseconds: pts,
                start_display_ms: 0,
                end_display_ms: end,
                ass: self
                    .writer
                    .frozen
                    .take()
                    .ok_or(Error::Invalid("native ASS missing owner"))?,
            })
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
fn utf8_width(b: u8) -> usize {
    match b {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}
fn rescale(ticks: i64, base: TimeBase, denominator: u64) -> i64 {
    TimeBase::new(1, denominator)
        .and_then(|to| base.rescale(ticks, to, Rounding::NearestAway))
        .unwrap_or(i64::MIN)
}
