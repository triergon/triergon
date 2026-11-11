//! SubRip as the `srt` demuxer reads it.
//!
//! A cue starts at each timing line ([`timestamp::subrip`]); lines before the
//! first are ignored. A cue's text is its lines up to the next timing line,
//! joined with LF, with empty lines left out. Cue numbers are not required:
//! the last text line before the next timing line is that cue's number, and
//! is dropped, when it reads as one ([`timestamp::cue_number`]): the whole
//! line if it is the cue's only line, its start otherwise. A cue with no text
//! is no packet. The duration is the end minus the start as a 32-bit `int`.

use super::lines::{Line, Span};
use super::timestamp;
use super::{Entries, Entry};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};

/// The cue being read; its text runs from `text_start` to the end of the
/// parser's text.
#[derive(Debug)]
struct Open {
    start_ms: i64,
    duration_ms: i64,
    text_start: usize,
    /// Where the last line's separator and the line itself start.
    last_line: Option<(usize, usize)>,
}

#[derive(Debug)]
pub(super) struct Parser {
    /// Every cue's text, in document order.
    text: BudgetVec<u8>,
    maximum: usize,
    cue: Option<Open>,
}

impl Parser {
    pub(super) fn new(pool: &Budget, maximum: usize) -> Self {
        Self {
            text: BudgetVec::new(pool),
            maximum,
            cue: None,
        }
    }

    /// Reads one line.
    pub(super) fn line(&mut self, source: &[u8], line: Line, entries: &mut Entries) -> Result<()> {
        let Line::Text(span) = line else {
            return self.close(false, entries);
        };
        let content = span.of(source);
        if let Some((start_ms, end_ms)) = timestamp::subrip(content) {
            self.close(true, entries)?;
            self.cue = Some(Open {
                start_ms,
                // The demuxer keeps the difference in an `int`.
                duration_ms: i64::from(end_ms.wrapping_sub(start_ms) as i32),
                text_start: self.text.len(),
                last_line: None,
            });
        } else if !content.is_empty() && self.cue.is_some() {
            self.append(source, span)?;
        }
        Ok(())
    }

    fn append(&mut self, source: &[u8], span: Span) -> Result<()> {
        let Some(cue) = self.cue.as_mut() else {
            return Ok(());
        };
        let separator = self.text.len();
        let first = separator == cue.text_start;
        let length = separator + usize::from(!first) + (span.end - span.start);
        if length > self.maximum {
            return Err(Error::Limit {
                requested: length,
                maximum: self.maximum,
            });
        }
        if !first {
            self.text.try_extend_from_slice(b"\n")?;
        }
        cue.last_line = Some((separator, self.text.len()));
        self.text.try_extend_from_slice(span.of(source))
    }

    /// Ends the open cue; `numbered` when a timing line follows, whose
    /// number its last line may be.
    fn close(&mut self, numbered: bool, entries: &mut Entries) -> Result<()> {
        let Some(cue) = self.cue.take() else {
            return Ok(());
        };
        if numbered
            && let Some((separator, line)) = cue.last_line
            && timestamp::cue_number(&self.text.as_slice()[line..], line == cue.text_start)
        {
            self.text.truncate(separator);
        }
        if self.text.len() == cue.text_start {
            return Ok(());
        }
        entries.push(Entry {
            start_ms: cue.start_ms,
            duration_ms: cue.duration_ms,
            order: 0,
            text: cue.text_start..self.text.len(),
            identifier: None,
            settings: None,
        })
    }

    /// The text every cue's range points into.
    pub(super) fn finish(self) -> Result<Bytes> {
        self.text.freeze().map_err(|error| error.error)
    }
}
