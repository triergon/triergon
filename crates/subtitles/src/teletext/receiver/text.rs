//! Plain-text subtitles: libzvbi's text print of the page, the reference's
//! space chopping and its escaping into an ASS event.

use super::grid::{Grid, Symbol, WIDTH};
use super::out::Out;
use crate::teletext::display::ROWS;
use triergon_core::Result;

/// The longest print: every position four UTF-8 bytes, and the newlines.
const PRINT_BYTES: usize = ROWS * (WIDTH * 4 + 1);

/// Writes the text of the page and reports whether anything was written.
///
/// The print is a C string: the rows joined by newlines (without the header
/// row when `chop_top`) and cut at the first NUL. With `visible_only` only
/// visible positions and row 24 print; the others are spaces.
///
/// Chopping takes the print line by line, where a newline right after a
/// non-ASCII byte does not end a line. Only newline-terminated lines are kept,
/// without their leading and trailing spaces (a space right after a non-ASCII
/// byte stays), and empty ones are dropped.
pub(super) fn write(
    grid: &Grid,
    visible_only: bool,
    chop_top: bool,
    chop_spaces: bool,
    out: &mut Out<'_>,
) -> Result<bool> {
    let mut buffer = [0; PRINT_BYTES];
    let print = print_page(grid, visible_only, chop_top, &mut buffer);
    let start = out.len();
    if !chop_spaces {
        escape(print, out)?;
        return Ok(out.len() > start);
    }
    let mut rest = print;
    while let Some(end) = line_end(rest) {
        let line = chop(&rest[..end]);
        if !line.is_empty() {
            if out.len() > start {
                out.str("\\N")?;
            }
            escape(line, out)?;
        }
        rest = &rest[end + 1..];
    }
    Ok(out.len() > start)
}

/// Prints the rows into `buffer` and returns the C string.
fn print_page<'a>(
    grid: &Grid,
    visible_only: bool,
    chop_top: bool,
    buffer: &'a mut [u8; PRINT_BYTES],
) -> &'a [u8] {
    let first = usize::from(chop_top);
    let last = grid.rows.len() - 1;
    let mut length = 0;
    for (index, row) in grid.rows.iter().enumerate().skip(first) {
        if index > first {
            buffer[length] = b'\n';
            length += 1;
        }
        let masked = visible_only && index < last;
        for position in row {
            let c = match position.symbol {
                _ if masked && !position.visible => ' ',
                Symbol::Char(c) | Symbol::Mosaic(c) => c,
                Symbol::Nul => return &buffer[..length],
            };
            length += c.encode_utf8(&mut buffer[length..]).len();
        }
    }
    &buffer[..length]
}

/// The newline that ends the first line of `text`: the first one not right
/// after a non-ASCII byte. A row whose 41st column holds a mosaic (a
/// private-use code point) therefore runs on into the next row, and a line
/// that reaches the end of the print without such a newline is not one.
fn line_end(text: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = text[from..].iter().position(|&b| b == b'\n') {
        let at = from + offset;
        if at == 0 || text[at - 1].is_ascii() {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// A line without its leading spaces and its trailing ones, except that the
/// space right after a non-ASCII byte stays: `"   £ab  "` becomes `"£ab"`,
/// `"ab£   "` becomes `"ab£ "` and `"ab£ x  "` becomes `"ab£ x"`.
fn chop(line: &[u8]) -> &[u8] {
    let start = line.iter().position(|&b| b != b' ').unwrap_or(line.len());
    let mut line = &line[start..];
    while let [rest @ .., before, b' '] = line
        && before.is_ascii()
    {
        line = &line[..=rest.len()];
    }
    line
}

/// The reference's ASS escaping of plain text.
fn escape(text: &[u8], out: &mut Out<'_>) -> Result<()> {
    let special = |b: &u8| matches!(b, b'\n' | b'\\' | b'{');
    for piece in text.split_inclusive(special) {
        match piece.split_last() {
            Some((&last, body)) if special(&last) => {
                out.bytes(body)?;
                out.str(match last {
                    b'\n' => "\\N",
                    b'\\' => "\\\u{2060}",
                    _ => "\\{{}",
                })?;
            }
            _ => out.bytes(piece)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{chop, line_end};

    /// Rows of pages the reference printed, as `txt_chop_spaces=0` shows them,
    /// and the lines `txt_chop_spaces=1` made of them.
    #[test]
    fn chop_keeps_the_space_after_a_non_ascii_byte() {
        let (blank, full) = (" ".repeat(41), "x".repeat(40));
        for (row, chopped) in [
            ("   Hello  world                          ", "Hello  world"),
            ("    x                                    ", "x"),
            (full.as_str(), full.as_str()),
            ("                                       £ ", "£ "),
            ("                                      £  ", "£ "),
            ("ab£ x                                    ", "ab£ x"),
            ("   £ab                                   ", "£ab"),
            ("ab←                                      ", "ab← "),
            ("a£ £                                     ", "a£ £ "),
            ("  âôû                                    ", "âôû "),
            (blank.as_str(), ""),
            ("", ""),
        ] {
            assert_eq!(chop(row.as_bytes()), chopped.as_bytes(), "{row:?}");
        }
        // A row ending in a mosaic runs on into a blank row, whose spaces go.
        let mosaic = format!("\u{ee7f}\u{ee7f}\u{ee7f}\n{blank}");
        assert_eq!(
            chop(mosaic.as_bytes()),
            "\u{ee7f}\u{ee7f}\u{ee7f}\n".as_bytes()
        );
    }

    #[test]
    fn a_newline_after_a_non_ascii_byte_ends_no_line() {
        assert_eq!(line_end(b"ab \ncd\n"), Some(3));
        assert_eq!(line_end(b"\nab"), Some(0));
        assert_eq!(line_end("ab £\nnext \n".as_bytes()), Some(11));
        assert_eq!(line_end("\u{ee7f}\n\u{ee7f}\nx".as_bytes()), None);
        assert_eq!(line_end(b"no newline"), None);
        assert_eq!(line_end(b""), None);
    }
}
