//! The receiver's caption memory: a displayed and a hidden screen, the cursor
//! and the caption mode, driven by one byte pair at a time.

use super::render::render;
use super::screen::{COLUMNS, Cell, Font, ROWS, Screen, Style};
use crate::cc::line21::{
    Background, Channel, Charset, Color, Command, Ink, MidRow, Pair, Preamble,
};
use crate::cc::text::Text;
use triergon_core::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Characters go to the hidden screen until an end of caption shows it.
    PopOn,
    /// Characters go straight to the displayed screen.
    PaintOn,
    /// Characters go to the displayed screen's base row.
    RollUp,
    /// Characters go to the displayed screen, and a carriage return does not scroll.
    Text,
}

/// What a pair did that the decoder reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Effect {
    None,
    /// Characters reached the displayed screen.
    Touched,
    /// The snapshot was replaced and is to be reported.
    Snapshot,
}

#[derive(Clone, Copy, Debug)]
struct Cursor {
    row: usize,
    column: usize,
    style: Style,
    /// The set of the next cell stored: a special or extended character sets
    /// it, storing a cell returns it to the basic set.
    charset: Charset,
}

#[derive(Debug)]
pub(super) struct Memory {
    screens: [Screen; 2],
    shown: usize,
    mode: Mode,
    /// The rows a carriage return scrolls, set by the last roll-up command.
    roll_rows: usize,
    cursor: Cursor,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            screens: [Screen::default(), Screen::default()],
            shown: 0,
            mode: Mode::RollUp,
            roll_rows: 2,
            cursor: Cursor {
                row: 10,
                column: 0,
                style: Style::default(),
                charset: Charset::Basic,
            },
        }
    }
}

impl Memory {
    pub(super) fn is_pop_on(&self) -> bool {
        self.mode == Mode::PopOn
    }

    /// Renders the displayed screen into `snapshot`.
    pub(super) fn capture(&self, snapshot: &mut Text) -> Result<()> {
        render(&self.screens[self.shown], snapshot)
    }

    fn displayed(&mut self) -> &mut Screen {
        &mut self.screens[self.shown]
    }

    /// The screen characters go to in the current mode.
    fn written(&mut self) -> &mut Screen {
        let index = if self.mode == Mode::PopOn {
            1 - self.shown
        } else {
            self.shown
        };
        &mut self.screens[index]
    }

    /// Applies one pair (parity removed). Snapshots go to `snapshot`.
    pub(super) fn apply(
        &mut self,
        hi: u8,
        lo: u8,
        real_time: bool,
        snapshot: &mut Text,
    ) -> Result<Effect> {
        let effect = match Pair::decode(hi, lo) {
            Pair::Text { first, second } => {
                let pair = [first, second];
                self.write(Charset::Basic, &pair[..if second == 0 { 1 } else { 2 }]);
                self.touch()
            }
            Pair::Character {
                channel: Channel::First,
                character,
            } => self.character(character.charset, character.code),
            // The receiver writes any other code of the character groups as a
            // character of that group.
            Pair::Unassigned {
                first: group @ 0x11..=0x13,
                second,
            } => self.character(
                match group {
                    0x11 => Charset::Special,
                    0x12 => Charset::SpanishFrench,
                    _ => Charset::PortugueseGerman,
                },
                second,
            ),
            Pair::Preamble {
                channel: Channel::First,
                row,
                preamble,
                underline,
            } => {
                let (ink, indent) = match preamble {
                    Preamble::Ink(ink) => (ink, 0),
                    Preamble::Indent(columns) => (Ink::Color(Color::White), columns),
                };
                self.cursor.row = usize::from(row);
                self.cursor.column = 0;
                self.cursor.charset = Charset::Basic;
                self.set_ink(ink, underline);
                // Indentation fills the row with blanks but does not use it.
                for _ in 0..indent {
                    self.put(b' ');
                }
                Effect::None
            }
            Pair::MidRow {
                channel: Channel::First,
                attributes: MidRow { ink, underline },
            } => {
                self.set_ink(ink, underline);
                self.blank();
                Effect::None
            }
            Pair::BlackText {
                channel: Channel::First,
                underline,
            } => {
                self.set_ink(Ink::Italics, underline);
                self.blank();
                Effect::None
            }
            Pair::Background {
                channel: Channel::First,
                background: Background::Opaque(color) | Background::SemiTransparent(color),
            } => {
                self.cursor.style.background = color;
                Effect::None
            }
            Pair::TabOffset {
                channel: Channel::First,
                columns,
            } => {
                for _ in 0..columns {
                    self.write(Charset::Basic, b" ");
                }
                self.touch()
            }
            Pair::Command { channel, command } if channel == Channel::First || hi == 0x1c => {
                return self.command(command, real_time, snapshot);
            }
            _ => Effect::None,
        };
        Ok(effect)
    }

    fn command(
        &mut self,
        command: Command,
        real_time: bool,
        snapshot: &mut Text,
    ) -> Result<Effect> {
        match command {
            Command::ResumeCaptionLoading => self.mode = Mode::PopOn,
            Command::ResumeDirectCaptioning => self.mode = Mode::PaintOn,
            Command::ResumeTextDisplay => self.mode = Mode::Text,
            Command::RollUp(rows) => {
                self.mode = Mode::RollUp;
                self.roll_rows = usize::from(rows);
            }
            Command::DeleteToEndOfRow => {
                let (row, column) = (self.cursor.row, self.cursor.column);
                if column < COLUMNS {
                    self.written().row_mut(row)[column].code = 0;
                }
            }
            Command::EraseDisplayedMemory => {
                if !real_time {
                    self.capture(snapshot)?;
                }
                self.erase_displayed();
                if real_time {
                    snapshot.clear();
                }
                return Ok(Effect::Snapshot);
            }
            Command::EndOfCaption => {
                self.shown = 1 - self.shown;
                self.cursor.column = 0;
                self.capture(snapshot)?;
                if !real_time {
                    self.erase_displayed();
                }
                return Ok(Effect::Snapshot);
            }
            Command::CarriageReturn => {
                let reported = !real_time;
                if reported {
                    self.capture(snapshot)?;
                }
                self.roll_up();
                self.cursor.column = 0;
                if reported {
                    return Ok(Effect::Snapshot);
                }
            }
            // Only in real-time mode does the receiver erase the hidden screen.
            Command::EraseNonDisplayedMemory => {
                if real_time {
                    self.screens[1 - self.shown].erase();
                }
            }
            // The receiver ignores these.
            Command::Backspace
            | Command::AlarmOff
            | Command::AlarmOn
            | Command::FlashOn
            | Command::TextRestart => {}
        }
        Ok(Effect::None)
    }

    /// Erasing the displayed screen also ends the background attribute.
    fn erase_displayed(&mut self) {
        self.displayed().erase();
        self.cursor.style.background = Style::default().background;
    }

    /// A special or extended character; an extended one replaces the
    /// character before it.
    fn character(&mut self, charset: Charset, code: u8) -> Effect {
        if charset != Charset::Special {
            self.cursor.column = self.cursor.column.saturating_sub(1);
        }
        let codes = [code];
        self.write(charset, &codes[..usize::from(code != 0)]);
        self.touch()
    }

    fn touch(&self) -> Effect {
        if self.mode == Mode::PopOn {
            Effect::None
        } else {
            Effect::Touched
        }
    }

    fn set_ink(&mut self, ink: Ink, underline: bool) {
        let style = &mut self.cursor.style;
        match ink {
            Ink::Color(color) => {
                style.color = color;
                style.font = Font::new(false, underline);
            }
            Ink::Italics => {
                style.color = Color::White;
                style.font = Font::new(true, underline);
            }
        }
    }

    /// Marks the cursor's row of the written screen used.
    fn use_row(&mut self) {
        let row = self.cursor.row;
        self.written().mark_used(row);
    }

    /// Stores one cell at the cursor in the pending set and advances the
    /// cursor (a terminator does not advance it); past the last column nothing
    /// is stored and the set stays pending.
    fn put(&mut self, code: u8) {
        let Cursor {
            row,
            column,
            style,
            charset,
        } = self.cursor;
        if column < COLUMNS {
            self.written().row_mut(row)[column] = Cell {
                code,
                charset,
                style,
            };
            self.cursor.charset = Charset::Basic;
            if code != 0 {
                self.cursor.column = column + 1;
            }
        }
    }

    /// Writes characters of `charset` as a caption does: the row becomes used
    /// and ends after them.
    fn write(&mut self, charset: Charset, codes: &[u8]) {
        self.use_row();
        self.cursor.charset = charset;
        for &code in codes {
            self.put(code);
        }
        self.put(0);
    }

    /// A mid-row code's blank: it uses the row but leaves what follows.
    fn blank(&mut self) {
        self.use_row();
        self.put(b' ');
    }

    /// Scrolls the roll-up window (the base row and the rows above it) up by
    /// one row, in every mode but text. Rows outside the window are left
    /// unused. If any row remains used, each window row takes the cells of the
    /// row below it (under the last row lies an empty one) and becomes used if
    /// that row was; a row in use stays in use. The base row is left unused.
    fn roll_up(&mut self) {
        if self.mode == Mode::Text {
            return;
        }
        let base = self.cursor.row;
        let top = (base + 1).saturating_sub(self.roll_rows);
        let screen = self.written();
        for row in (0..ROWS).filter(|&row| row < top || row > base) {
            screen.set_used(row, false);
        }
        if !screen.is_blank() {
            for row in top..=base {
                screen.take_row_below(row);
            }
        }
        screen.set_used(base, false);
    }
}
