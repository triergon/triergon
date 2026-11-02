//! Forward-only source cue and metadata framing after strict charset validation.
use super::{
    Encoding, Field, FieldOwner, Format, Limits, MetadataKind, clock,
    emit::{Fields, Plan, TextSpan, Times},
    line::{Line, Reader},
    unicode::{Poll, cancelled},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
enum Phase {
    Header,
    Blocks,
    TimingLine,
    Timing,
    Payload,
    Metadata,
}

#[derive(Debug)]
#[cfg_attr(
    target_pointer_width = "64",
    expect(
        clippy::large_enum_variant,
        reason = "Exclusive parse plans move charged owners inline without a new heap allocation; guarded at 272 bytes."
    )
)]
pub(crate) enum Item {
    Metadata(MetadataKind, Range<usize>),
    Cue(Plan),
}

// Physical value ceiling, including enum layout. This is not a heap/RSS bound.
const _: () = assert!(std::mem::size_of::<Item>() <= 272);

#[derive(Debug)]
pub(crate) struct Document {
    reader: Reader,
    phase: Phase,
    comma: bool,
    encoding: Encoding,
    seen_cue: bool,
    cues: usize,
    cue_start: usize,
    text_start: usize,
    text_end: usize,
    decoded_start: usize,
    decoded_end: usize,
    fields: Fields,
    times: Option<Times>,
    clock: Option<clock::Parser>,
    metadata_kind: MetadataKind,
    metadata_start: usize,
    metadata_end: usize,
}

impl Document {
    pub fn new(range: Range<usize>, encoding: Encoding, format: Format) -> Self {
        let comma = format == Format::SubRip;
        Self {
            reader: Reader::new(range, encoding),
            phase: if comma { Phase::Blocks } else { Phase::Header },
            comma,
            encoding,
            seen_cue: false,
            cues: 0,
            cue_start: 0,
            text_start: 0,
            text_end: 0,
            decoded_start: 0,
            decoded_end: 0,
            fields: Fields::default(),
            times: None,
            clock: None,
            metadata_kind: MetadataKind::Note,
            metadata_start: 0,
            metadata_end: 0,
        }
    }

    fn timing_line(&mut self, line: &Line) {
        self.text_start = line.after;
        self.text_end = line.after;
        self.decoded_start = line.decoded_after;
        self.decoded_end = line.decoded_after;
        self.clock = Some(clock::Parser::new(line, self.encoding, self.comma));
        self.phase = Phase::Timing;
    }

    fn cue(&mut self, pool: &Budget, limits: Limits) -> Result<Item> {
        let requested = self.cues.checked_add(1).ok_or(Error::Overflow)?;
        if requested > limits.max_cues {
            return Err(Error::Limit {
                requested,
                maximum: limits.max_cues,
            });
        }
        let decoded_len = self
            .decoded_end
            .checked_sub(self.decoded_start)
            .ok_or(Error::Invalid("cue decoded boundaries"))?;
        self.cues = requested;
        self.seen_cue = true;
        self.phase = Phase::Blocks;
        Ok(Item::Cue(Plan {
            span: TextSpan {
                raw: self.cue_start..self.text_end,
                text: self.text_start..self.text_end,
                decode: self.text_start..self.text_end,
                encoding: self.encoding,
                decoded_len,
            },
            fields: std::mem::take(&mut self.fields),
            times: self
                .times
                .take()
                .ok_or(Error::Invalid("missing parsed cue clock"))?,
            modifiers: BudgetVec::new(pool),
        }))
    }

    pub fn poll(
        &mut self,
        bytes: &[u8],
        pool: &Budget,
        limits: Limits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<Item>> {
        loop {
            cancelled(cancel)?;
            if matches!(self.phase, Phase::Timing) {
                match self
                    .clock
                    .as_mut()
                    .ok_or(Error::Invalid("missing cue clock parser"))?
                    .poll(bytes, work, cancel)?
                {
                    Poll::Yield => return Ok(Poll::Yield),
                    Poll::End => return Err(Error::Invalid("cue clock parser ended")),
                    Poll::Ready(parsed) => {
                        self.fields.settings = parsed.settings;
                        self.times = Some(Times {
                            start: Some(parsed.start),
                            end: Some(parsed.end),
                            duration: Some(parsed.duration),
                        });
                        self.clock = None;
                        self.phase = Phase::Payload;
                        continue;
                    }
                }
            }
            let line = match self
                .reader
                .poll(bytes, limits.max_line_bytes, work, cancel)?
            {
                Poll::Yield => return Ok(Poll::Yield),
                Poll::End => {
                    return match self.phase {
                        Phase::Header => Err(Error::Invalid("missing WebVTT signature")),
                        Phase::TimingLine => Err(Error::Invalid("missing cue timing line")),
                        Phase::Payload => self.cue(pool, limits).map(Poll::Ready),
                        Phase::Metadata => {
                            self.phase = Phase::Blocks;
                            Ok(Poll::Ready(Item::Metadata(
                                self.metadata_kind,
                                self.metadata_start..self.metadata_end,
                            )))
                        }
                        Phase::Blocks => Ok(Poll::End),
                        Phase::Timing => Err(Error::Invalid("unresolved cue clock")),
                    };
                }
                Poll::Ready(line) => line,
            };
            match self.phase {
                Phase::Header => {
                    if !line.signature() {
                        return Err(Error::Invalid("WebVTT signature"));
                    }
                    self.phase = Phase::Blocks;
                    return Ok(Poll::Ready(Item::Metadata(
                        MetadataKind::Header,
                        0..line.end,
                    )));
                }
                Phase::Blocks if line.empty() => {}
                Phase::Blocks => {
                    if !self.comma
                        && (line.note() || line.keyword(b"STYLE") || line.keyword(b"REGION"))
                    {
                        let kind = if line.note() {
                            MetadataKind::Note
                        } else if line.keyword(b"STYLE") {
                            MetadataKind::Style
                        } else {
                            MetadataKind::Region
                        };
                        if self.seen_cue && kind != MetadataKind::Note {
                            return Err(Error::Invalid("WebVTT global block after cue"));
                        }
                        self.metadata_kind = kind;
                        self.metadata_start = line.start;
                        self.metadata_end = line.end;
                        self.phase = Phase::Metadata;
                        continue;
                    }
                    self.cue_start = line.start;
                    self.fields = Fields::default();
                    if self.comma {
                        if !line.digits_only {
                            return Err(Error::Invalid("SubRip sequence identifier"));
                        }
                        self.fields.identifier = Some(Field {
                            owner: FieldOwner::Payload,
                            range: line.range(),
                        });
                        self.phase = Phase::TimingLine;
                    } else if line.has_arrow {
                        self.timing_line(&line);
                    } else {
                        self.fields.identifier = Some(Field {
                            owner: FieldOwner::Payload,
                            range: line.range(),
                        });
                        self.phase = Phase::TimingLine;
                    }
                }
                Phase::TimingLine => self.timing_line(&line),
                Phase::Payload if line.empty() => return self.cue(pool, limits).map(Poll::Ready),
                Phase::Payload => {
                    self.text_end = line.end;
                    self.decoded_end = line.decoded_end;
                }
                Phase::Metadata if line.empty() => {
                    self.phase = Phase::Blocks;
                    return Ok(Poll::Ready(Item::Metadata(
                        self.metadata_kind,
                        self.metadata_start..self.metadata_end,
                    )));
                }
                Phase::Metadata => self.metadata_end = line.end,
                Phase::Timing => return Err(Error::Invalid("cue clock framing phase")),
            }
        }
    }
}
