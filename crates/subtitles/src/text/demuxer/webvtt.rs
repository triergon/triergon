//! WebVTT as the `webvtt` demuxer reads it.
//!
//! The demuxer reads the document one block at a time ([`super::blocks`]).
//! A block that starts with `WEBVTT`, with a UTF-8 byte-order mark and
//! `WEBVTT`, with `NOTE`, `STYLE` or `REGION` is skipped, whatever follows
//! the word. Any other block must be a cue; the first block that is not ends
//! the document, and nothing after it is read. No header is required, and a
//! byte-order mark before anything other than `WEBVTT` is part of the block.
//!
//! Lines end at CR LF, LF or CR. A cue's first line is its identifier unless
//! it contains `-->`; the timing then starts on the next line, otherwise on
//! the first. The timing is a start time, then anything up to the first
//! `-->` after it, in the same line or a later one, then an end time. A time
//! is `H:M:S.F` or `M:S.F`. Each field is leading white space (which may span
//! lines), an optional sign and decimal digits. The digits count up to
//! 2³² − 1 and stop there, a minus negates the count modulo 2³², and the
//! result reads as a 32-bit signed number: `4294967295` and `-1` read as −1,
//! `2147483648` as −2³¹. This is what is observed on 32-bit ARMv7. A field has
//! no range of its own, and the fraction is a count of
//! milliseconds whatever its width (`1.5` is 1.005 seconds). Whatever
//! follows a time's last digit is ignored.
//!
//! The settings are the rest of the line that holds `-->`: after the arrow,
//! spaces and tabs, the end time's bytes up to the next space, tab or line
//! end, and more spaces and tabs. Empty settings are none. The cue's text
//! runs from the next line to the end of the block, less any CR and LF bytes
//! at its end; it may be empty. The cue lasts from its start to its end time,
//! in milliseconds.
//!
//! An empty block, the end of the document, ends it too.

use super::lines::Span;
use super::timestamp::is_space;
use super::{Entries, Entry};
use std::ops::Range;
use triergon_core::Result;

const ARROW: &[u8] = b"-->";
const BYTE_ORDER_MARK: &[u8] = b"\xef\xbb\xbf";

fn is_break(byte: u8) -> bool {
    byte == b'\r' || byte == b'\n'
}

fn is_blank(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

/// Whether the demuxer skips a block that starts with `bytes`.
fn skipped(bytes: &[u8]) -> bool {
    let header = bytes.strip_prefix(BYTE_ORDER_MARK).unwrap_or(bytes);
    header.starts_with(b"WEBVTT")
        || [&b"NOTE"[..], b"STYLE", b"REGION"]
            .iter()
            .any(|word| bytes.starts_with(word))
}

/// A cursor over one block.
struct Scan<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip(&mut self, set: fn(u8) -> bool) {
        while self.peek().is_some_and(set) {
            self.at += 1;
        }
    }

    fn byte(&mut self, wanted: u8) -> Option<()> {
        self.peek().filter(|&byte| byte == wanted)?;
        self.at += 1;
        Some(())
    }

    /// A field: its digits as a count that stops at 2³² − 1, negated modulo
    /// 2³² after a minus, read as a 32-bit signed number.
    fn field(&mut self) -> Option<i64> {
        self.skip(is_space);
        let negative = self.peek() == Some(b'-');
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.at += 1;
        }
        let digits = self.at;
        let mut value = Some(0_u32);
        while let Some(digit) = self.peek().filter(u8::is_ascii_digit) {
            value = value
                .and_then(|value| value.checked_mul(10))
                .and_then(|value| value.checked_add(u32::from(digit - b'0')));
            self.at += 1;
        }
        if self.at == digits {
            return None;
        }
        let value = match value {
            Some(value) if negative => value.wrapping_neg(),
            Some(value) => value,
            None => u32::MAX,
        };
        Some(i64::from(value.cast_signed()))
    }

    /// `count` fields separated by `:`, then `.` and the fraction.
    fn clock(&mut self, count: usize) -> Option<i64> {
        let mut total = 0_i64;
        for at in 0..count {
            if at > 0 {
                self.byte(b':')?;
            }
            total = total * 60 + self.field()?;
        }
        self.byte(b'.')?;
        Some(total * 1000 + self.field()?)
    }

    /// `H:M:S.F` or `M:S.F`, in milliseconds.
    fn time(&mut self) -> Option<i64> {
        let from = self.at;
        if let Some(time) = self.clock(3) {
            return Some(time);
        }
        self.at = from;
        self.clock(2)
    }

    /// Past the line end (CR LF, LF or CR) at the cursor, if there is one.
    fn line_end(&mut self) {
        let _ = self.byte(b'\r');
        let _ = self.byte(b'\n');
    }
}

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Reads one block (`block` is its byte range in `source`), adding its cue
/// to `entries` if it is one; `Ok(false)` when the document ends at it.
pub(super) fn block(source: &[u8], block: Span, entries: &mut Entries) -> Result<bool> {
    let bytes = block.of(source);
    if skipped(bytes) {
        return Ok(true);
    }
    let Some(cue) = cue(bytes) else {
        return Ok(false);
    };
    let range = |range: Range<usize>| block.start + range.start..block.start + range.end;
    entries.push(Entry {
        start_ms: cue.start_ms,
        duration_ms: cue.end_ms - cue.start_ms,
        order: 0,
        text: range(cue.text),
        identifier: cue.identifier.map(range),
        settings: cue.settings.map(range),
    })?;
    Ok(true)
}

/// One cue, with ranges in its block.
struct Cue {
    start_ms: i64,
    end_ms: i64,
    text: Range<usize>,
    identifier: Option<Range<usize>>,
    settings: Option<Range<usize>>,
}

fn cue(bytes: &[u8]) -> Option<Cue> {
    let mut scan = Scan { bytes, at: 0 };
    scan.skip(|byte| !is_break(byte));
    let identifier = if find(&bytes[..scan.at], ARROW).is_some() {
        scan.at = 0;
        None
    } else {
        let identifier = 0..scan.at;
        scan.line_end();
        Some(identifier)
    };
    let timing = scan.at;
    let start_ms = scan.time()?;
    scan.at = timing + find(&bytes[timing..], ARROW)? + ARROW.len();
    let arrow = scan.at;
    let end_ms = scan.time()?;
    scan.at = arrow;
    scan.skip(is_blank);
    scan.skip(|byte| !is_blank(byte) && !is_break(byte));
    scan.skip(is_blank);
    let settings = scan.at;
    scan.skip(|byte| !is_break(byte));
    let settings = (scan.at > settings).then_some(settings..scan.at);
    scan.line_end();
    let text_start = scan.at;
    let text_end = bytes
        .iter()
        .rposition(|&byte| !is_break(byte))
        .map_or(0, |last| last + 1)
        .max(text_start);
    Some(Cue {
        start_ms,
        end_ms,
        text: text_start..text_end,
        identifier,
        settings,
    })
}
