//! Byte-string primitives with libass's character classes.
//!
//! Scripts are byte strings: libass never validates UTF-8, and every input is
//! read only up to its first NUL byte, as libass reads C strings. The slices
//! here never contain NUL; their end plays the part of the terminator.

/// Spaces and tabs, the only blanks libass trims around fields and tokens.
pub(crate) const fn is_blank(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// The C locale's `isspace`, which numeric conversions skip.
pub(crate) const fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `s` up to its first NUL byte.
pub(crate) fn until_nul(s: &[u8]) -> &[u8] {
    s.iter().position(|&c| c == 0).map_or(s, |end| &s[..end])
}

/// `s` without leading spaces and tabs.
pub(crate) fn trim_start(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| !is_blank(c)).unwrap_or(s.len());
    &s[start..]
}

/// `s` without trailing spaces and tabs.
pub(crate) fn trim_end(s: &[u8]) -> &[u8] {
    let end = s.iter().rposition(|&c| !is_blank(c)).map_or(0, |i| i + 1);
    &s[..end]
}

/// Whether `s` starts with `prefix`, ignoring ASCII case (libass's
/// `ass_strncasecmp(s, prefix, len) == 0`).
pub(crate) fn starts_with_ignore_case(s: &[u8], prefix: &[u8]) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Splits the next comma-separated token off `rest`, as libass's `next_token`:
/// leading blanks are skipped, an exhausted string yields `None`, and the
/// token is trimmed at the end only when `trim` is set.
pub(crate) fn next_token<'a>(rest: &mut &'a [u8], trim: bool) -> Option<&'a [u8]> {
    let s = trim_start(rest);
    if s.is_empty() {
        *rest = s;
        return None;
    }
    let (token, after) = match s.iter().position(|&c| c == b',') {
        Some(comma) => (&s[..comma], &s[comma + 1..]),
        None => (s, &s[s.len()..]),
    };
    *rest = after;
    Some(if trim { trim_end(token) } else { token })
}

#[cfg(test)]
mod tests {
    use super::{next_token, starts_with_ignore_case, trim_end, trim_start, until_nul};

    #[test]
    fn tokens_split_on_commas_and_trim_as_asked() {
        let mut rest: &[u8] = b"  a , b,,c ";
        assert_eq!(next_token(&mut rest, true), Some(&b"a"[..]));
        assert_eq!(next_token(&mut rest, false), Some(&b"b"[..]));
        // An empty field is a token; the blank before the end is not.
        assert_eq!(next_token(&mut rest, false), Some(&b""[..]));
        assert_eq!(next_token(&mut rest, false), Some(&b"c "[..]));
        assert_eq!(next_token(&mut rest, false), None);
    }

    #[test]
    fn helpers_follow_c_strings() {
        assert_eq!(until_nul(b"ab\0cd"), b"ab");
        assert_eq!(trim_start(b" \t x "), b"x ");
        assert_eq!(trim_end(b" x \t"), b" x");
        assert!(starts_with_ignore_case(b"[script info]x", b"[Script Info]"));
        assert!(!starts_with_ignore_case(b"[Script", b"[Script Info]"));
    }
}
