//! One byte per work unit; batches are bounded to 256 bytes between checks.
//! Independent ASCII/zero reductions permit compiler vectorization; UTF-8
//! fallback still passes at most four bytes to safe std validation at a time.
//! The constructor seals the exact immutable span after validation, without a
//! second whole-span scan. SharedText::as_str remains the existing safe view.
use super::{SharedBytes, SharedText};
use crate::{
    Error, Result,
    step::{ParseStep, Required},
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::WorkBudget;

#[derive(Debug, Clone)]
pub(crate) struct TextValue {
    pub text: SharedText,
    pub range: Range<usize>,
}
/// Validation state of one text span, holding no source owner: each step reads
/// the span from the source the caller passes, which must be the same bytes
/// every time. A completed check returns the span up to its first zero byte.
#[derive(Debug)]
pub(crate) struct TextCheck {
    range: Range<usize>,
    at: usize,
    scalar: [u8; 4],
    have: usize,
    want: usize,
    end: Option<usize>,
    ascii: bool,
    bad_terminator: bool,
    bad_utf8: bool,
    bad_ascii: bool,
}
impl TextCheck {
    pub(crate) fn new(source: &[u8], range: Range<usize>, ascii: bool) -> Result<Self> {
        if range.start > range.end || range.end > source.len() {
            return Err(Error::Truncated);
        }
        Ok(Self {
            at: range.start,
            range,
            scalar: [0; 4],
            have: 0,
            want: 0,
            end: None,
            ascii,
            bad_terminator: false,
            bad_utf8: false,
            bad_ascii: false,
        })
    }
    pub(crate) fn step(
        &mut self,
        source: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Range<usize>>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.at == self.range.end {
                // Match metadata::text/string_span precedence even when
                // a malformed scalar or ASCII byte precedes the first zero.
                if self.bad_terminator {
                    return Err(Error::Invalid("metadata string terminator"));
                }
                if self.bad_utf8 || self.have != 0 {
                    return Err(Error::Invalid("UTF-8 metadata"));
                }
                if self.bad_ascii {
                    return Err(Error::Invalid("ASCII metadata"));
                }
                return Ok(ParseStep::Event(
                    self.range.start..self.end.unwrap_or(self.range.end),
                ));
            }
            let count = work.take((self.range.end - self.at).min(256) as u64) as usize;
            if count == 0 {
                return Ok(ParseStep::Yield);
            }
            let stop = self.at + count;
            let batch = source.get(self.at..stop).ok_or(Error::Truncated)?;
            if self.end.is_some() {
                // The source owner is immutable. OR is an independent-byte
                // reduction, including all remaining terminator bytes; there
                // is no early acceptance of an invalid nonzero suffix.
                let nonzero = batch.iter().fold(0_u8, |bits, &byte| bits | byte);
                self.bad_terminator |= nonzero != 0;
                self.at = stop;
                continue;
            }
            if self.have == 0 && !self.bad_utf8 {
                // bit0: zero; bit1: outside printable ASCII; bit7: non-ASCII.
                // All lanes are inspected once without scalar dependencies or
                // a data-dependent early exit. Complex fields use the original
                // scalar state below, including cross-batch UTF-8 continuations.
                let flags = batch.iter().fold(0_u8, |flags, &byte| {
                    flags
                        | (byte & 0x80)
                        | u8::from(byte == 0)
                        | (u8::from(!(0x20..=0x7e).contains(&byte)) << 1)
                });
                if flags & 0x81 == 0 {
                    self.bad_ascii |= self.ascii && flags & 2 != 0;
                    self.at = stop;
                    continue;
                }
            }
            for &byte in batch {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                self.at += 1;
                if self.end.is_some() {
                    self.bad_terminator |= byte != 0;
                    continue;
                }
                if byte == 0 {
                    self.end = Some(self.at - 1);
                    self.bad_utf8 |= self.have != 0;
                    self.have = 0;
                    continue;
                }
                self.bad_ascii |= self.ascii && !(0x20..=0x7e).contains(&byte);
                if self.bad_utf8 {
                    continue;
                }
                if self.have == 0 {
                    self.want = match byte {
                        // One ASCII byte is a complete scalar.
                        0..=0x7f => continue,
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => {
                            self.bad_utf8 = true;
                            continue;
                        }
                    };
                }
                self.scalar[self.have] = byte;
                self.have += 1;
                if self.have == self.want {
                    self.bad_utf8 = std::str::from_utf8(&self.scalar[..self.want]).is_err();
                    self.have = 0;
                }
            }
        }
    }
}

/// A text check that keeps its source and returns the validated span as shared text.
#[derive(Debug)]
pub(crate) struct TextJob {
    source: SharedBytes,
    check: TextCheck,
    result: Option<TextValue>,
    failed: bool,
}
impl TextJob {
    pub(crate) fn new(source: SharedBytes, range: Range<usize>, ascii: bool) -> Result<Self> {
        Ok(Self {
            check: TextCheck::new(&source, range, ascii)?,
            source,
            result: None,
            failed: false,
        })
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<TextValue>> {
        if self.failed {
            return Err(Error::Invalid("text requires replacement after error"));
        }
        let result = self.advance(work, cancel);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<TextValue>> {
        if self.result.is_none() {
            let range = match self.check.step(&self.source, work, cancel)? {
                ParseStep::Event(range) => range,
                other => return crate::step::pending(other),
            };
            let text = if range.is_empty() {
                SharedText::constant("")
            } else {
                SharedText {
                    bytes: self.source.slice(range.clone())?,
                    constant: None,
                }
            };
            self.result = Some(TextValue { text, range });
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        Ok(ParseStep::Event(
            self.result.clone().required("missing text result")?,
        ))
    }
}

crate::storage::usage::storage!(TextValue, |s| [s.text]);
crate::storage::usage::storage!(TextJob, |s| [s.source, s.result]);
