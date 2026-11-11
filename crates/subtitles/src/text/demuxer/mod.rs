//! SubRip and WebVTT documents read as FFmpeg's `srt` and `webvtt` demuxers
//! read them, re-exported as [`crate::compat::document`].
//!
//! A [`Reader`] takes one whole document and publishes its cues in the order
//! the demuxer returns its packets: by start time, ties in document order.
//! Each cue carries the demuxer's millisecond start and duration, its packet
//! bytes and, for WebVTT, the identifier and settings side data. A cue that
//! ends before it starts lasts until the next cue starts, if there is one.
//! The rules for each syntax are in [`subrip`] and [`webvtt`].

mod blocks;
mod lines;
mod subrip;
mod timestamp;
mod utf16;
mod webvtt;

use blocks::{Block, Blocks};
use lines::{Line, Lines};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// Which demuxer reads the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    /// FFmpeg's `srt` demuxer.
    SubRip,
    /// FFmpeg's `webvtt` demuxer.
    WebVtt,
}

/// Resource bounds of one document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Document bytes, and for a UTF-16 SubRip document its UTF-8 conversion.
    pub document_bytes: usize,
    /// Cues read from the document.
    pub cues: usize,
    /// Bytes of SubRip cue text, which the reader assembles from its lines.
    pub text_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            document_bytes: 16 << 20,
            cues: 1 << 16,
            text_bytes: 16 << 20,
        }
    }
}

/// One packet of the demuxer.
#[derive(Clone, Debug)]
pub struct Cue {
    /// Start in milliseconds.
    pub start_ms: i64,
    /// End minus start in milliseconds; zero and negative values are kept.
    pub duration_ms: i64,
    /// The packet bytes: the cue text, unvalidated.
    pub text: Bytes,
    /// The WebVTT cue identifier.
    pub identifier: Option<Bytes>,
    /// The WebVTT cue settings.
    pub settings: Option<Bytes>,
}

/// What a [`Reader::poll`] produced.
#[derive(Clone, Debug)]
pub enum Status {
    /// The work budget ran out; poll again.
    Yield,
    /// The next cue.
    Cue(Cue),
    /// Every cue has been published.
    Finished,
}

/// A cue found in the document, with its byte ranges in the reader's text.
#[derive(Clone, Debug)]
struct Entry {
    start_ms: i64,
    duration_ms: i64,
    /// Position in the document, which orders cues with the same start.
    order: u32,
    text: Range<usize>,
    identifier: Option<Range<usize>>,
    settings: Option<Range<usize>>,
}

/// The cues read so far, bounded by the limits.
#[derive(Debug)]
struct Entries {
    entries: BudgetVec<Entry>,
    maximum: usize,
}

impl Entries {
    fn push(&mut self, mut entry: Entry) -> Result<()> {
        let count = self.entries.len();
        if count == self.maximum {
            return Err(Error::Limit {
                requested: count + 1,
                maximum: self.maximum,
            });
        }
        entry.order = u32::try_from(count).map_err(|_| Error::Overflow)?;
        self.entries.try_push(entry).map_err(|error| error.error)
    }
}

#[derive(Debug)]
enum Phase {
    /// Converting a UTF-16 SubRip document to UTF-8.
    Decode(utf16::Decoder),
    /// Reading a SubRip document's lines.
    SubRip(Lines, subrip::Parser),
    /// Reading a WebVTT document's blocks.
    WebVtt(Blocks),
    /// Ordering the cues by start time, once `paid` covers its cost.
    Sort {
        paid: u64,
    },
    /// Publishing the next cue.
    Publish(usize),
    Finished,
}

/// Reads one SubRip or WebVTT document into the demuxer's cues.
#[derive(Debug)]
pub struct Reader {
    pool: Budget,
    limits: Limits,
    syntax: Syntax,
    /// The document as the parser reads it: the input, or its conversion.
    source: Bytes,
    phase: Phase,
    entries: Entries,
    /// The bytes cue ranges point into: SubRip text, or the source for WebVTT.
    text: Option<Bytes>,
}

impl Reader {
    /// A reader of `document`, read as `syntax` reads it.
    pub fn new(pool: &Budget, document: Bytes, syntax: Syntax, limits: Limits) -> Result<Self> {
        if document.len() > limits.document_bytes {
            return Err(Error::Limit {
                requested: document.len(),
                maximum: limits.document_bytes,
            });
        }
        let phase = match utf16::Decoder::for_document(pool, &document, syntax, limits) {
            Some(decoder) => Phase::Decode(decoder),
            None => Self::parse_phase(pool, &document, syntax, limits),
        };
        Ok(Self {
            pool: pool.clone(),
            limits,
            syntax,
            source: document,
            phase,
            entries: Entries {
                entries: BudgetVec::new(pool),
                maximum: limits.cues,
            },
            text: None,
        })
    }

    fn parse_phase(pool: &Budget, source: &Bytes, syntax: Syntax, limits: Limits) -> Phase {
        match syntax {
            Syntax::SubRip => Phase::SubRip(
                Lines::new(source.as_ref()),
                subrip::Parser::new(pool, limits.text_bytes),
            ),
            Syntax::WebVtt => Phase::WebVtt(Blocks::new()),
        }
    }

    /// Advances by at most `work` units: one per document byte read or
    /// converted, one per cue compared while sorting and one per cue
    /// published.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match &mut self.phase {
                Phase::Decode(decoder) => {
                    let Some(converted) = decoder.advance(self.source.as_ref(), work)? else {
                        return Ok(Status::Yield);
                    };
                    self.source = converted;
                    self.phase =
                        Self::parse_phase(&self.pool, &self.source, self.syntax, self.limits);
                }
                Phase::SubRip(lines, parser) => {
                    let source = self.source.as_ref();
                    loop {
                        let Some(line) = lines.next(source, work) else {
                            return Ok(Status::Yield);
                        };
                        parser.line(source, line, &mut self.entries)?;
                        if line == Line::End {
                            break;
                        }
                    }
                    if let Phase::SubRip(_, parser) =
                        std::mem::replace(&mut self.phase, Phase::Sort { paid: 0 })
                    {
                        self.text = Some(parser.finish()?);
                    }
                }
                Phase::WebVtt(blocks) => {
                    let source = self.source.as_ref();
                    loop {
                        match blocks.next(source, work) {
                            None => return Ok(Status::Yield),
                            Some(Block::Text(span))
                                if webvtt::block(source, span, &mut self.entries)? => {}
                            Some(_) => break,
                        }
                    }
                    self.text = Some(self.source.clone());
                    self.phase = Phase::Sort { paid: 0 };
                }
                Phase::Sort { paid } => {
                    let entries = self.entries.entries.as_mut_slice();
                    let count = entries.len() as u64;
                    let cost = sort_work(count) + count;
                    *paid += work.take(cost - *paid);
                    if *paid < cost {
                        return Ok(Status::Yield);
                    }
                    entries.sort_unstable_by_key(|entry| (entry.start_ms, entry.order));
                    // A cue that ends before it starts lasts until the next.
                    for at in 1..entries.len() {
                        let next = entries[at].start_ms;
                        let entry = &mut entries[at - 1];
                        if entry.duration_ms < 0 {
                            entry.duration_ms = next - entry.start_ms;
                        }
                    }
                    self.phase = Phase::Publish(0);
                }
                Phase::Publish(next) => {
                    let Some(entry) = self.entries.entries.as_slice().get(*next) else {
                        self.phase = Phase::Finished;
                        continue;
                    };
                    if !work.try_charge(1) {
                        return Ok(Status::Yield);
                    }
                    let text = self.text.as_ref().ok_or(Error::Invalid("cue text"))?;
                    let slice = |range: &Range<usize>| text.slice(range.clone());
                    let cue = Cue {
                        start_ms: entry.start_ms,
                        duration_ms: entry.duration_ms,
                        text: slice(&entry.text)?,
                        identifier: entry.identifier.as_ref().map(slice).transpose()?,
                        settings: entry.settings.as_ref().map(slice).transpose()?,
                    };
                    *next += 1;
                    return Ok(Status::Cue(cue));
                }
                Phase::Finished => return Ok(Status::Finished),
            }
        }
    }
}

/// Comparisons a sort of `count` cues makes at most, `count · ⌈log₂ count⌉`.
fn sort_work(count: u64) -> u64 {
    count * u64::from(u64::BITS - count.saturating_sub(1).leading_zeros())
}
