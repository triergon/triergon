//! Worker trace protocol for the differential harness (never shipped).
//!
//! Workers print one JSON document per operation. Strings use [`push_quoted`],
//! byte fields lower-case hex ([`push_hex`]). Timed operations prove equal work
//! with the ordered work digest in [`digest`]: `h <- 31*h + v (mod 2^64)` over
//! every field and payload byte, never sampled. `harness/reference.c`
//! implements the same polynomial; the golden vectors below pin it. The decode
//! workers fingerprint their output with the [`lanes`] digest instead.
#![forbid(unsafe_code)]

pub mod digest;
pub mod lanes;
pub mod md5;

use std::{fmt::Write as _, io};

/// Append `bytes` as lower-case hex.
pub fn push_hex(out: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    out.reserve(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
}

/// `bytes` as lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    push_hex(&mut out, bytes);
    out
}

/// Stream `bytes` as lower-case hex.
pub fn write_hex(out: &mut impl io::Write, bytes: &[u8]) -> io::Result<()> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for &byte in bytes {
        out.write_all(&[
            DIGITS[usize::from(byte >> 4)],
            DIGITS[usize::from(byte & 15)],
        ])?;
    }
    Ok(())
}

/// Decode test-vector hex. `"~"` and `"-"` denote an empty payload. Panics on
/// odd length or a non-hex digit: vectors are authored, never untrusted input.
pub fn unhex(text: &str) -> Vec<u8> {
    if text == "~" || text == "-" {
        return Vec::new();
    }
    assert_eq!(text.len() % 2, 0, "odd-length hex vector");
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("invalid hex digit in vector"),
    };
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[high, low]| (nibble(high) << 4) | nibble(low))
        .collect()
}

/// Append `value` as a JSON string literal.
pub fn push_quoted(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => write!(out, "\\u{:04x}", u32::from(c)).expect("String write"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Stream `value` as a JSON string literal.
pub fn write_quoted(out: &mut impl io::Write, value: &str) -> io::Result<()> {
    let mut text = String::with_capacity(value.len() + 2);
    push_quoted(&mut text, value);
    out.write_all(text.as_bytes())
}

/// `value` as a JSON string literal.
pub fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    push_quoted(&mut out, value);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_marks_empty_vectors() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(unhex(&hex(&bytes)), bytes);
        assert_eq!(hex(&[0x0f, 0xa0]), "0fa0");
        assert_eq!(unhex("0FA0"), [0x0f, 0xa0]);
        assert!(unhex("~").is_empty() && unhex("-").is_empty() && unhex("").is_empty());
    }

    #[test]
    fn quoting_escapes_json_controls() {
        assert_eq!(
            quoted("a\"b\\c\n\r\t\u{1}é"),
            "\"a\\\"b\\\\c\\n\\r\\t\\u0001é\""
        );
    }
}
