//! Characters of event text: libass's lenient UTF-8 and its escapes.

use crate::WrapStyle;

/// libass's `ass_utf8_get_char`: a lead byte with one to four continuation
/// bytes and exactly that many following (overlong forms and surrogates are
/// accepted); anything else yields its first byte alone. Returns the code and
/// the bytes used; the end of `s` reads as NUL.
pub(crate) fn utf8_char(s: &[u8]) -> (u32, usize) {
    let Some(&lead) = s.first() else {
        return (0, 0);
    };
    let ones = lead.leading_ones() as usize;
    let continuation = ones.wrapping_sub(1); // bytes the lead announces
    if (1..=4).contains(&continuation) {
        let mut code = u32::from(lead) & (0x7f >> ones);
        let mut used = 1;
        while let Some(&c) = s.get(used).filter(|&&c| c & 0xc0 == 0x80) {
            if used > continuation {
                return (u32::from(lead), 1);
            }
            code = code << 6 | u32::from(c & 0x3f);
            used += 1;
        }
        if used == continuation + 1 {
            return (code, used);
        }
    }
    (u32::from(lead), 1)
}

/// libass's `ass_get_next_char` for text outside override blocks: a tab is a
/// space, `\N` a line break (and `\n` too under wrap style 2, else a space),
/// `\h` a no-break space, `\{` and `\}` braces. Returns the code and the
/// bytes used; 0 ends the text.
pub(crate) fn next_char(s: &[u8], wrap_style: WrapStyle) -> (u32, usize) {
    match s {
        [b'\t', ..] => (u32::from(b' '), 1),
        [b'\\', b'N', ..] => (u32::from(b'\n'), 2),
        [b'\\', b'n', ..] if wrap_style == WrapStyle::Manual => (u32::from(b'\n'), 2),
        [b'\\', b'n', ..] => (u32::from(b' '), 2),
        [b'\\', b'h', ..] => (0xa0, 2),
        [b'\\', c @ (b'{' | b'}'), ..] => (u32::from(*c), 2),
        _ => utf8_char(s),
    }
}

#[cfg(test)]
mod tests {
    use super::{next_char, utf8_char};
    use crate::WrapStyle::{Manual, Smart};

    #[test]
    fn utf8_is_decoded_leniently() {
        assert_eq!(utf8_char("é".as_bytes()), (0xe9, 2));
        assert_eq!(utf8_char("😀".as_bytes()), (0x1f600, 4));
        assert_eq!(utf8_char(&[0xc0, 0x80]), (0, 2)); // overlong NUL
        assert_eq!(utf8_char(&[0xc3, 0xa9, 0x80]), (0xc3, 1)); // extra continuation
        assert_eq!(utf8_char(&[0xe2, 0x82]), (0xe2, 1)); // short
        assert_eq!(utf8_char(&[0x80]), (0x80, 1));
        assert_eq!(utf8_char(&[0xf8, 0x88, 0x80, 0x80, 0x80]), (0x200000, 5));
        assert_eq!(utf8_char(&[0xfc, 0x80]), (0xfc, 1));
        assert_eq!(utf8_char(b""), (0, 0));
    }

    #[test]
    fn escapes_follow_libass() {
        assert_eq!(next_char(b"\\N", Smart), (10, 2));
        assert_eq!(next_char(b"\\n", Smart), (32, 2));
        assert_eq!(next_char(b"\\n", Manual), (10, 2));
        assert_eq!(next_char(b"\\h", Smart), (0xa0, 2));
        assert_eq!(next_char(b"\\x", Smart), (u32::from(b'\\'), 1));
        assert_eq!(next_char(b"\t", Smart), (32, 1));
    }
}
