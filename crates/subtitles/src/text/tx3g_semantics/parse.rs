//! Forward bounded modifier reader; physical box geometry is supplied only by
//! the existing internally executed tx3g extractor/configuration parser.
use super::super::{
    Encoding, Modifier,
    tx3g_config::StyleRecord,
    unicode::{Cursor, Poll, cancelled},
};
use super::{RecordSource, WrapMode};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Header,
    Records,
    Url,
    AltLength,
    Alt,
    Done,
}
pub(super) struct Reader {
    kind: [u8; 4],
    index: usize,
    at: usize,
    end: usize,
    begin: usize,
    body_start: usize,
    phase: Phase,
    buffer: [u8; 12],
    read: usize,
    need: usize,
    count: usize,
    current: usize,
    utf8: Option<Cursor>,
    start: u16,
    finish: u16,
    url: Range<usize>,
    alternate: Range<usize>,
}
impl Reader {
    pub(super) fn new(bytes: &[u8], modifier: &Modifier, index: usize) -> Result<Self> {
        let start = modifier.range.start;
        let short = bytes
            .get(start..start.checked_add(4).ok_or(Error::Overflow)?)
            .ok_or(Error::Truncated)?;
        let size = if short == [0, 0, 0, 1] { 16 } else { 8 };
        let at = start.checked_add(size).ok_or(Error::Overflow)?;
        if at > modifier.range.end || modifier.range.end > bytes.len() {
            return Err(Error::Truncated);
        }
        let need = match &modifier.kind {
            b"styl" => 2,
            b"krok" => 6,
            b"hlit" | b"hclr" | b"dlay" | b"blnk" => 4,
            b"href" => 5,
            b"tbox" => 8,
            b"twrp" => 1,
            b"disp" => 2,
            _ => 0,
        };
        Ok(Self {
            kind: modifier.kind,
            index,
            at,
            end: modifier.range.end,
            begin: at,
            body_start: at,
            phase: if need == 0 {
                Phase::Done
            } else {
                Phase::Header
            },
            buffer: [0; 12],
            read: 0,
            need,
            count: 0,
            current: 0,
            utf8: None,
            start: 0,
            finish: 0,
            url: 0..0,
            alternate: 0..0,
        })
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
            if self.phase == Phase::Done {
                return Ok(Poll::End);
            }
            if work.remaining() == 0 {
                return Ok(Poll::Yield);
            }
            if matches!(self.phase, Phase::Url | Phase::Alt) {
                match self
                    .utf8
                    .as_mut()
                    .ok_or(Error::Invalid("tx3g UTF8 field cursor"))?
                    .poll(bytes, work, cancel)?
                {
                    Poll::Yield => return Ok(Poll::Yield),
                    Poll::Ready(_) => continue,
                    Poll::End => self.utf8 = None,
                }
                if self.phase == Phase::Url {
                    self.phase = Phase::AltLength;
                    self.reset(1);
                    continue;
                }
                self.exact_end()?;
                self.phase = Phase::Done;
                return Ok(Poll::Ready(Item::Hyperlink(
                    self.source(self.body_start),
                    self.start,
                    self.finish,
                    self.url.clone(),
                    self.alternate.clone(),
                )));
            }
            if !self.data(bytes, work, cancel)? {
                return Ok(Poll::Yield);
            }
            if self.phase == Phase::AltLength {
                let end = self
                    .at
                    .checked_add(usize::from(self.buffer[0]))
                    .ok_or(Error::Overflow)?;
                if end > self.end {
                    return Err(Error::Truncated);
                }
                self.alternate = self.at..end;
                self.utf8 = Some(Cursor::new(self.alternate.clone(), Encoding::Utf8));
                self.at = end;
                self.phase = Phase::Alt;
                continue;
            }
            if self.phase == Phase::Records {
                if self.current == self.count {
                    self.exact_end()?;
                    self.phase = Phase::Done;
                    continue;
                }
                let source = self.source(self.begin);
                let item = if self.kind == *b"styl" {
                    Item::Style(
                        source,
                        StyleRecord {
                            start: self.u16(0),
                            end: self.u16(2),
                            font_id: self.u16(4),
                            face_flags: self.buffer[6],
                            font_size: self.buffer[7],
                            rgba: self.buffer[8..12]
                                .try_into()
                                .map_err(|_| Error::Truncated)?,
                        },
                    )
                } else {
                    Item::Karaoke(source, self.u32(0), self.u16(4), self.u16(6))
                };
                self.current += 1;
                self.reset(if self.kind == *b"styl" { 12 } else { 8 });
                if self.current == self.count {
                    self.exact_end()?;
                    self.phase = Phase::Done;
                }
                return Ok(Poll::Ready(item));
            }
            let source = self.source(self.begin);
            let item = match &self.kind {
                b"styl" | b"krok" => {
                    let karaoke = self.kind == *b"krok";
                    self.count = usize::from(self.u16(if karaoke { 4 } else { 0 }));
                    self.current = 0;
                    let count = self
                        .count
                        .checked_mul(if karaoke { 8 } else { 12 })
                        .ok_or(Error::Overflow)?;
                    if self.at.checked_add(count).ok_or(Error::Overflow)? != self.end {
                        return Err(Error::Invalid("tx3g modifier record count/size"));
                    }
                    let header =
                        karaoke.then(|| Item::KaraokeHeader(source, self.u32(0), self.u16(4)));
                    self.phase = if self.count == 0 {
                        Phase::Done
                    } else {
                        Phase::Records
                    };
                    self.reset(if karaoke { 8 } else { 12 });
                    if let Some(item) = header {
                        return Ok(Poll::Ready(item));
                    }
                    continue;
                }
                b"href" => {
                    self.start = self.u16(0);
                    self.finish = self.u16(2);
                    let end = self
                        .at
                        .checked_add(usize::from(self.buffer[4]))
                        .ok_or(Error::Overflow)?;
                    if end > self.end {
                        return Err(Error::Truncated);
                    }
                    self.url = self.at..end;
                    self.utf8 = Some(Cursor::new(self.url.clone(), Encoding::Utf8));
                    self.at = end;
                    self.phase = Phase::Url;
                    continue;
                }
                b"hlit" => Item::Highlight(source, self.u16(0), self.u16(2)),
                b"blnk" => Item::Blink(source, self.u16(0), self.u16(2)),
                b"hclr" => Item::HighlightColour(
                    source,
                    self.buffer[..4].try_into().map_err(|_| Error::Truncated)?,
                ),
                b"dlay" => Item::ScrollDelay(source, self.u32(0)),
                b"tbox" => Item::TextBox(
                    source,
                    [
                        self.u16(0) as i16,
                        self.u16(2) as i16,
                        self.u16(4) as i16,
                        self.u16(6) as i16,
                    ],
                ),
                b"twrp" => Item::Wrap(
                    source,
                    match self.buffer[0] {
                        0 => WrapMode::NoWrap,
                        1 => WrapMode::Soft,
                        _ => return Err(Error::Unsupported("tx3g wrap flag")),
                    },
                ),
                b"disp" => Item::Disparity(source, self.u16(0) as i16),
                _ => return Err(Error::Invalid("tx3g semantic reader dispatch")),
            };
            self.exact_end()?;
            self.phase = Phase::Done;
            return Ok(Poll::Ready(item));
        }
    }
}
