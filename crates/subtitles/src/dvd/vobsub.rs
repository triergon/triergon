//! The VobSub `.idx` index: line-oriented `key: value` text naming the
//! subpicture tracks of the sibling `.sub` program stream and where each
//! subpicture unit starts.
//!
//! [`Lines`] reads it one line at a time without allocating: settings such as
//! `size:` and `palette:` stay raw for the caller, and every `id:`, `delay:`
//! and `timestamp:` line is decoded. Line ends may be LF or CRLF.
use triergon_core::{Error, Result};

/// One line of an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line<'a> {
    /// Empty, or only white space.
    Blank,
    /// A line beginning with `#`.
    Comment,
    /// `langidx:`: the track a player selects first.
    LanguageIndex(&'a [u8]),
    /// `id: LANGUAGE, index: N`: the following lines describe track `N`, the
    /// subpictures in substream `0x20 + N`. A missing index is `None`.
    Track {
        language: &'a [u8],
        index: Option<u8>,
    },
    /// `delay: [-]HH:MM:SS:mmm`: the offset of the timestamps that follow.
    Delay(i64),
    /// `timestamp: HH:MM:SS:mmm, filepos: HEX`: a unit of the current track at
    /// this byte position of the `.sub` file.
    Timestamp { milliseconds: i64, filepos: u64 },
    /// Any other line, such as `size:`, `palette:` or `custom colors:`.
    Setting(&'a [u8]),
}

/// The lines of an index, each without its line end.
#[derive(Clone, Debug)]
pub struct Lines<'a> {
    rest: &'a [u8],
}

impl<'a> Lines<'a> {
    pub fn new(text: &'a [u8]) -> Self {
        Self { rest: text }
    }
}

impl<'a> Iterator for Lines<'a> {
    /// The line's raw bytes and its meaning; `Err` for an `id:`, `delay:` or
    /// `timestamp:` line that does not parse.
    type Item = (&'a [u8], Result<Line<'a>>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let end = self.rest.iter().position(|&b| b == b'\n');
        let (line, rest) = match end {
            Some(end) => (&self.rest[..end], &self.rest[end + 1..]),
            None => (self.rest, &[][..]),
        };
        self.rest = rest;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        Some((line, classify(line)))
    }
}

fn classify(line: &[u8]) -> Result<Line<'_>> {
    if line.trim_ascii().is_empty() {
        return Ok(Line::Blank);
    }
    if line.starts_with(b"#") {
        return Ok(Line::Comment);
    }
    if let Some(value) = line.strip_prefix(b"langidx:") {
        return Ok(Line::LanguageIndex(value.trim_ascii()));
    }
    if let Some(value) = line.strip_prefix(b"id:") {
        let value = value.trim_ascii_start();
        let (language, rest) = match value.iter().position(|&b| b == b',') {
            Some(comma) => (&value[..comma], &value[comma + 1..]),
            None => (value, &[][..]),
        };
        let index = Scanner(rest)
            .word(b"index:")
            .and_then(|mut s| s.number())
            .map(|n| u8::try_from(n).map_err(|_| Error::Invalid("VobSub track index")))
            .transpose()?;
        return Ok(Line::Track {
            language: language.trim_ascii(),
            index,
        });
    }
    if let Some(value) = line.strip_prefix(b"delay:") {
        let mut scanner = Scanner(value);
        scanner.space();
        let negative = scanner.literal(b"-");
        let time = scanner.clock().ok_or(Error::Invalid("VobSub delay"))?;
        return Ok(Line::Delay(if negative { -time } else { time }));
    }
    if let Some(value) = line.strip_prefix(b"timestamp:") {
        let mut scanner = Scanner(value);
        let milliseconds = scanner.clock().ok_or(Error::Invalid("VobSub timestamp"))?;
        let filepos = scanner
            .word(b",")
            .and_then(|s| s.word(b"filepos:"))
            .and_then(|mut s| s.hex())
            .ok_or(Error::Invalid("VobSub file position"))?;
        return Ok(Line::Timestamp {
            milliseconds,
            filepos,
        });
    }
    Ok(Line::Setting(line))
}

/// A cursor over the fields of one line.
struct Scanner<'a>(&'a [u8]);

impl<'a> Scanner<'a> {
    fn space(&mut self) {
        self.0 = self.0.trim_ascii_start();
    }

    fn literal(&mut self, text: &[u8]) -> bool {
        match self.0.strip_prefix(text) {
            Some(rest) => {
                self.0 = rest;
                true
            }
            None => false,
        }
    }

    /// Optional white space, then `text`.
    fn word(mut self, text: &[u8]) -> Option<Scanner<'a>> {
        self.space();
        self.literal(text).then_some(self)
    }

    fn digits(&mut self, radix: u32) -> Option<u64> {
        self.space();
        let length = self
            .0
            .iter()
            .take_while(|b| char::from(**b).is_digit(radix))
            .count();
        let (digits, rest) = self.0.split_at(length);
        self.0 = rest;
        digits
            .iter()
            .try_fold(0_u64, |value, &digit| {
                value
                    .checked_mul(u64::from(radix))?
                    .checked_add(u64::from(char::from(digit).to_digit(radix)?))
            })
            .filter(|_| length != 0)
    }

    fn number(&mut self) -> Option<u64> {
        self.digits(10)
    }

    fn hex(&mut self) -> Option<u64> {
        self.digits(16)
    }

    /// `H:M:S:ms` in milliseconds; each field any number of digits.
    fn clock(&mut self) -> Option<i64> {
        let mut total = 0_i64;
        for (i, scale) in [3_600_000, 60_000, 1_000, 1].into_iter().enumerate() {
            if i != 0 && !self.literal(b":") {
                return None;
            }
            let field = i64::try_from(self.number()?).ok()?;
            total = total.checked_add(field.checked_mul(scale)?)?;
        }
        Some(total)
    }
}
