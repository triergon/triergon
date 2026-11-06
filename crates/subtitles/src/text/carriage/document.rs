//! Original resumable document carriage. Raw source Input stays untouched.
//! Native compatibility is checked against native reference rows, never by a
//! post-output text/time/colour normalization step.
use super::clock::{Flavor, timing};
use super::native_read::LineRead;
use super::utf16::{Decoder, LineMark};
use super::{
    Discovery, FormatFacts, Limits, NativeView, Packet, Progress, ProgressKind, SideData, Stream,
};
use crate::text::Input;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

const INVALID_DATA: i32 = -1094995529;
const END_OF_FILE: i32 = -541478725;

pub(crate) struct Parts {
    pub format: Option<FormatFacts>,
    pub stream: Option<Stream>,
    pub packets: BudgetVec<Packet>,
    pub progress: BudgetVec<Progress>,
    pub native_error: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Header,
    BlockStart,
    Identifier,
    SrtTiming,
    Body,
    Skip,
}

#[derive(Debug, Clone, Copy)]
struct Cue {
    start: i64,
    duration: i64,
    source_position: i64,
    identifier: Option<(usize, usize)>,
    settings: Option<(usize, usize)>,
    text_start: Option<usize>,
    text_end: usize,
    lines: usize,
}

#[derive(Debug)]
pub(crate) struct Document {
    pool: Budget,
    limits: Limits,
    discovery: Discovery,
    flavor: Flavor,
    state: State,
    cursor: usize,
    line_start: usize,
    first_line: bool,
    scanned_end: bool,
    identifier: Option<(usize, usize)>,
    cue: Option<Cue>,
    body: BudgetVec<u8>,
    copying: Option<LineRead>,
    commit_position: Option<usize>,
    packets: BudgetVec<Packet>,
    packet_bytes: usize,
    progress: BudgetVec<Progress>,
    progress_cursor: usize,
    open_failed: bool,
    done: bool,
    decoder: Option<Decoder>,
    decoded: Option<Bytes>,
    line_marks: BudgetVec<LineMark>,
    line_number: usize,
    unicode_stopped: bool,
}

impl Document {
    pub(crate) fn new(
        pool: &Budget,
        limits: Limits,
        discovery: Discovery,
        file_name: &Bytes,
    ) -> Self {
        let path = file_name.as_ref();
        let vtt = path.len() >= 4 && path[path.len() - 4..].eq_ignore_ascii_case(b".vtt");
        Self {
            pool: pool.clone(),
            limits,
            discovery,
            flavor: if vtt { Flavor::WebVtt } else { Flavor::SubRip },
            state: State::Header,
            cursor: 0,
            line_start: 0,
            first_line: true,
            scanned_end: false,
            identifier: None,
            cue: None,
            body: BudgetVec::new(pool),
            copying: None,
            commit_position: None,
            packets: BudgetVec::new(pool),
            packet_bytes: 0,
            progress: BudgetVec::new(pool),
            progress_cursor: 0,
            open_failed: false,
            done: false,
            decoder: None,
            decoded: None,
            line_marks: BudgetVec::new(pool),
            line_number: 0,
            unicode_stopped: false,
        }
    }

    fn start_cue(&mut self, line: (usize, usize), clock: (i64, i64, usize), position: i64) {
        self.cue = Some(Cue {
            start: clock.0,
            duration: clock.1,
            source_position: position,
            identifier: self.identifier.take(),
            settings: if clock.2 < line.1 - line.0 {
                Some((line.0 + clock.2, line.1))
            } else {
                None
            },
            text_start: None,
            text_end: line.1,
            lines: 0,
        });
        self.state = State::Body;
    }

    fn line(
        &mut self,
        bytes: &Bytes,
        mut start: usize,
        mut end: usize,
        after: usize,
        position: i64,
    ) -> Result<()> {
        let source = bytes.as_ref();
        if end > start && source[end - 1] == b'\r' {
            end -= 1;
        }
        if self.first_line {
            self.first_line = false;
            if source.get(start..start.saturating_add(3)) == Some(b"\xef\xbb\xbf") {
                start += 3;
            }
            // The filename is an explicit AVFormat caller fact. Byte probing
            // can additionally identify WEBVTT without forcing a format.
            if source.get(start..start.saturating_add(6)) == Some(b"WEBVTT") {
                self.flavor = Flavor::WebVtt;
            }
            if self.flavor == Flavor::WebVtt {
                self.state = State::Header;
                return Ok(());
            }
            self.state = State::BlockStart;
        }
        if self.open_failed {
            return Ok(());
        }
        let bytes = &source[start..end];
        let empty = bytes.is_empty();
        match self.state {
            State::Header | State::Skip => {
                if empty {
                    self.state = State::BlockStart;
                }
            }
            State::BlockStart => {
                if empty {
                    return Ok(());
                }
                if self.flavor == Flavor::SubRip {
                    if bytes.len() <= 20 && bytes.iter().all(u8::is_ascii_digit) {
                        self.state = State::SrtTiming;
                    } else {
                        self.open_failed = self.packets.is_empty();
                        self.state = State::Skip;
                    }
                } else if bytes.starts_with(b"NOTE") || bytes == b"STYLE" || bytes == b"REGION" {
                    self.state = State::Skip;
                } else if let Some(clock) = timing(bytes, self.flavor) {
                    self.start_cue((start, end), clock, position);
                } else {
                    self.identifier = Some((start, end));
                    self.state = State::Identifier;
                }
            }
            State::Identifier | State::SrtTiming => {
                if let Some(clock) = timing(bytes, self.flavor) {
                    self.start_cue((start, end), clock, position);
                } else {
                    self.identifier = None;
                    self.state = State::Skip;
                    if self.flavor == Flavor::SubRip && self.packets.is_empty() {
                        self.open_failed = true;
                    }
                }
            }
            State::Body => {
                if empty {
                    self.commit_position = Some(after);
                    return Ok(());
                }
                if let Some(cue) = self.cue.as_mut() {
                    if cue.text_start.is_none() {
                        cue.text_start = Some(start);
                    }
                    cue.text_end = end;
                    if self.flavor == Flavor::SubRip {
                        self.copying = Some(NativeView::D32.line_read(
                            source,
                            self.decoded.is_some(),
                            start..end,
                            cue.lines != 0,
                        )?);
                    } else {
                        cue.lines += 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn copy_one(&mut self, source: &Bytes) -> Result<()> {
        let Some(mut copy) = self.copying else {
            return Ok(());
        };
        let byte = copy.byte(source.as_ref())?;
        let requested = self.body.len().checked_add(1).ok_or(Error::Overflow)?;
        if requested > self.limits.total_packet_bytes {
            return Err(Error::Limit {
                requested,
                maximum: self.limits.total_packet_bytes,
            });
        }
        self.body.try_push(byte).map_err(|error| error.error)?;
        copy.advance();
        if copy.complete() {
            self.copying = None;
            if let Some(cue) = self.cue.as_mut() {
                cue.lines += 1;
            }
        } else {
            self.copying = Some(copy);
        }
        Ok(())
    }

    fn commit(&mut self, source: &Bytes, after: usize) -> Result<()> {
        let cue = self
            .cue
            .ok_or(Error::Invalid("missing native carriage cue"))?;
        let length = if self.flavor == Flavor::SubRip {
            self.body.len()
        } else {
            cue.text_start.map_or(0, |start| cue.text_end - start)
        };
        let total = self
            .packet_bytes
            .checked_add(length)
            .ok_or(Error::Overflow)?;
        if total > self.limits.total_packet_bytes {
            return Err(Error::Limit {
                requested: total,
                maximum: self.limits.total_packet_bytes,
            });
        }
        if self.packets.len() >= self.limits.packets {
            return Err(Error::Limit {
                requested: self.packets.len() + 1,
                maximum: self.limits.packets,
            });
        }
        if self.packets.len() == self.packets.capacity() {
            // Reserve before consuming the mutable body; geometric growth
            // avoids rebuilding the complete packet list for every cue.
            let target = self
                .packets
                .capacity()
                .saturating_mul(2)
                .max(4)
                .min(self.limits.packets);
            self.packets
                .try_reserve_exact(target - self.packets.len())?;
        }
        let mut side_data = BudgetVec::new(&self.pool);
        side_data.try_reserve_exact(
            usize::from(cue.identifier.is_some()) + usize::from(cue.settings.is_some()),
        )?;
        if self.flavor == Flavor::WebVtt {
            for (native_type, range) in [(16, cue.identifier), (17, cue.settings)] {
                if let Some((start, end)) = range {
                    side_data
                        .try_push(SideData {
                            native_type,
                            bytes: source.slice(start..end)?,
                        })
                        .map_err(|error| error.error)?;
                }
            }
        }
        let data = if self.flavor == Flavor::SubRip {
            let buffer = std::mem::replace(&mut self.body, BudgetVec::new(&self.pool));
            match buffer.freeze() {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.body = error.buffer;
                    return Err(error.error);
                }
            }
        } else {
            source.slice(cue.text_start.unwrap_or(cue.text_end)..cue.text_end)?
        };
        let ordinal = u32::try_from(self.packets.len()).map_err(|_| Error::Overflow)?;
        self.packets
            .try_push(Packet {
                ordinal,
                stream: 0,
                data,
                position_raw: if self.flavor == Flavor::SubRip {
                    cue.source_position
                } else {
                    i64::try_from(after).map_err(|_| Error::Overflow)?
                },
                flags: 1,
                buffer_is_null: false,
                pts_raw: cue.start,
                dts_raw: cue.start,
                duration_raw: cue.duration,
                packet_time_base: [0, 1],
                stream_time_base: [1, 1000],
                side_data,
            })
            .map_err(|error| error.error)?;
        self.packet_bytes = total;
        self.cue = None;
        self.commit_position = None;
        self.identifier = None;
        self.state = State::BlockStart;
        Ok(())
    }

    pub(crate) fn advance(
        &mut self,
        input: &Input,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if self.decoder.is_none() && self.decoded.is_none() {
            let raw = input.payload.as_ref();
            if raw.starts_with(b"\xfe\xff") || raw.starts_with(b"\xff\xfe") {
                self.decoder = Some(Decoder::new(
                    &self.pool,
                    raw.starts_with(b"\xff\xfe"),
                    self.limits.total_packet_bytes,
                ));
            }
        }
        if let Some(decoder) = self.decoder.as_mut() {
            if !decoder.advance(input.payload.as_ref(), work, cancel)? {
                return Ok(false);
            }
            let (bytes, marks, stopped) = decoder.finish()?;
            self.decoded = Some(bytes);
            self.line_marks = marks;
            self.unicode_stopped = stopped;
            self.decoder = None;
        }
        let source = self.decoded.as_ref().unwrap_or(&input.payload).clone();
        while !self.done {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.copying.is_some() {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.copy_one(&source)?;
                continue;
            }
            if let Some(after) = self.commit_position {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.commit(&source, after)?;
                continue;
            }
            if !self.scanned_end {
                if let Some(byte) = source.as_ref().get(self.cursor).copied() {
                    if !work.try_charge(1) {
                        return Ok(false);
                    }
                    let at = self.cursor;
                    if byte != b'\n' && at - self.line_start >= self.limits.line_bytes {
                        return Err(Error::Limit {
                            requested: at - self.line_start + 1,
                            maximum: self.limits.line_bytes,
                        });
                    }
                    self.cursor += 1;
                    if byte == b'\n' {
                        let start = self.line_start;
                        let position = self.line_position(start)?;
                        self.line_start = self.cursor;
                        self.line(&source, start, at, self.cursor, position)?;
                        self.line_number += 1;
                    }
                    continue;
                }
                self.scanned_end = true;
                if self.line_start < self.cursor {
                    let position = self.line_position(self.line_start)?;
                    self.line(&source, self.line_start, self.cursor, self.cursor, position)?;
                    self.line_start = self.cursor;
                    continue;
                }
            }
            if self.cue.is_some() {
                // The native EOF observation for an invalid scalar after the
                // timing line publishes no empty/partial-valid-looking cue.
                // Original source bytes and strict source error remain intact.
                if self.unicode_stopped && self.cue.as_ref().is_some_and(|cue| cue.lines == 0) {
                    self.cue = None;
                    self.state = State::BlockStart;
                } else {
                    self.commit_position = Some(self.cursor);
                    continue;
                }
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let value = if self.open_failed {
                if self.progress_cursor == 0 {
                    Some(Progress {
                        kind: ProgressKind::FormatOpen,
                        ordinal: None,
                        returned: Some(INVALID_DATA),
                    })
                } else {
                    None
                }
            } else {
                match self.progress_cursor {
                    0 => Some(Progress {
                        kind: ProgressKind::FormatOpen,
                        ordinal: None,
                        returned: Some(0),
                    }),
                    1 => Some(Progress {
                        kind: ProgressKind::Discovery,
                        ordinal: None,
                        returned: if self.discovery == Discovery::StreamInfo {
                            Some(0)
                        } else {
                            None
                        },
                    }),
                    index if index - 2 <= self.packets.len() => Some(Progress {
                        kind: ProgressKind::ReadFrame,
                        ordinal: Some(u32::try_from(index - 2).map_err(|_| Error::Overflow)?),
                        returned: Some(if index - 2 == self.packets.len() {
                            END_OF_FILE
                        } else {
                            0
                        }),
                    }),
                    _ => None,
                }
            };
            if let Some(value) = value {
                self.progress.try_push(value).map_err(|error| error.error)?;
                self.progress_cursor += 1;
            } else {
                self.done = true;
            }
        }
        Ok(true)
    }

    fn line_position(&self, utf8: usize) -> Result<i64> {
        let position = if self.decoded.is_some() {
            let mark = self
                .line_marks
                .as_slice()
                .get(self.line_number)
                .ok_or(Error::Invalid("UTF16 line source mark"))?;
            if mark.utf8 != utf8 {
                return Err(Error::Invalid("UTF16 line/source cursor disagreement"));
            }
            // Versioned observation: native packet.pos is one byte beyond the
            // physical line boundary in both original UTF16 BOM cases. This
            // compatibility field does not rewrite independent source ranges.
            mark.source.checked_add(1).ok_or(Error::Overflow)?
        } else {
            utf8
        };
        i64::try_from(position).map_err(|_| Error::Overflow)
    }

    pub(crate) fn finish(self) -> Result<Parts> {
        if !self.done {
            return Err(Error::Invalid("unfinished native carriage document"));
        }
        let format = if self.open_failed {
            None
        } else {
            Some(FormatFacts {
                name: if self.flavor == Flavor::SubRip {
                    "srt"
                } else {
                    "webvtt"
                },
                probe_size: 5_000_000,
                max_analyze_duration: 0,
            })
        };
        let stream = if self.open_failed {
            None
        } else {
            Some(Stream {
                index: 0,
                codec_type: 3,
                codec_id: if self.flavor == Flavor::SubRip {
                    94225
                } else {
                    94226
                },
                codec_name: if self.flavor == Flavor::SubRip {
                    "subrip"
                } else {
                    "webvtt"
                },
                codec_tag: 0,
                time_base: [1, 1000],
                start_raw: i64::MIN,
                duration_raw: i64::MIN,
                disposition: 0,
                extradata: None,
                coded_side_data: BudgetVec::new(&self.pool),
                metadata: BudgetVec::new(&self.pool),
            })
        };
        Ok(Parts {
            format,
            stream,
            packets: self.packets,
            progress: self.progress,
            native_error: if self.open_failed {
                Some(INVALID_DATA)
            } else {
                None
            },
        })
    }
}
