//! WebVTT as the `webvtt` demuxer reads it.
//!
//! The document is read in blocks ([`blocks`](super::blocks) says where
//! they end). A block that starts with `WEBVTT` (after a UTF-8 byte-order mark or
//! not), `STYLE`, `REGION` or `NOTE` is skipped. Any other block is a cue:
//!
//! - its first line, up to a CR or LF, is the identifier unless it contains
//!   `-->` (an empty one is none);
//! - then come the start time, the first `-->` after it, wherever it is in
//!   the block, and the end time;
//! - the settings follow the end time's word and the blanks after it, up to
//!   the next CR or LF;
//! - the payload is the rest of the block after that line, byte for byte.
//!
//! A cue whose times do not read ends the document. An empty payload is a
//! packet.

use super::lines::Span;
use super::timestamp::{self, is_space};
use super::{Entries, Entry};
use triergon_core::Result;

/// Block starts that are not cues.
const SKIPPED: [&[u8]; 5] = [
    b"WEBVTT",
    b"\xef\xbb\xbfWEBVTT",
    b"STYLE",
    b"REGION",
    b"NOTE",
];

/// Reads one block; `false` when the document ends at it.
pub(super) fn block(source: &[u8], block: Span, entries: &mut Entries) -> Result<bool> {
    match cue(source, block) {
        Read::Skipped => Ok(true),
        Read::Cue(entry) => entries.push(entry).map(|()| true),
        Read::Invalid => Ok(false),
    }
}

enum Read {
    Skipped,
    Cue(Entry),
    Invalid,
}

/// The cue a block holds.
fn cue(source: &[u8], block: Span) -> Read {
    let text = block.of(source);
    if SKIPPED.iter().any(|start| text.starts_with(start)) {
        return Read::Skipped;
    }
    let first = line_end(text, 0);
    let (identifier, timing) = if text[..first].windows(3).any(|w| w == b"-->") {
        (None, 0)
    } else {
        ((first > 0).then_some(0..first), next_line(text, first))
    };
    let Some(timing) = times(text, timing) else {
        return Read::Invalid;
    };
    let at = |range: std::ops::Range<usize>| block.start + range.start..block.start + range.end;
    Read::Cue(Entry {
        start_ms: timing.start_ms,
        duration_ms: timing.end_ms.wrapping_sub(timing.start_ms),
        order: 0,
        text: at(timing.payload..text.len()),
        identifier: identifier.map(at),
        settings: timing.settings.map(at),
    })
}

struct Times {
    start_ms: i64,
    end_ms: i64,
    settings: Option<std::ops::Range<usize>>,
    payload: usize,
}

/// The times, settings and payload start from `at` in a cue block.
fn times(text: &[u8], at: usize) -> Option<Times> {
    let (start_ms, after_start) = timestamp::webvtt(text, at)?;
    let arrow = after_start + text[after_start..].windows(3).position(|w| w == b"-->")? + 3;
    let (end_ms, _) = timestamp::webvtt(text, arrow)?;
    let mut settings = skip_blanks(text, arrow);
    while text.get(settings).is_some_and(|&byte| !is_space(byte)) {
        settings += 1;
    }
    settings = skip_blanks(text, settings);
    let end = line_end(text, settings);
    Some(Times {
        start_ms,
        end_ms,
        settings: (end > settings).then_some(settings..end),
        payload: next_line(text, end).min(text.len()),
    })
}

fn skip_blanks(text: &[u8], mut at: usize) -> usize {
    while matches!(text.get(at), Some(b' ' | b'\t')) {
        at += 1;
    }
    at
}

/// The position of the first CR or LF from `at`, or the end.
fn line_end(text: &[u8], at: usize) -> usize {
    text[at..]
        .iter()
        .position(|&byte| byte == b'\r' || byte == b'\n')
        .map_or(text.len(), |offset| at + offset)
}

/// After the CR, the LF, or both, at `at`.
fn next_line(text: &[u8], mut at: usize) -> usize {
    if text.get(at) == Some(&b'\r') {
        at += 1;
    }
    if text.get(at) == Some(&b'\n') {
        at += 1;
    }
    at
}
