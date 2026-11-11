//! The lines of a SubRip document as the `srt` demuxer splits it.
//!
//! Lines end at LF, CR LF or a lone CR, and a UTF-8 byte-order mark at the
//! start is skipped. A NUL byte ends the document, and the line it interrupts
//! is lost. A last line without a terminator is a line.

use triergon_core::WorkBudget;

/// The byte range of one line or block, without its terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Span {
    pub(super) start: usize,
    pub(super) end: usize,
}

impl Span {
    pub(super) fn of(self, source: &[u8]) -> &[u8] {
        &source[self.start..self.end]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Line {
    Text(Span),
    /// The document ended; every later call returns this again.
    End,
}

#[derive(Debug)]
pub(super) struct Lines {
    /// Start of the line being scanned.
    start: usize,
    /// Next byte to scan.
    cursor: usize,
    ended: bool,
}

const BYTE_ORDER_MARK: &[u8] = b"\xef\xbb\xbf";

impl Lines {
    pub(super) fn new(source: &[u8]) -> Self {
        let start = if source.starts_with(BYTE_ORDER_MARK) {
            BYTE_ORDER_MARK.len()
        } else {
            0
        };
        Self {
            start,
            cursor: start,
            ended: false,
        }
    }

    /// The next line, or `None` when `work` runs out first. Each byte scanned
    /// costs one unit.
    pub(super) fn next(&mut self, source: &[u8], work: &mut WorkBudget) -> Option<Line> {
        if self.ended {
            return Some(Line::End);
        }
        loop {
            let rest = &source[self.cursor..];
            if rest.is_empty() {
                self.ended = true;
                let span = Span {
                    start: self.start,
                    end: self.cursor,
                };
                return Some(if span.start < span.end {
                    Line::Text(span)
                } else {
                    Line::End
                });
            }
            let budget = usize::try_from(work.remaining()).unwrap_or(usize::MAX);
            let window = &rest[..rest.len().min(budget)];
            if window.is_empty() {
                return None;
            }
            let Some(offset) = window
                .iter()
                .position(|&byte| matches!(byte, b'\n' | b'\r' | 0))
            else {
                work.take(window.len() as u64);
                self.cursor += window.len();
                continue;
            };
            work.take(offset as u64 + 1);
            let at = self.cursor + offset;
            self.cursor = at + 1;
            if source[at] == 0 {
                self.ended = true;
                return Some(Line::End);
            }
            // A CR LF pair is one terminator; the LF costs no unit of its own.
            if source[at] == b'\r' && source.get(self.cursor) == Some(&b'\n') {
                self.cursor += 1;
            }
            let span = Span {
                start: self.start,
                end: at,
            };
            self.start = self.cursor;
            return Some(Line::Text(span));
        }
    }
}
