//! The page as the reference's libzvbi lays it out: 41 columns, its own page
//! number in the header row and private-use code points for G1 and G3
//! mosaics.

use crate::teletext::display::{COLUMNS, Cell, Colour, Display, Glyph, Part, ROWS};
use crate::teletext::presentation::Remapping;

/// Colour lookup table 1 entry 0, transparent.
const TRANSPARENT: Colour = Colour(8);

/// Columns of the laid-out page: the 40 teletext columns and one more.
pub(super) const WIDTH: usize = COLUMNS + 1;

/// What a position prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Symbol {
    Char(char),
    /// A block mosaic, as its private-use code point.
    Mosaic(char),
    /// A character without a Unicode form: libzvbi prints it as NUL.
    Nul,
}

impl Symbol {
    const SPACE: Self = Self::Char(' ');

    /// A character the ASS event shows: not a space, a mosaic, a NUL or a
    /// private-use code point.
    pub(super) fn is_ink(self) -> bool {
        matches!(self, Self::Char(c) if !matches!(c, ' ' | '\u{a0}' | '\u{e000}'..='\u{f8ff}'))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Position {
    pub(super) symbol: Symbol,
    pub(super) foreground: Colour,
    pub(super) background: Colour,
    pub(super) visible: bool,
}

impl Position {
    pub(super) const BLANK: Self = Self {
        symbol: Symbol::SPACE,
        foreground: Colour::WHITE,
        background: Colour::BLACK,
        visible: false,
    };
}

pub(super) type Row = [Position; WIDTH];

#[derive(Clone, Debug)]
pub(super) struct Grid {
    pub(super) rows: [Row; ROWS],
}

impl Grid {
    /// The layout of `display`. A page presented with its X/26/0 packet
    /// (`enhanced`) hides positions above row 24 transparent (colour 8) in
    /// foreground and background, and its invisible characters (other than in
    /// row 24) do not count as used in column 0.
    pub(super) fn new(display: &Display, enhanced: bool, remapping: Remapping) -> Self {
        let mut rows = [[Position::BLANK; WIDTH]; ROWS];
        for (r, (out, cells)) in rows.iter_mut().zip(&display.rows).enumerate() {
            for (c, (position, cell)) in out.iter_mut().zip(cells).enumerate() {
                let (foreground, background) = cell.shown_colours();
                let transparent = enhanced
                    && r < ROWS - 1
                    && foreground == TRANSPARENT
                    && background == TRANSPARENT;
                *position = Position {
                    symbol: symbol(cell),
                    foreground,
                    background,
                    visible: display.is_visible(r, c) && !transparent,
                };
            }
        }
        // The extra column continues column 39 in the header row, and in the
        // others when one of them starts with a position other than a blank
        // space and none ends with a lone mosaic, one column 38 does not
        // repeat (either half of a doubled one repeats it): a repeated mosaic
        // continues as it is, anything else as a space in column 39's colours.
        // Otherwise it is a space in the row's starting colours.
        let last = rows.len() - 1;
        let uses_first_column = rows.iter().enumerate().skip(1).any(|(index, row)| {
            let first = row[0];
            let shown = first.visible || !enhanced || index == last;
            (shown && first.symbol != Symbol::SPACE) || first.background != Colour::BLACK
        });
        let ends_in_lone_mosaic = |cells: &[Cell; COLUMNS]| {
            let [.., before, last] = cells;
            is_mosaic(last) && last.size.height != Part::Second && before.glyph != last.glyph
        };
        let continues = uses_first_column && !display.rows[1..].iter().any(ends_in_lone_mosaic);
        let blank = Position {
            foreground: remapping.foreground(7),
            background: remapping.background(0),
            ..Position::BLANK
        };
        for (index, (row, cells)) in rows.iter_mut().zip(&display.rows).enumerate() {
            let last_position = row[COLUMNS - 1];
            row[COLUMNS] = if index > 0 && !continues {
                blank
            } else if is_mosaic(&cells[COLUMNS - 1]) && !ends_in_lone_mosaic(cells) {
                last_position
            } else {
                Position {
                    symbol: Symbol::SPACE,
                    ..last_position
                }
            };
        }
        Self { rows }
    }
}

/// Whether a position holds a G1 or G3 mosaic.
fn is_mosaic(cell: &Cell) -> bool {
    matches!(cell.glyph, Glyph::Mosaic { .. } | Glyph::Smooth(_))
}

fn symbol(cell: &Cell) -> Symbol {
    if cell.size.width == Part::Second || cell.size.height == Part::Second {
        return Symbol::SPACE;
    }
    match cell.glyph {
        Glyph::Char(c) => Symbol::Char(c),
        Glyph::Mosaic { code, separated } => {
            let offset = if separated {
                u32::from(code) - 0x20
            } else {
                u32::from(code)
            };
            Symbol::Mosaic(char::from_u32(0xEE00 + offset).unwrap_or(' '))
        }
        Glyph::Smooth(code) => {
            Symbol::Mosaic(char::from_u32(0xEF00 + u32::from(code)).unwrap_or(' '))
        }
        Glyph::Uncomposable => Symbol::Nul,
    }
}

/// libzvbi's codes for columns 0 to 7 of the header row: the page number and
/// the low byte of the subcode in hexadecimal, green, as `\x02PPP.SS\x07`.
pub(super) fn header_start(page: u16, subcode: u16) -> [u8; 8] {
    let subcode = if subcode == 0x3F7F { 0 } else { subcode & 0xFF };
    let hex = |digit: u16| b"0123456789ABCDEF"[usize::from(digit & 0xF)];
    [
        0x02,
        hex(page >> 8),
        hex(page >> 4),
        hex(page),
        b'.',
        hex(subcode >> 4),
        hex(subcode),
        0x07,
    ]
}
