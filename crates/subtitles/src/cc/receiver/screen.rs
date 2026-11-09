//! The receiver's caption memory: two 15 x 32 screens of styled cells.
//!
//! A row is written like a C string: every character is followed by a
//! terminator in the next cell, and a row reads up to its first terminator.
//! Erasing a screen only forgets which rows are in use; their cells stay and
//! reappear when a later character marks the row used again.

use crate::cc::line21::{Charset, Color};

pub(super) const ROWS: usize = 15;
pub(super) const COLUMNS: usize = 32;

/// The receiver's four fonts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Font {
    #[default]
    Regular,
    Italics,
    Underlined,
    UnderlinedItalics,
}

impl Font {
    pub(super) const fn new(italics: bool, underline: bool) -> Self {
        match (italics, underline) {
            (false, false) => Self::Regular,
            (true, false) => Self::Italics,
            (false, true) => Self::Underlined,
            (true, true) => Self::UnderlinedItalics,
        }
    }
}

/// The attributes a cell is drawn with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Style {
    pub(super) font: Font,
    pub(super) color: Color,
    /// The background colour; the receiver draws semi-transparent backgrounds
    /// opaque and keeps the colour before a transparent one.
    pub(super) background: Color,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            font: Font::Regular,
            color: Color::White,
            background: Color::Black,
        }
    }
}

/// One cell. A `code` of 0 terminates its row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Cell {
    pub(super) code: u8,
    pub(super) charset: Charset,
    pub(super) style: Style,
}

impl Cell {
    /// A blank of the basic set: what a row's indentation is made of.
    pub(super) fn is_blank(&self) -> bool {
        self.code == b' ' && self.charset == Charset::Basic
    }
}

pub(super) type Row = [Cell; COLUMNS];

/// One screen: its rows and which of them are in use.
#[derive(Clone, Debug)]
pub(super) struct Screen {
    rows: [Row; ROWS],
    used: u16,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            rows: [[Cell::default(); COLUMNS]; ROWS],
            used: 0,
        }
    }
}

impl Screen {
    pub(super) fn is_used(&self, row: usize) -> bool {
        self.used & (1 << row) != 0
    }

    pub(super) fn mark_used(&mut self, row: usize) {
        self.used |= 1 << row;
    }

    pub(super) fn set_used(&mut self, row: usize, used: bool) {
        if used {
            self.mark_used(row);
        } else {
            self.used &= !(1 << row);
        }
    }

    pub(super) fn erase(&mut self) {
        self.used = 0;
    }

    pub(super) fn is_blank(&self) -> bool {
        self.used == 0
    }

    pub(super) fn row_mut(&mut self, row: usize) -> &mut Row {
        &mut self.rows[row]
    }

    /// The rows in use, top to bottom.
    pub(super) fn used_rows(&self) -> impl Iterator<Item = (usize, &Row)> {
        self.rows
            .iter()
            .enumerate()
            .filter(|&(row, _)| self.is_used(row))
    }

    /// Copies the cells of the row below `row` into it, and its use if it is
    /// used; below the last row lies an empty one.
    pub(super) fn take_row_below(&mut self, row: usize) {
        let below = row + 1;
        if below < ROWS {
            self.rows[row] = self.rows[below];
            if self.is_used(below) {
                self.mark_used(row);
            }
        } else {
            self.rows[row] = [Cell::default(); COLUMNS];
        }
    }
}
