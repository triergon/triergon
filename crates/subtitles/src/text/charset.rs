//! Strict conversion of text in an explicitly selected charset to UTF-8,
//! with no detection and no replacement characters. The source bytes and
//! the declared label are kept beside the result.
use super::{
    Encoding, Limits,
    unicode::{Cursor, Outside, Poll, Probe, cancelled},
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
    /// The reader of `range` in this charset, before any of it is read.
    fn reader(self, range: Range<usize>) -> Reader {
        let encoding = match self {
            Self::Utf8 => Encoding::Utf8,
            Self::Utf16Bom => Encoding::Utf16Bom,
            Self::Utf16Be => Encoding::Utf16Be,
            Self::Utf16Le => Encoding::Utf16Le,
            Self::Multibyte(profile) => Encoding::Multibyte(profile),
            Self::Ascii => return Reader::Table(Table::Ascii, range.start),
            Self::Latin1 => return Reader::Table(Table::Latin1, range.start),
            Self::Windows1251 => return Reader::Table(Table::Windows1251, range.start),
            Self::Windows1252 => return Reader::Table(Table::Windows1252, range.start),
        };
        Reader::Probe(Probe::new(range, encoding))
    }
}

/// A single-byte charset: every byte is one character.
#[derive(Clone, Copy, Debug)]
enum Table {
    Ascii,
    Latin1,
    Windows1251,
    Windows1252,
}
impl Table {
    fn scalar(self, byte: u8) -> Result<char> {
        let value = match self {
            Self::Ascii if byte >= 128 => return Err(Error::Invalid("non-ASCII byte")),
            Self::Ascii | Self::Latin1 => u32::from(byte),
            Self::Windows1251 if byte >= 128 => tables::CP1251[usize::from(byte - 128)],
            Self::Windows1252 if byte >= 128 => tables::CP1252[usize::from(byte - 128)],
            Self::Windows1251 | Self::Windows1252 => u32::from(byte),
        };
        char::from_u32(value).ok_or(Error::Invalid("undefined charset byte"))
    }
}

/// Where the source's characters come from.
#[derive(Debug)]
enum Reader {
    /// A single-byte charset and the offset of its next byte.
    Table(Table, usize),
    /// A Unicode or multibyte charset whose leading bytes have not chosen
    /// its encoding yet.
    Probe(Probe),
    /// The scalar decoder of the chosen encoding.
    Scalars(Cursor),
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

#[derive(Debug)]
enum Phase {
    /// Validating the source and counting the output.
    Count,
    /// Reserving the output; whether the reservation's unit is paid.
    Allocate {
        paid: bool,
    },
    /// Writing the output.
    Encode,
    /// Freezing the output; whether the freeze's unit is paid.
    Freeze {
        paid: bool,
    },
    /// The conversion, ready to take.
    Ready(Bytes),
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
    /// The source bytes that are text: all of them, or those after a
    /// byte-order mark.
    range: Range<usize>,
    reader: Reader,
    /// The output's length, counted.
    length: usize,
    output: BudgetVec<u8>,
    /// A character read but not yet written whole: its UTF-8 bytes and how
    /// many are written.
    pending: [u8; 4],
    pending_len: usize,
    pending_at: usize,
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
            reader: charset.reader(range.clone()),
            range,
            length: 0,
            output: BudgetVec::new(budget),
            pending: [0; 4],
            pending_len: 0,
            pending_at: 0,
            phase: Phase::Count,
            failed: None,
        })
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
        let mut value = self.inner(work, cancel);
        // Only Big5 decodes anything its table refuses.
        while value.is_err() && self.charset == Charset::Multibyte(MultibyteCharset::Big5) {
            match self.outside_table() {
                Ok(true) => value = self.inner(work, cancel),
                Ok(false) => break,
                Err(error) => {
                    value = Err(error);
                    break;
                }
            }
        }
        if let Err(error) = value
            && !matches!(error, Error::Backpressure { .. } | Error::Cancelled)
        {
            self.output = BudgetVec::new(&self.budget);
            self.failed = Some(error);
        }
        value
    }
    // Out of line, so the conversion loops compile the same whatever calls
    // `poll`: inlined into a decoder's larger step they cost about a tenth more
    // instructions for each conversion.
    #[inline(never)]
    /// After `inner` failed: takes what the reader's profile decodes although
    /// its table refused it, as `inner` takes a character; `false` when the
    /// refusal stands. `inner` reads through the cursor's bare step, so this
    /// is the one place a conversion calls for it.
    #[cold]
    fn outside_table(&mut self) -> Result<bool> {
        let Reader::Scalars(cursor) = &mut self.reader else {
            return Ok(false);
        };
        match cursor.outside_table(self.source.as_ref()) {
            Outside::Refused => return Ok(false),
            Outside::Lead => {}
            Outside::Scalar(scalar) => match self.phase {
                Phase::Count => self.length = counted(self.length, scalar.value, &self.limits)?,
                Phase::Encode => {
                    self.pending_len = scalar.value.encode_utf8(&mut self.pending).len();
                    self.pending_at = 0;
                }
                _ => {}
            },
        }
        Ok(true)
    }
    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CharsetStatus> {
        loop {
            cancelled(cancel)?;
            match self.phase {
                Phase::Ready(_) => return Ok(CharsetStatus::Ready),
                Phase::Taken => return Err(Error::Invalid("charset output already taken")),
                _ => {}
            }
            if work.remaining() == 0 {
                return Ok(CharsetStatus::Pending);
            }
            match self.phase {
                Phase::Count => match &mut self.reader {
                    // A run of single-byte characters, a unit each; it stops
                    // where the budget does.
                    Reader::Table(table, offset) => {
                        if *offset == self.range.end {
                            self.phase = Phase::Allocate { paid: false };
                            continue;
                        }
                        for &byte in run(&self.source, *offset..self.range.end) {
                            if !work.try_charge(1) {
                                break;
                            }
                            let value = table.scalar(byte)?;
                            *offset += 1;
                            self.length = counted(self.length, value, &self.limits)?;
                        }
                    }
                    Reader::Probe(probe) => {
                        let Some((encoding, range)) =
                            probe.poll(self.source.as_ref(), work, cancel)?
                        else {
                            return Ok(CharsetStatus::Pending);
                        };
                        self.reader = Reader::Scalars(Cursor::new(range.clone(), encoding));
                        self.range = range;
                    }
                    Reader::Scalars(cursor) => {
                        match cursor.step(self.source.as_ref(), work, cancel)? {
                            Poll::Yield => return Ok(CharsetStatus::Pending),
                            Poll::Ready(scalar) => {
                                self.length = counted(self.length, scalar.value, &self.limits)?;
                            }
                            Poll::End if self.charset == Charset::Utf8 => {
                                self.phase = Phase::Ready(self.source.slice(self.range.clone())?);
                            }
                            Poll::End => self.phase = Phase::Allocate { paid: false },
                        }
                    }
                },
                Phase::Allocate { paid } => {
                    if !paid {
                        if !work.try_charge(1) {
                            return Ok(CharsetStatus::Pending);
                        }
                        self.phase = Phase::Allocate { paid: true };
                    }
                    self.output.try_reserve_exact(self.length)?;
                    // The second pass reads the text again from its start.
                    match &mut self.reader {
                        Reader::Table(_, offset) => *offset = self.range.start,
                        Reader::Scalars(cursor) => {
                            *cursor = Cursor::new(self.range.clone(), cursor.encoding());
                        }
                        Reader::Probe(_) => {}
                    }
                    self.phase = Phase::Encode;
                }
                Phase::Encode if self.pending_at < self.pending_len => {
                    if !work.try_charge(1) {
                        return Ok(CharsetStatus::Pending);
                    }
                    self.output
                        .try_push(self.pending[self.pending_at])
                        .map_err(|e| e.error)?;
                    self.pending_at += 1;
                }
                Phase::Encode => {
                    let value = match &mut self.reader {
                        // A run of single-byte characters, a unit each and one
                        // per output byte; it stops before a character the
                        // budget does not cover whole, which is then read and
                        // written a byte at a time.
                        Reader::Table(table, offset) => {
                            if *offset == self.range.end {
                                self.phase = Phase::Freeze { paid: false };
                                continue;
                            }
                            let from = *offset;
                            let mut utf8 = [0; 4];
                            for &byte in run(&self.source, from..self.range.end) {
                                // The count validated every byte.
                                let encoded = table.scalar(byte)?.encode_utf8(&mut utf8).as_bytes();
                                if !work.try_charge(1 + encoded.len() as u64) {
                                    break;
                                }
                                self.output.try_extend_from_slice(encoded)?;
                                *offset += 1;
                            }
                            if *offset != from {
                                continue;
                            }
                            if !work.try_charge(1) {
                                return Ok(CharsetStatus::Pending);
                            }
                            let value = table.scalar(self.source.as_ref()[from])?;
                            *offset += 1;
                            value
                        }
                        Reader::Probe(_) => {
                            self.phase = Phase::Count;
                            continue;
                        }
                        Reader::Scalars(cursor) => {
                            match cursor.step(self.source.as_ref(), work, cancel)? {
                                Poll::Yield => return Ok(CharsetStatus::Pending),
                                Poll::Ready(scalar) => scalar.value,
                                Poll::End => {
                                    self.phase = Phase::Freeze { paid: false };
                                    continue;
                                }
                            }
                        }
                    };
                    self.pending_len = value.encode_utf8(&mut self.pending).len();
                    self.pending_at = 0;
                }
                Phase::Freeze { paid } => {
                    if !paid {
                        if !work.try_charge(1) {
                            return Ok(CharsetStatus::Pending);
                        }
                        self.phase = Phase::Freeze { paid: true };
                    }
                    match std::mem::replace(&mut self.output, BudgetVec::new(&self.budget)).freeze()
                    {
                        Ok(bytes) => self.phase = Phase::Ready(bytes),
                        Err(e) => {
                            self.output = e.value;
                            return Err(e.error);
                        }
                    }
                }
                // The loop's head returns in these phases; returning here as well,
                // not looping, keeps the per-character dispatch short.
                Phase::Ready(_) => return Ok(CharsetStatus::Ready),
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
        let utf8 = match std::mem::replace(&mut self.phase, Phase::Taken) {
            Phase::Ready(utf8) => utf8,
            phase => {
                self.phase = phase;
                return Err(Error::Invalid("charset output not ready"));
            }
        };
        Ok(ConvertedText {
            source: self.source.clone(),
            declared_label: self.label.clone(),
            charset: self.charset,
            utf8,
        })
    }
}

/// The source bytes of a single-byte charset one step converts: `text`, or its
/// first [`SINGLE_BYTE_RUN`] bytes.
fn run(source: &Bytes, text: Range<usize>) -> &[u8] {
    let end = text.end.min(text.start.saturating_add(SINGLE_BYTE_RUN));
    &source.as_ref()[text.start..end]
}

/// The counted output length `length` with `character` added; `Error::Limit`
/// beyond `limits.max_decoded_bytes`.
fn counted(length: usize, character: char, limits: &Limits) -> Result<usize> {
    let length = length
        .checked_add(character.len_utf8())
        .ok_or(Error::Overflow)?;
    if length > limits.max_decoded_bytes {
        return Err(Error::Limit {
            requested: length,
            maximum: limits.max_decoded_bytes,
        });
    }
    Ok(length)
}
