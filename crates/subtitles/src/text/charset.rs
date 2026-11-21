//! Strict conversion of text in an explicitly selected charset to UTF-8,
//! with no detection and no replacement characters. The source bytes and
//! the declared label are kept beside the result.
use super::{
    Encoding, Limits,
    unicode::{Cursor, Poll, Probe, cancelled},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};
pub mod multibyte;
mod tables;

use multibyte::MultibyteCharset;

/// A charset to convert from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charset {
    /// Strict UTF-8; the result shares the source's storage.
    Utf8,
    /// UTF-16 in the byte order of its leading byte-order mark, which is
    /// required and is not content.
    Utf16Bom,
    /// Big-endian UTF-16; a leading FEFF is content.
    Utf16Be,
    /// Little-endian UTF-16; a leading FEFF is content.
    Utf16Le,
    /// US-ASCII; a byte above 0x7F is refused.
    Ascii,
    /// ISO-8859-1: each byte is the scalar of its value, 0x80–0x9F the C1
    /// controls.
    Latin1,
    /// Windows-1251; byte 0x98, which its table leaves undefined, is refused.
    Windows1251,
    /// Windows-1252; bytes 0x81, 0x8D, 0x8F, 0x90 and 0x9D, which its table
    /// leaves undefined, are refused.
    Windows1252,
    /// A multibyte profile.
    Multibyte(MultibyteCharset),
}
impl Charset {
    /// The charset of `label`, matched ASCII case-insensitively: utf-8,
    /// utf8, utf-16, utf-16be, utf-16le, us-ascii, ascii, iso-8859-1,
    /// windows-1251, cp1251, windows-1252 or cp1252. `Error::Limit` for a
    /// label over 256 bytes, `Error::Unsupported` for any other; multibyte
    /// labels resolve with [`MultibyteCharset::from_label`].
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
/// What [`CharsetJob::poll`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharsetStatus {
    /// The work budget ran out; poll again.
    Pending,
    /// The conversion is complete; take it with
    /// [`take_output`](CharsetJob::take_output).
    Ready,
}
/// A completed conversion.
#[derive(Debug)]
pub struct ConvertedText {
    /// The source bytes, unchanged.
    pub source: Bytes,
    /// The label the caller declared, unchanged.
    pub declared_label: Option<Bytes>,
    /// The charset converted from.
    pub charset: Charset,
    /// The text as UTF-8; the byte-order mark [`Charset::Utf16Bom`] reads is
    /// not part of it.
    pub utf8: Bytes,
}
/// The most source bytes of a single-byte charset converted in one step.
const SINGLE_BYTE_RUN: usize = 4096;

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

/// Converts one byte stream from an explicit [`Charset`] to UTF-8 in two
/// passes: the first validates the source and counts the output, the second
/// writes it into storage of exactly that size. Cancellation and
/// backpressure are resumable and never reread a converted character; any
/// other error is sticky.
#[derive(Debug)]
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
    /// A job converting `source`, declared with `label`, from `charset`.
    /// `Error::ForeignPool` when either is not charged to `budget`;
    /// `Error::Limit` when the source exceeds `limits.max_input_bytes` or
    /// the label `limits.max_label_bytes`.
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
    /// Counts or encodes the next source bytes of a single-byte charset, up
    /// to [`SINGLE_BYTE_RUN`] of them, charging what converting them one at a
    /// time charges: a unit per source byte and, when encoding, one per output
    /// byte. It stops before a character the budget does not cover in full.
    fn single_byte_run(&mut self, work: &mut WorkBudget) -> Result<()> {
        let end = self
            .range
            .end
            .min(self.offset.saturating_add(SINGLE_BYTE_RUN));
        let source = &self.source.as_ref()[self.offset..end];
        match self.phase {
            Phase::Count => {
                for &byte in source {
                    if !work.try_charge(1) {
                        break;
                    }
                    let value = self.charset.scalar(byte)?;
                    self.offset += 1;
                    self.length = self
                        .length
                        .checked_add(value.len_utf8())
                        .ok_or(Error::Overflow)?;
                    if self.length > self.limits.max_decoded_bytes {
                        return Err(Error::Limit {
                            requested: self.length,
                            maximum: self.limits.max_decoded_bytes,
                        });
                    }
                }
            }
            Phase::Encode => {
                let mut utf8 = [0; 4];
                for &byte in source {
                    // The count validated every byte.
                    let value = self.charset.scalar(byte)?;
                    let encoded = value.encode_utf8(&mut utf8).as_bytes();
                    if !work.try_charge(1 + encoded.len() as u64) {
                        break;
                    }
                    self.output.try_extend_from_slice(encoded)?;
                    self.offset += 1;
                }
            }
            _ => {}
        }
        Ok(())
    }
    /// Advances the conversion, spending a unit per source byte in each pass
    /// and one per output byte written. `Error::Invalid` or
    /// `Error::Truncated` for a byte or sequence the charset refuses,
    /// `Error::Limit` when the output would exceed `limits.max_decoded_bytes`.
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
            if self.charset.cursor_encoding().is_none()
                && matches!(self.phase, Phase::Count | Phase::Encode)
                && self.pending_at == self.pending_len
            {
                let from = self.offset;
                self.single_byte_run(work)?;
                if self.offset != from {
                    continue;
                }
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
                            self.output = e.value;
                            return Err(e.error);
                        }
                    }
                }
                // The loop's head returns in these phases; returning here as well,
                // not looping, keeps the per-character dispatch short.
                Phase::Ready => return Ok(CharsetStatus::Ready),
                Phase::Taken => return Err(Error::Invalid("charset output already taken")),
            }
        }
    }
    /// The conversion, once [`poll`](Self::poll) reported
    /// [`CharsetStatus::Ready`]; `Error::Invalid` before then and once taken.
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
