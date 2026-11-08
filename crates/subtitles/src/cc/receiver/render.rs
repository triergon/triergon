//! A screen as the receiver's ASS text.
//!
//! Each row in use becomes `{\an7}{\pos(x,y)}` and its text, rows joined by
//! `\N`. Positions place the 32 x 15 grid inside the central 80% of a 384 x 288
//! frame. A row starts at the screen's shared indentation and ends at its
//! first terminator; spaces before its first other character become `\h`.
//! Tags open where the style changes from the cell drawn before, across rows:
//! the font first, then the colour, then the background.

use super::screen::{COLUMNS, Cell, Font, ROWS, Row, Screen, Style};
use crate::cc::line21::{Character, Charset, Color};
use crate::cc::text::Text;
use triergon_core::Result;

/// The ASS text of `screen`, replacing `out`.
pub(super) fn render(screen: &Screen, out: &mut Text) -> Result<()> {
    out.clear();
    let indent = shared_indent(screen);
    let mut drawn = Style::default();
    for (n, (row, cells)) in screen.used_rows().enumerate() {
        let blanks = leading_blanks(cells);
        let start = indent.min(blanks);
        if n > 0 {
            out.put(b"\\N")?;
        }
        position(out, start, row)?;
        let mut seen_text = false;
        for cell in cells[start..].iter().take_while(|cell| cell.code != 0) {
            if cell.style != drawn {
                tags(out, drawn, cell.style)?;
                drawn = cell.style;
            }
            match glyph(cell) {
                Some(ch) => {
                    let mut utf8 = [0; 4];
                    out.put(ch.encode_utf8(&mut utf8).as_bytes())?;
                }
                None if cell.code == b' ' && !seen_text => {
                    out.put(b"\\h")?;
                    continue;
                }
                None => out.put(&[cell.code])?,
            }
            seen_text = true;
        }
    }
    Ok(())
}

/// The indentation every row starts from: the least leading blank count, except
/// that a row without indentation does not count once a later row has some.
fn shared_indent(screen: &Screen) -> usize {
    screen
        .used_rows()
        .map(|(_, cells)| leading_blanks(cells))
        .fold(0, |indent, blanks| {
            if indent == 0 || blanks < indent {
                blanks
            } else {
                indent
            }
        })
}

fn leading_blanks(cells: &Row) -> usize {
    cells.iter().take_while(|cell| cell.is_blank()).count()
}

fn position(out: &mut Text, column: usize, row: usize) -> Result<()> {
    out.put(b"{\\an7}{\\pos(")?;
    decimal(out, COLUMN_X[column])?;
    out.put(b",")?;
    decimal(out, ROW_Y[row])?;
    out.put(b")}")
}

/// The receiver's `\pos` coordinates of each column and row: about
/// `384 * (0.1 + 0.8 * column / 32)` and `288 * (0.1 + 0.8 * row / 15)`,
/// truncated (row 9 sits at 166). Recovered from reference output.
const COLUMN_X: [usize; COLUMNS + 1] = [
    38, 48, 57, 67, 76, 86, 96, 105, 115, 124, 134, 144, 153, 163, 172, 182, 192, 201, 211, 220,
    230, 240, 249, 259, 268, 278, 288, 297, 307, 316, 326, 336, 345,
];
const ROW_Y: [usize; ROWS] = [
    28, 44, 59, 74, 90, 105, 120, 136, 151, 166, 182, 197, 213, 228, 243,
];

fn decimal(out: &mut Text, value: usize) -> Result<()> {
    let digits = [
        b'0' + (value / 100 % 10) as u8,
        b'0' + (value / 10 % 10) as u8,
        b'0' + (value % 10) as u8,
    ];
    let skip = if value >= 100 {
        0
    } else if value >= 10 {
        1
    } else {
        2
    };
    out.put(&digits[skip..])
}

fn tags(out: &mut Text, from: Style, to: Style) -> Result<()> {
    if from.font != to.font {
        out.put(match from.font {
            Font::Regular => b"",
            Font::Italics => b"{\\i0}",
            Font::Underlined => b"{\\u0}",
            Font::UnderlinedItalics => b"{\\u0}{\\i0}",
        })?;
        out.put(match to.font {
            Font::Regular => b"",
            Font::Italics => b"{\\i1}",
            Font::Underlined => b"{\\u1}",
            Font::UnderlinedItalics => b"{\\u1}{\\i1}",
        })?;
    }
    if from.color != to.color {
        color_tag(out, b"{\\c&H", to.color)?;
    }
    if from.background != to.background {
        color_tag(out, b"{\\3c&H", to.background)?;
    }
    Ok(())
}

fn color_tag(out: &mut Text, tag: &[u8], color: Color) -> Result<()> {
    // ASS colours are blue, green, red.
    let bgr: &[u8; 6] = match color {
        Color::White => b"FFFFFF",
        Color::Green => b"00FF00",
        Color::Blue => b"FF0000",
        Color::Cyan => b"FFFF00",
        Color::Red => b"0000FF",
        Color::Yellow => b"00FFFF",
        Color::Magenta => b"FF00FF",
        Color::Black => b"000000",
    };
    out.put(tag)?;
    out.put(bgr)?;
    out.put(b"&}")
}

/// The glyph the receiver substitutes for a cell's code, if any: the
/// CEA-608 characters outside ASCII, with a few of its own choices. Other
/// codes are written as they are.
fn glyph(cell: &Cell) -> Option<char> {
    let character = Character {
        charset: cell.charset,
        code: cell.code,
    };
    match (cell.charset, cell.code) {
        (Charset::Basic, 0x27) => Some('’'),
        (Charset::Basic, 0x7f) => Some('█'),
        (Charset::Basic, 0x2a | 0x5c | 0x5e | 0x5f | 0x60 | 0x7b..=0x7e) => character.to_char(),
        (Charset::Basic, _) => None,
        (Charset::SpanishFrench, 0x26) => Some('´'),
        (Charset::SpanishFrench, 0x29) => Some('‘'),
        (Charset::SpanishFrench, 0x2a) => Some('-'),
        (Charset::SpanishFrench, 0x2d) => Some('·'),
        _ => character.to_char(),
    }
}
