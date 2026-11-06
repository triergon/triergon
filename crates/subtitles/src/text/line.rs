//! Incremental line boundaries and fixed-size lexical summaries. No line copy.
use super::{
    Encoding,
    unicode::{Cursor, Poll, Scalar, cancelled},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Error, Result, WorkBudget};

#[derive(Clone, Debug)]
pub(crate) struct Line {
    pub start: usize,
    pub end: usize,
    pub after: usize,
    pub decoded_end: usize,
    pub decoded_after: usize,
    pub characters: usize,
    pub digits_only: bool,
    pub has_arrow: bool,
    prefix: [u8; 8],
    last_nonspace: usize,
}

impl Line {
    pub(super) fn empty(&self) -> bool {
        self.characters == 0
    }
    pub(super) fn range(&self) -> Range<usize> {
        self.start..self.end
    }
    pub(super) fn signature(&self) -> bool {
        self.prefix[..6] == *b"WEBVTT"
            && (self.characters == 6 || matches!(self.prefix[6], b' ' | b'\t'))
    }
    pub(super) fn keyword(&self, word: &[u8]) -> bool {
        word.len() <= self.prefix.len()
            && self.characters >= word.len()
            && &self.prefix[..word.len()] == word
            && self.last_nonspace == word.len()
    }
    pub(super) fn note(&self) -> bool {
        self.characters >= 4
            && self.prefix[..4] == *b"NOTE"
            && (self.characters == 4 || matches!(self.prefix[4], b' ' | b'\t'))
    }
}

#[derive(Debug)]
pub(crate) struct Reader {
    cursor: Cursor,
    range_end: usize,
    start: usize,
    end: usize,
    decoded_end: usize,
    characters: usize,
    digits_only: bool,
    has_arrow: bool,
    minus: u8,
    prefix: [u8; 8],
    last_nonspace: usize,
    cr: Option<Scalar>,
    lookahead: Option<Scalar>,
    done: bool,
}

impl Reader {
    pub(super) fn new(range: Range<usize>, encoding: Encoding) -> Self {
        Self {
            cursor: Cursor::new(range.clone(), encoding),
            range_end: range.end,
            start: range.start,
            end: range.start,
            decoded_end: 0,
            characters: 0,
            digits_only: true,
            has_arrow: false,
            minus: 0,
            prefix: [0; 8],
            last_nonspace: 0,
            cr: None,
            lookahead: None,
            done: false,
        }
    }

    fn finish(
        &mut self,
        end: usize,
        after: usize,
        decoded_end: usize,
        decoded_after: usize,
    ) -> Line {
        let line = Line {
            start: self.start,
            end,
            after,
            decoded_end,
            decoded_after,
            characters: self.characters,
            digits_only: self.digits_only,
            has_arrow: self.has_arrow,
            prefix: self.prefix,
            last_nonspace: self.last_nonspace,
        };
        self.start = after;
        self.end = after;
        self.decoded_end = decoded_after;
        self.characters = 0;
        self.digits_only = true;
        self.has_arrow = false;
        self.minus = 0;
        self.prefix = [0; 8];
        self.last_nonspace = 0;
        line
    }

    pub(super) fn poll(
        &mut self,
        bytes: &[u8],
        maximum: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<Line>> {
        loop {
            cancelled(cancel)?;
            if self.done {
                return Ok(Poll::End);
            }
            let scalar = if let Some(saved) = self.lookahead.take() {
                Poll::Ready(saved)
            } else {
                self.cursor.poll(bytes, work, cancel)?
            };
            let scalar = match scalar {
                Poll::Yield => return Ok(Poll::Yield),
                Poll::End => {
                    self.done = true;
                    if let Some(cr) = self.cr.take() {
                        return Ok(Poll::Ready(self.finish(
                            cr.start,
                            cr.end,
                            cr.decoded_start,
                            cr.decoded_end,
                        )));
                    }
                    if self.start < self.range_end {
                        return Ok(Poll::Ready(self.finish(
                            self.end,
                            self.range_end,
                            self.decoded_end,
                            self.cursor.decoded_len(),
                        )));
                    }
                    return Ok(Poll::End);
                }
                Poll::Ready(scalar) => scalar,
            };
            if let Some(cr) = self.cr.take() {
                if scalar.value == '\n' {
                    return Ok(Poll::Ready(self.finish(
                        cr.start,
                        scalar.end,
                        cr.decoded_start,
                        scalar.decoded_end,
                    )));
                }
                self.lookahead = Some(scalar);
                return Ok(Poll::Ready(self.finish(
                    cr.start,
                    cr.end,
                    cr.decoded_start,
                    cr.decoded_end,
                )));
            }
            match scalar.value {
                '\r' => self.cr = Some(scalar),
                '\n' => {
                    return Ok(Poll::Ready(self.finish(
                        scalar.start,
                        scalar.end,
                        scalar.decoded_start,
                        scalar.decoded_end,
                    )));
                }
                value => {
                    let size = scalar.end - self.start;
                    if size > maximum {
                        return Err(Error::Limit {
                            requested: size,
                            maximum,
                        });
                    }
                    if self.characters < self.prefix.len() {
                        self.prefix[self.characters] =
                            if value.is_ascii() { value as u8 } else { 0xff };
                    }
                    self.characters = self.characters.checked_add(1).ok_or(Error::Overflow)?;
                    self.digits_only &= value.is_ascii_digit();
                    if !matches!(value, ' ' | '\t') {
                        self.last_nonspace = self.characters;
                    }
                    if value == '-' {
                        self.minus = self.minus.saturating_add(1).min(2);
                    } else {
                        self.has_arrow |= value == '>' && self.minus == 2;
                        self.minus = 0;
                    }
                    self.end = scalar.end;
                    self.decoded_end = scalar.decoded_end;
                }
            }
        }
    }
}
