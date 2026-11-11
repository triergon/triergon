//! UTF-16 SubRip documents, converted to UTF-8 as the `srt` demuxer
//! converts them.
//!
//! A document that starts with a UTF-16 byte-order mark is read in that byte
//! order; the WebVTT demuxer has no such conversion. An unpaired surrogate
//! reads as a NUL, which ends the document, and an odd last byte is ignored.
//!
//! The demuxer looks at the character after each CR, and after each LF that
//! does not complete a line end, to see whether it completes one: an LF after
//! a CR or after an LF. A character it looks at that does not complete one
//! keeps only the first byte of its UTF-8 form.

use super::{Limits, Syntax};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Debug)]
pub(super) struct Decoder {
    little_endian: bool,
    /// Next byte of the source.
    at: usize,
    /// A high surrogate waiting for its low half.
    high: Option<u16>,
    output: BudgetVec<u8>,
    maximum: usize,
    /// The next character is looked at: it follows a CR or LF that it may
    /// complete into a line end.
    peeked: bool,
}

impl Decoder {
    /// The conversion `document` needs, if any.
    pub(super) fn for_document(
        pool: &Budget,
        document: &Bytes,
        syntax: Syntax,
        limits: Limits,
    ) -> Option<Self> {
        let little_endian = match document.as_ref().get(..2)? {
            [0xfe, 0xff] => false,
            [0xff, 0xfe] => true,
            _ => return None,
        };
        (syntax == Syntax::SubRip).then(|| Self {
            little_endian,
            at: 2,
            high: None,
            output: BudgetVec::new(pool),
            maximum: limits.document_bytes,
            peeked: false,
        })
    }

    /// Converts until `work` runs out (`None`) or the document is converted.
    /// Each code unit costs one unit.
    pub(super) fn advance(
        &mut self,
        source: &[u8],
        work: &mut WorkBudget,
    ) -> Result<Option<Bytes>> {
        while let Some(pair) = source.get(self.at..self.at + 2) {
            if !work.try_charge(1) {
                return Ok(None);
            }
            self.at += 2;
            let unit = if self.little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            };
            let scalar = match (self.high.take(), unit) {
                (None, 0xd800..=0xdbff) => {
                    self.high = Some(unit);
                    continue;
                }
                (Some(high), 0xdc00..=0xdfff) => {
                    0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00)
                }
                (Some(_), _) | (None, 0xdc00..=0xdfff) => {
                    self.push('\0')?;
                    break;
                }
                (None, _) => u32::from(unit),
            };
            let scalar = char::from_u32(scalar).ok_or(Error::Invalid("UTF-16 scalar"))?;
            self.push(scalar)?;
        }
        let empty = BudgetVec::new(self.output.budget());
        let output = std::mem::replace(&mut self.output, empty);
        output.freeze().map(Some).map_err(|error| error.error)
    }

    fn push(&mut self, scalar: char) -> Result<()> {
        let mut buffer = [0; 4];
        let mut encoded = scalar.encode_utf8(&mut buffer).as_bytes();
        let line_break = matches!(scalar, '\r' | '\n');
        if std::mem::take(&mut self.peeked) {
            // An LF completes the line end the CR or LF before it started.
            if scalar != '\n' {
                encoded = &encoded[..1];
                self.peeked = line_break;
            }
        } else {
            self.peeked = line_break;
        }
        let length = self.output.len() + encoded.len();
        if length > self.maximum {
            return Err(Error::Limit {
                requested: length,
                maximum: self.maximum,
            });
        }
        self.output.try_reserve(encoded.len())?;
        self.output.try_extend_from_slice(encoded)
    }
}
