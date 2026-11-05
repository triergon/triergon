//! Explicit strict charset conversion; no track detection or lossy replacement.
//! Original bytes and declared label remain separate owned source facts.
use super::{
    Encoding, Limits,
    unicode::{Cursor, Poll, Probe, cancelled},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};
pub mod multibyte;
mod tables;

use multibyte::MultibyteCharset;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charset {
    Utf8,
    Utf16Bom,
    Utf16Be,
    Utf16Le,
    Ascii,
    Latin1,
    Windows1251,
    Windows1252,
    /// Additive explicit profile; the original label resolver is unchanged.
    Multibyte(MultibyteCharset),
}
impl Charset {
    pub fn from_label(label: &[u8]) -> Result<Self> {
        if label.len() > 256 {
            return Err(Error::Limit {
                requested: label.len(),
                maximum: 256,
            });
        }
        for (name, value) in [
            ("utf-8", Self::Utf8),
            ("utf8", Self::Utf8),
            ("utf-16", Self::Utf16Bom),
            ("utf-16be", Self::Utf16Be),
            ("utf-16le", Self::Utf16Le),
            ("us-ascii", Self::Ascii),
            ("ascii", Self::Ascii),
            ("iso-8859-1", Self::Latin1),
            ("windows-1251", Self::Windows1251),
            ("cp1251", Self::Windows1251),
            ("windows-1252", Self::Windows1252),
            ("cp1252", Self::Windows1252),
        ] {
            if label.eq_ignore_ascii_case(name.as_bytes()) {
                return Ok(value);
            }
        }
        Err(Error::Unsupported("declared charset conversion"))
    }
    fn cursor_encoding(self) -> Option<Encoding> {
        match self {
            Self::Utf8 => Some(Encoding::Utf8),
            Self::Utf16Bom => Some(Encoding::Utf16Bom),
            Self::Utf16Be => Some(Encoding::Utf16Be),
            Self::Utf16Le => Some(Encoding::Utf16Le),
            Self::Multibyte(profile) => Some(Encoding::Multibyte(profile)),
            _ => None,
        }
    }
    pub(super) fn scalar(self, byte: u8) -> Result<char> {
        let value = match self {
            Self::Ascii if byte >= 128 => return Err(Error::Invalid("non-ASCII byte")),
            Self::Ascii | Self::Latin1 => u32::from(byte),
            Self::Windows1251 if byte >= 128 => tables::CP1251[usize::from(byte - 128)],
            Self::Windows1252 if byte >= 128 => tables::CP1252[usize::from(byte - 128)],
            Self::Windows1251 | Self::Windows1252 => u32::from(byte),
            _ => return Err(Error::Invalid("charset scalar dispatch")),
        };
        char::from_u32(value).ok_or(Error::Invalid("undefined charset byte"))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharsetStatus {
    Pending,
    Ready,
}
#[derive(Debug)]
pub struct ConvertedText {
    pub source: Bytes,
    pub declared_label: Option<Bytes>,
    pub charset: Charset,
    pub utf8: Bytes,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Probe,
    Count,
    Allocate,
    Encode,
    Freeze,
    Ready,
    Taken,
}

/// Two forward passes validate/count, then encode into exact bounded storage.
/// Cancellation is resumable, matching the existing text source contract.
/// Retained output pressure never rereads a consumed scalar or guesses a charset.
pub struct CharsetJob {
    source: Bytes,
    label: Option<Bytes>,
    charset: Charset,
    budget: Budget,
    limits: Limits,
    range: Range<usize>,
    probe: Option<Probe>,
    cursor: Option<Cursor>,
    effective: Option<Encoding>,
    offset: usize,
    length: usize,
    output: BudgetVec<u8>,
    result: Option<Bytes>,
    pending: [u8; 4],
    pending_len: usize,
    pending_at: usize,
    paid: bool,
    phase: Phase,
    failed: Option<Error>,
}
impl CharsetJob {
    pub fn new(
        source: &Bytes,
        label: Option<&Bytes>,
        charset: Charset,
        budget: &Budget,
        limits: Limits,
    ) -> Result<Self> {
        for (bytes, maximum) in [
            (Some(source), limits.max_input_bytes),
            (label, limits.max_label_bytes),
        ]
        .into_iter()
        .filter_map(|(v, m)| v.map(|b| (b, m)))
        {
            if !bytes.belongs_to(budget) {
                return Err(Error::ForeignPool);
            }
            if bytes.len() > maximum {
                return Err(Error::Limit {
                    requested: bytes.len(),
                    maximum,
                });
            }
        }
        let range = 0..source.len();
        Ok(Self {
            source: source.clone(),
            label: label.cloned(),
            charset,
            budget: budget.clone(),
            limits,
            range: range.clone(),
            probe: charset.cursor_encoding().map(|e| Probe::new(range, e)),
            cursor: None,
            effective: None,
            offset: 0,
            length: 0,
            output: BudgetVec::new(budget),
            result: None,
            pending: [0; 4],
            pending_len: 0,
            pending_at: 0,
            paid: false,
            phase: if charset.cursor_encoding().is_some() {
                Phase::Probe
            } else {
                Phase::Count
            },
            failed: None,
        })
    }
    fn next(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Poll<char>> {
        if let Some(cursor) = self.cursor.as_mut() {
            return Ok(match cursor.poll(self.source.as_ref(), work, cancel)? {
                Poll::Ready(s) => Poll::Ready(s.value),
                Poll::End => Poll::End,
                Poll::Yield => Poll::Yield,
            });
        }
        if self.offset == self.range.end {
            return Ok(Poll::End);
        }
        if !work.try_charge(1) {
            return Ok(Poll::Yield);
        }
        let value = self.charset.scalar(self.source.as_ref()[self.offset])?;
        self.offset += 1;
        Ok(Poll::Ready(value))
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CharsetStatus> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        let value = self.inner(work, cancel);
        if let Err(error) = value
            && !matches!(error, Error::Backpressure { .. } | Error::Cancelled)
        {
            self.output = BudgetVec::new(&self.budget);
            self.result = None;
            self.failed = Some(error);
        }
        value
    }
    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CharsetStatus> {
        loop {
            cancelled(cancel)?;
            if self.phase == Phase::Ready {
                return Ok(CharsetStatus::Ready);
            }
            if self.phase == Phase::Taken {
                return Err(Error::Invalid("charset output already taken"));
            }
            if work.remaining() == 0 {
                return Ok(CharsetStatus::Pending);
            }
            match self.phase {
                Phase::Probe => match self
                    .probe
                    .as_mut()
                    .ok_or(Error::Invalid("charset probe absent"))?
                    .poll(self.source.as_ref(), work, cancel)?
                {
                    Poll::Yield => return Ok(CharsetStatus::Pending),
                    Poll::Ready((encoding, range)) => {
                        self.cursor = Some(Cursor::new(range.clone(), encoding));
                        self.range = range;
                        self.effective = Some(encoding);
                        self.phase = Phase::Count;
                    }
                    Poll::End => return Err(Error::Truncated),
                },
                Phase::Count => match self.next(work, cancel)? {
                    Poll::Yield => return Ok(CharsetStatus::Pending),
                    Poll::Ready(c) => {
                        self.length = self
                            .length
                            .checked_add(c.len_utf8())
                            .ok_or(Error::Overflow)?;
                        if self.length > self.limits.max_decoded_bytes {
                            return Err(Error::Limit {
                                requested: self.length,
                                maximum: self.limits.max_decoded_bytes,
                            });
                        }
                    }
                    Poll::End => {
                        if self.charset == Charset::Utf8 {
                            self.result = Some(self.source.slice(self.range.clone())?);
                            self.phase = Phase::Ready;
                        } else {
                            self.phase = Phase::Allocate;
                        }
                    }
                },
                Phase::Allocate => {
                    if !self.paid {
                        if !work.try_charge(1) {
                            return Ok(CharsetStatus::Pending);
                        }
                        self.paid = true;
                    }
                    self.output.try_reserve_exact(self.length)?;
                    self.cursor = self.effective.map(|e| Cursor::new(self.range.clone(), e));
                    self.offset = self.range.start;
                    self.paid = false;
                    self.phase = Phase::Encode;
                }
                Phase::Encode => {
                    if self.pending_at < self.pending_len {
                        if !work.try_charge(1) {
                            return Ok(CharsetStatus::Pending);
                        }
                        self.output
                            .try_push(self.pending[self.pending_at])
                            .map_err(|e| e.error)?;
                        self.pending_at += 1;
                    } else {
                        match self.next(work, cancel)? {
                            Poll::Yield => return Ok(CharsetStatus::Pending),
                            Poll::Ready(c) => {
                                self.pending_len = c.encode_utf8(&mut self.pending).len();
                                self.pending_at = 0;
                            }
                            Poll::End => {
                                self.phase = Phase::Freeze;
                                self.paid = false;
                            }
                        }
                    }
                }
                Phase::Freeze => {
                    if !self.paid {
                        if !work.try_charge(1) {
                            return Ok(CharsetStatus::Pending);
                        }
                        self.paid = true;
                    }
                    match std::mem::replace(&mut self.output, BudgetVec::new(&self.budget)).freeze()
                    {
                        Ok(bytes) => {
                            self.result = Some(bytes);
                            self.phase = Phase::Ready;
                        }
                        Err(e) => {
                            self.output = e.buffer;
                            return Err(e.error);
                        }
                    }
                }
                Phase::Ready | Phase::Taken => unreachable!(),
            }
        }
    }
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<ConvertedText> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("charset output not ready"));
        }
        let utf8 = self
            .result
            .take()
            .ok_or(Error::Invalid("charset output absent"))?;
        self.phase = Phase::Taken;
        Ok(ConvertedText {
            source: self.source.clone(),
            declared_label: self.label.clone(),
            charset: self.charset,
            utf8,
        })
    }
}
