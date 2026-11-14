//! ASS subtitles: the whole page in the `Teletext` style, or the visible text
//! of a subtitle page placed in the `Subtitle` style.
//!
//! Both styles lay the visible text out as a [`Block`] and carry its
//! horizontal [`Alignment`] from page to page.

use super::grid::{Position, Row, Symbol};
use super::out::Out;
use super::settings::Palette;
use crate::teletext::display::{Colour, ROWS};
use triergon_core::Result;

/// Horizontal placement of a subtitle, kept from page to page; the decoder
/// starts centred.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Alignment {
    Left,
    #[default]
    Centre,
    Right,
}

/// Writes every row of `rows` (numbered from `first_row`): the visible ones in
/// full, the others empty. Its text still moves the alignment.
pub(super) fn write_page(
    rows: &[Row],
    first_row: usize,
    alignment: &mut Alignment,
    palette: &Palette,
    out: &mut Out<'_>,
) -> Result<()> {
    if let Some(block) = Block::new(rows, first_row) {
        block.align(alignment);
    }
    let mut pen = Pen::new(palette);
    for row in rows {
        if row.iter().any(|p| p.visible) {
            for position in row {
                pen.cell(position, out)?;
            }
        }
        out.str(" \\N")?;
    }
    Ok(())
}

/// Writes the visible text of `rows` (numbered from `first_row`) and reports
/// whether there was any.
pub(super) fn write_subtitle(
    rows: &[Row],
    first_row: usize,
    alignment: &mut Alignment,
    palette: &Palette,
    out: &mut Out<'_>,
) -> Result<bool> {
    let Some(block) = Block::new(rows, first_row) else {
        return Ok(false);
    };
    let horizontal = block.align(alignment);
    let lines = block.lines();
    let (top, bottom) = (lines[0].row, lines[lines.len() - 1].row);
    let vertical = Vertical::of(top);
    out.str("{\\an")?;
    out.number(vertical.base() + horizontal as u32)?;
    out.str("}")?;
    if vertical == Vertical::Top {
        blank_lines(top.saturating_sub(1) / 2, out)?;
    }
    // Centred text is written line by line; otherwise every line fills the
    // block, so that the ASS renderer keeps the teletext layout.
    let fill = !block.centres();
    let mut pen = Pen::new(palette);
    let mut previous_row = None;
    for line in lines {
        if let Some(previous_row) = previous_row {
            blank_lines((line.row - previous_row - 1) / 2, out)?;
        }
        previous_row = Some(line.row);
        let (from, to) = match horizontal {
            Alignment::Left => (block.left, line.last),
            Alignment::Right => (line.first, block.right),
            Alignment::Centre if fill => (block.left, block.right),
            Alignment::Centre => (line.first, line.last),
        };
        for position in &rows[line.row - first_row][from..=to] {
            pen.cell(position, out)?;
        }
        out.str(" \\N")?;
    }
    if vertical == Vertical::Bottom {
        blank_lines(23usize.saturating_sub(bottom) / 2, out)?;
    }
    Ok(true)
}

/// Columns of the visible text of one row.
#[derive(Clone, Copy, Debug)]
struct Line {
    row: usize,
    first: usize,
    last: usize,
}

/// The rows with visible text and the columns they span together.
#[derive(Debug)]
struct Block {
    lines: [Line; ROWS],
    count: usize,
    left: usize,
    right: usize,
}

impl Block {
    fn new(rows: &[Row], first_row: usize) -> Option<Self> {
        let mut block = Self {
            lines: [Line {
                row: 0,
                first: 0,
                last: 0,
            }; ROWS],
            count: 0,
            left: usize::MAX,
            right: 0,
        };
        let ink = |p: &Position| p.visible && p.symbol.is_ink();
        for (index, row) in rows.iter().enumerate() {
            if let (Some(first), Some(last)) = (row.iter().position(ink), row.iter().rposition(ink))
            {
                block.lines[block.count] = Line {
                    row: first_row + index,
                    first,
                    last,
                };
                block.count += 1;
                block.left = block.left.min(first);
                block.right = block.right.max(last);
            }
        }
        (block.count > 0).then_some(block)
    }

    fn lines(&self) -> &[Line] {
        &self.lines[..self.count]
    }

    /// Whether the lines sit in the middle of the block (the balance of each
    /// line's margins differs from the previous line's by at most one column)
    /// and none is left of the page centre.
    fn centres(&self) -> bool {
        let balance = |l: &Line| (l.first - self.left) as isize - (self.right - l.last) as isize;
        let lines = self.lines();
        lines.iter().all(|l| l.first + l.last >= 36)
            && lines
                .windows(2)
                .all(|pair| balance(&pair[0]).abs_diff(balance(&pair[1])) <= 1)
    }

    fn allows(&self, alignment: Alignment) -> bool {
        let lines = self.lines();
        match alignment {
            Alignment::Left => self.left <= 5 && lines.iter().all(|l| l.first == self.left),
            Alignment::Right => self.right >= 38 && lines.iter().all(|l| l.last == self.right),
            Alignment::Centre => self.centres(),
        }
    }

    /// The block's alignment: the previous one while the block allows it,
    /// else centred, left or right text, which `previous` becomes. A block
    /// that allows none is centred and leaves `previous` as it was.
    fn align(&self, previous: &mut Alignment) -> Alignment {
        let candidates = [
            *previous,
            Alignment::Centre,
            Alignment::Left,
            Alignment::Right,
        ];
        match candidates.into_iter().find(|&a| self.allows(a)) {
            Some(alignment) => {
                *previous = alignment;
                alignment
            }
            None => Alignment::Centre,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vertical {
    Bottom,
    Middle,
    Top,
}

impl Vertical {
    /// The placement of a block whose first line is on `row`.
    fn of(row: usize) -> Self {
        match row {
            0..=6 => Self::Top,
            7..=14 => Self::Middle,
            _ => Self::Bottom,
        }
    }

    /// The `\an` value of left alignment.
    fn base(self) -> u32 {
        match self {
            Self::Bottom => 1,
            Self::Middle => 4,
            Self::Top => 7,
        }
    }
}

fn blank_lines(count: usize, out: &mut Out<'_>) -> Result<()> {
    (0..count).try_for_each(|_| out.str(" \\N"))
}

/// The colours in effect in the event text; tags are written on change.
struct Pen<'a> {
    foreground: Colour,
    background: Colour,
    palette: &'a Palette,
}

impl<'a> Pen<'a> {
    fn new(palette: &'a Palette) -> Self {
        Self {
            foreground: Colour::WHITE,
            background: Colour::BLACK,
            palette,
        }
    }

    fn cell(&mut self, position: &Position, out: &mut Out<'_>) -> Result<()> {
        if position.foreground != self.foreground {
            self.foreground = position.foreground;
            out.str("{\\c&H")?;
            out.hex24(self.palette.bgr(position.foreground))?;
            out.str("&}")?;
        }
        if position.background != self.background {
            self.background = position.background;
            out.str("{\\3c&H")?;
            out.hex24(self.palette.bgr(position.background))?;
            out.str("&}")?;
        }
        match position.symbol {
            Symbol::Char(c) if position.visible && position.symbol.is_ink() => match c {
                '\\' => out.str("\\\\"),
                '{' => out.str("\\{"),
                '}' => out.str("\\}"),
                c => out.char(c),
            },
            _ => out.str("\\h"),
        }
    }
}
