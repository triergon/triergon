//! Reads the records of one modifier box forward, a byte at a time; the box
//! ranges come from the tx3g extractor or the description parser.
use super::super::{
    Encoding, Modifier,
    tx3g_config::StyleRecord,
    unicode::{Cursor, Poll},
};
use super::{RecordSource, WrapMode};
use crate::cancelled;
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Error, Result, WorkBudget};

#[derive(Clone, Debug)]
pub(super) enum Item {
    Style(RecordSource, StyleRecord),
    Highlight(RecordSource, u16, u16),
    HighlightColour(RecordSource, [u8; 4]),
    KaraokeHeader(RecordSource, u32, u16),
    Karaoke(RecordSource, u32, u16, u16),
    ScrollDelay(RecordSource, u32),
    Hyperlink(RecordSource, u16, u16, Range<usize>, Range<usize>),
    TextBox(RecordSource, [i16; 4]),
    Blink(RecordSource, u16, u16),
    Wrap(RecordSource, WrapMode),
    Disparity(RecordSource, i16),
}
/// The modifier boxes the reader resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Style,
    Karaoke,
    Highlight,
    HighlightColour,
    ScrollDelay,
    Hyperlink,
    TextBox,
    Blink,
    Wrap,
    Disparity,
}
impl Kind {
    fn of(kind: [u8; 4]) -> Option<Self> {
        Some(match &kind {
            b"styl" => Self::Style,
            b"krok" => Self::Karaoke,
            b"hlit" => Self::Highlight,
            b"hclr" => Self::HighlightColour,
            b"dlay" => Self::ScrollDelay,
            b"href" => Self::Hyperlink,
            b"tbox" => Self::TextBox,
            b"blnk" => Self::Blink,
            b"twrp" => Self::Wrap,
            b"disp" => Self::Disparity,
            _ => return None,
        })
    }
    /// The bytes of the fields before any records or strings.
    fn header(self) -> usize {
        match self {
            Self::Style | Self::Disparity => 2,
            Self::Karaoke => 6,
            Self::Highlight | Self::HighlightColour | Self::ScrollDelay | Self::Blink => 4,
            Self::Hyperlink => 5,
            Self::TextBox => 8,
            Self::Wrap => 1,
        }
    }
}
#[derive(Debug)]
enum Phase {
    Header,
    Records { left: usize },
    Url(Cursor),
    AltLength,
    Alt(Cursor),
    Done,
}
#[derive(Debug)]
pub(super) struct Reader {
    kind: Kind,
    index: usize,
    at: usize,
    end: usize,
    begin: usize,
    body_start: usize,
    phase: Phase,
    buffer: [u8; 12],
    read: usize,
    need: usize,
    start: u16,
    finish: u16,
    url: Range<usize>,
    alternate: Range<usize>,
}
impl Reader {
    /// The reader of `modifier`, the box at `index`; `None` for a box it does
    /// not resolve. `Error::Truncated` when the box does not fit `bytes`.
    pub(super) fn new(bytes: &[u8], modifier: &Modifier, index: usize) -> Result<Option<Self>> {
        let start = modifier.range.start;
        let short = bytes
            .get(start..start.checked_add(4).ok_or(Error::Overflow)?)
            .ok_or(Error::Truncated)?;
        let size = if short == [0, 0, 0, 1] { 16 } else { 8 };
        let at = start.checked_add(size).ok_or(Error::Overflow)?;
        if at > modifier.range.end || modifier.range.end > bytes.len() {
            return Err(Error::Truncated);
        }
        let Some(kind) = Kind::of(modifier.kind) else {
            return Ok(None);
        };
        Ok(Some(Self {
            kind,
            index,
            at,
            end: modifier.range.end,
            begin: at,
            body_start: at,
            phase: Phase::Header,
            buffer: [0; 12],
            read: 0,
            need: kind.header(),
            start: 0,
            finish: 0,
            url: 0..0,
            alternate: 0..0,
        }))
    }
    fn source(&self, start: usize) -> RecordSource {
        RecordSource {
            modifier: self.index,
            bytes: start..self.at,
        }
    }
    fn u16(&self, at: usize) -> u16 {
        u16::from_be_bytes([self.buffer[at], self.buffer[at + 1]])
    }
    fn u32(&self, at: usize) -> u32 {
        u32::from_be_bytes([
            self.buffer[at],
            self.buffer[at + 1],
            self.buffer[at + 2],
            self.buffer[at + 3],
        ])
    }
    fn reset(&mut self, need: usize) {
        self.read = 0;
        self.need = need;
        self.begin = self.at;
    }
    fn exact_end(&self) -> Result<()> {
        if self.at == self.end {
            Ok(())
        } else {
            Err(Error::Invalid("tx3g modifier trailing bytes"))
        }
    }
    fn data(&mut self, bytes: &[u8], work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        while self.read < self.need {
            cancelled(cancel)?;
            if self.at == self.end {
                return Err(Error::Truncated);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            self.buffer[self.read] = *bytes.get(self.at).ok_or(Error::Truncated)?;
            self.at += 1;
            self.read += 1;
        }
        Ok(true)
    }
    pub(super) fn poll(
        &mut self,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<Item>> {
        loop {
            cancelled(cancel)?;
            if let Phase::Done = self.phase {
                return Ok(Poll::End);
            }
            if work.remaining() == 0 {
                return Ok(Poll::Yield);
            }
            if let Phase::Url(cursor) | Phase::Alt(cursor) = &mut self.phase {
                match cursor.poll(bytes, work, cancel)? {
                    Poll::Yield => return Ok(Poll::Yield),
                    Poll::Ready(_) => continue,
                    Poll::End => {}
                }
                if let Phase::Url(_) = self.phase {
                    self.phase = Phase::AltLength;
                    self.reset(1);
                    continue;
                }
                return self
                    .finish(Item::Hyperlink(
                        self.source(self.body_start),
                        self.start,
                        self.finish,
                        self.url.clone(),
                        self.alternate.clone(),
                    ))
                    .map(Poll::Ready);
            }
            if !self.data(bytes, work, cancel)? {
                return Ok(Poll::Yield);
            }
            match self.phase {
                Phase::Records { left } => return self.record(left).map(Poll::Ready),
                Phase::AltLength => {
                    self.alternate = self.string_range(self.buffer[0])?;
                    self.phase = Phase::Alt(Cursor::new(self.alternate.clone(), Encoding::Utf8));
                }
                _ => {
                    if let Some(item) = self.header()? {
                        return Ok(Poll::Ready(item));
                    }
                }
            }
        }
    }

    fn finish(&mut self, item: Item) -> Result<Item> {
        self.exact_end()?;
        self.phase = Phase::Done;
        Ok(item)
    }

    fn string_range(&mut self, length: u8) -> Result<Range<usize>> {
        let end = self
            .at
            .checked_add(usize::from(length))
            .ok_or(Error::Overflow)?;
        if end > self.end {
            return Err(Error::Truncated);
        }
        let range = self.at..end;
        self.at = end;
        Ok(range)
    }

    fn record(&mut self, left: usize) -> Result<Item> {
        let source = self.source(self.begin);
        let item = if self.kind == Kind::Style {
            Item::Style(
                source,
                StyleRecord {
                    start: self.u16(0),
                    end: self.u16(2),
                    font_id: self.u16(4),
                    face_flags: self.buffer[6],
                    font_size: self.buffer[7],
                    rgba: [
                        self.buffer[8],
                        self.buffer[9],
                        self.buffer[10],
                        self.buffer[11],
                    ],
                },
            )
        } else {
            Item::Karaoke(source, self.u32(0), self.u16(4), self.u16(6))
        };
        if left == 1 {
            return self.finish(item);
        }
        self.phase = Phase::Records { left: left - 1 };
        self.reset(self.need);
        Ok(item)
    }

    fn header(&mut self) -> Result<Option<Item>> {
        let source = self.source(self.begin);
        let item = match self.kind {
            Kind::Style | Kind::Karaoke => {
                let karaoke = self.kind == Kind::Karaoke;
                let count = self.u16(if karaoke { 4 } else { 0 });
                let size = if karaoke { 8 } else { 12 };
                let left = usize::from(count);
                let end = self.at.checked_add(left * size).ok_or(Error::Overflow)?;
                if end != self.end {
                    return Err(Error::Invalid("tx3g modifier record count/size"));
                }
                self.phase = if left == 0 {
                    Phase::Done
                } else {
                    Phase::Records { left }
                };
                self.reset(size);
                return Ok(karaoke.then(|| Item::KaraokeHeader(source, self.u32(0), count)));
            }
            Kind::Hyperlink => {
                self.start = self.u16(0);
                self.finish = self.u16(2);
                self.url = self.string_range(self.buffer[4])?;
                self.phase = Phase::Url(Cursor::new(self.url.clone(), Encoding::Utf8));
                return Ok(None);
            }
            Kind::Highlight => Item::Highlight(source, self.u16(0), self.u16(2)),
            Kind::Blink => Item::Blink(source, self.u16(0), self.u16(2)),
            Kind::HighlightColour => Item::HighlightColour(
                source,
                [
                    self.buffer[0],
                    self.buffer[1],
                    self.buffer[2],
                    self.buffer[3],
                ],
            ),
            Kind::ScrollDelay => Item::ScrollDelay(source, self.u32(0)),
            Kind::TextBox => Item::TextBox(
                source,
                [
                    self.u16(0) as i16,
                    self.u16(2) as i16,
                    self.u16(4) as i16,
                    self.u16(6) as i16,
                ],
            ),
            Kind::Wrap => Item::Wrap(
                source,
                match self.buffer[0] {
                    0 => WrapMode::NoWrap,
                    1 => WrapMode::Soft,
                    _ => return Err(Error::Unsupported("tx3g wrap flag")),
                },
            ),
            Kind::Disparity => Item::Disparity(source, self.u16(0) as i16),
        };
        self.finish(item).map(Some)
    }
}
