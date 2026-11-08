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

/// The index of the newline that ends the first line, if any.
fn line_end(text: &[u8]) -> Option<usize> {
    (0..text.len()).find(|&i| text[i] == b'\n' && (i == 0 || text[i - 1] < 0x80))
}

fn chop(line: &[u8]) -> &[u8] {
    let first = line.iter().position(|&b| b != b' ').unwrap_or(line.len());
    let line = &line[first..];
    let mut end = line.len();
    while end > 0 && line[end - 1] == b' ' && !(end > 1 && line[end - 2] >= 0x80) {
        end -= 1;
    }
    &line[..end]
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
