//! Versioned opaque-byte carriage facts from the pinned public native API.
//! This reader is separate from Unicode decoding and never edits source bytes.
use std::ops::Range;
use triergon_core::{Error, Result};

/// The packet reader whose byte, clock and position behaviour a snapshot
/// reproduces. `Reference` is FFmpeg's text demuxer reader at the revision
/// [`revision`](Self::revision) names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderView {
    Reference,
}

impl ReaderView {
    pub const fn revision(self) -> &'static str {
        match self {
            Self::Reference => "d32b387f2b0a484599d4587d651891f0c63c4238",
        }
    }

    pub(crate) fn line_read(
        self,
        source: &[u8],
        utf16: bool,
        range: Range<usize>,
        separator: bool,
    ) -> Result<LineRead> {
        let line = source
            .get(range.clone())
            .ok_or(Error::Invalid("native line span"))?;
        if line.is_empty() {
            return Err(Error::Invalid("empty native line read"));
        }
        let resume = match (self, utf16) {
            (Self::Reference, true) => {
                // The native reference shows this observable entry transition on each UTF16
                // SubRip body line: emit the first converted byte, then resume at
                // the next scalar. No byte value (including0x85) is special.
                // This is not a claim about the native implementation's internals.
                let width = match line[0] {
                    0x00..=0x7f => 1,
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Err(Error::Invalid("converted Unicode scalar boundary")),
                };
                let first = line
                    .get(..width)
                    .ok_or(Error::Invalid("converted Unicode scalar tail"))?;
                std::str::from_utf8(first)
                    .map_err(|_| Error::Invalid("converted Unicode scalar"))?;
                Some(range.start.checked_add(width).ok_or(Error::Overflow)?)
            }
            (Self::Reference, false) => None,
        };
        Ok(LineRead {
            next: range.start,
            end: range.end,
            separator,
            resume,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LineRead {
    next: usize,
    end: usize,
    separator: bool,
    resume: Option<usize>,
}

impl LineRead {
    pub(crate) fn byte(self, source: &[u8]) -> Result<u8> {
        if self.separator {
            Ok(b'\n')
        } else if self.next >= self.end {
            Err(Error::Invalid("native line cursor end"))
        } else {
            source
                .get(self.next)
                .copied()
                .ok_or(Error::Invalid("native line cursor"))
        }
    }
    pub(crate) fn advance(&mut self) {
        if self.separator {
            self.separator = false;
        } else if let Some(next) = self.resume.take() {
            self.next = next;
        } else {
            self.next += 1;
        }
    }
    pub(crate) fn complete(self) -> bool {
        !self.separator && self.next == self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn read(source: &[u8], utf16: bool) -> Vec<u8> {
        let mut cursor = ReaderView::Reference
            .line_read(source, utf16, 0..source.len(), false)
            .unwrap();
        let mut result = Vec::new();
        while !cursor.complete() {
            result.push(cursor.byte(source).unwrap());
            cursor.advance();
        }
        result
    }
    #[test]
    fn source_unicode_remains_correct_while_native_line_entry_is_versioned() {
        for source in [
            &b"\xc3\x85Z"[..],
            &b"\xc3\xa9Z"[..],
            &b"\xe6\xb0\xb4Z"[..],
            &b"\xf0\x9f\x98\x80Z"[..],
        ] {
            let before = source.to_vec();
            assert_eq!(read(source, true), [source[0], b'Z']);
            assert_eq!(source, before);
            assert!(std::str::from_utf8(source).is_ok());
            assert_eq!(read(source, false), source);
        }
        assert_eq!(
            ReaderView::Reference.revision(),
            "d32b387f2b0a484599d4587d651891f0c63c4238"
        );
    }
    #[test]
    fn middle_scalar_and_real_nel_are_not_filtered() {
        for source in [&b"A\xc3\x85Z"[..], &b"A\xc2\x85Z"[..]] {
            assert_eq!(read(source, true), source);
            assert_eq!(read(source, false), source);
        }
    }
    #[test]
    fn only_converted_unicode_uses_scalar_entry_checks() {
        assert_eq!(read(b"\xc3", false), b"\xc3");
        assert!(
            ReaderView::Reference
                .line_read(b"\xc3", true, 0..1, false)
                .is_err()
        );
        assert!(
            ReaderView::Reference
                .line_read(b"A", true, 0..2, false)
                .is_err()
        );
        assert!(
            ReaderView::Reference
                .line_read(b"", true, 0..0, false)
                .is_err()
        );
    }
}
