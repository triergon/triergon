use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvdDate(pub u16);

impl DvdDate {
    /// Exact 90 kHz tick count for the 1024-tick control-date representation.
    /// This preserves the integer source date, independently of milliseconds.
    pub fn ticks_90k(self) -> u32 {
        u32::from(self.0) * 1024
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvdArea {
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
}

impl DvdArea {
    pub fn dimensions(self) -> Result<(u16, u16)> {
        let width = self
            .right
            .checked_sub(self.left)
            .and_then(|n| n.checked_add(1));
        let height = self
            .bottom
            .checked_sub(self.top)
            .and_then(|n| n.checked_add(1));
        match (width, height) {
            (Some(w), Some(h)) => Ok((w, h)),
            _ => Err(Error::Invalid("DVD reversed or overflowing display area")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldOffsets {
    pub top: u16,
    pub bottom: u16,
}

/// Borrowed view of exactly one declared-size SPU. Trailing packets are not
/// silently consumed; a caller assembling PES fragments owns that boundary.
#[derive(Clone, Copy, Debug)]
pub struct DvdPacket<'a> {
    raw: &'a [u8],
    control: usize,
}

impl<'a> DvdPacket<'a> {
    pub fn parse(raw: &'a [u8]) -> Result<Self> {
        let header = raw.first_chunk::<4>().ok_or(Error::Truncated)?;
        let size = usize::from(u16::from_be_bytes([header[0], header[1]]));
        let control = usize::from(u16::from_be_bytes([header[2], header[3]]));
        if size > raw.len() {
            return Err(Error::Truncated);
        }
        if size != raw.len() {
            return Err(Error::Invalid("DVD trailing packet bytes"));
        }
        if control < 4 || control + 5 > size {
            return Err(Error::Invalid("DVD control offset"));
        }
        Ok(Self { raw, control })
    }

    /// Rebuild a view with the offset already validated for these same bytes.
    pub(super) fn with_control_offset(raw: &'a [u8], control: usize) -> Self {
        Self { raw, control }
    }

    pub fn raw(self) -> &'a [u8] {
        self.raw
    }
    pub fn control_offset(self) -> usize {
        self.control
    }
    pub fn pixel_data(self) -> &'a [u8] {
        &self.raw[4..self.control]
    }

    pub fn controls(self, maximum: usize, max_commands: usize) -> Controls<'a> {
        Controls {
            packet: self,
            next: Some(self.control),
            count: 0,
            maximum,
            max_commands,
        }
    }

    pub(super) fn control_at(self, at: usize, max_commands: usize) -> Result<Control<'a>> {
        if at < self.control {
            return Err(Error::Invalid("DVD control before control area"));
        }
        Controls {
            packet: self,
            next: Some(at),
            count: 0,
            maximum: 1,
            max_commands,
        }
        .parse(at)
    }

    /// Receiver commands are bounded by the complete assembled SPU. A raw link
    /// is retained even when the receiver stops rather than following it.
    pub(super) fn receiver_control_at(self, at: usize, max_commands: usize) -> Result<Control<'a>> {
        let raw = self
            .raw
            .get(at..)
            .filter(|tail| at >= self.control && tail.len() > 4)
            .ok_or(Error::Invalid("DVD receiver control offset"))?;
        let [date_high, date_low, next_high, next_low, commands @ ..] = raw else {
            return Err(Error::Invalid("DVD receiver control offset"));
        };
        Ok(Control {
            offset: at,
            date: DvdDate(u16::from_be_bytes([*date_high, *date_low])),
            next_offset: u16::from_be_bytes([*next_high, *next_low]),
            raw,
            commands,
            max_commands,
        })
    }
}

fn word(input: &[u8], at: usize) -> Result<u16> {
    let end = at.checked_add(2).ok_or(Error::Overflow)?;
    let b = input.get(at..end).ok_or(Error::Truncated)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

#[derive(Clone, Copy, Debug)]
pub struct Control<'a> {
    pub offset: usize,
    pub date: DvdDate,
    pub next_offset: u16,
    pub raw: &'a [u8],
    commands: &'a [u8],
    max_commands: usize,
}

impl<'a> Control<'a> {
    pub fn commands(self) -> Commands<'a> {
        Commands {
            input: self.commands,
            at: 0,
            count: 0,
            maximum: self.max_commands,
            done: false,
        }
    }

    /// Internal resumable cursor. The owning program stores only offsets;
    /// no borrow into its owned packet survives a work boundary.
    pub(super) fn command_at(self, at: usize, count: usize) -> Result<(Command<'a>, usize, bool)> {
        Commands::read_at(self.commands, at, count, self.max_commands)
    }
}

/// Links must advance or terminate at the current block. This bounded profile
/// rejects backward links explicitly instead of repairing or looping on them.
#[derive(Debug)]
pub struct Controls<'a> {
    packet: DvdPacket<'a>,
    next: Option<usize>,
    count: usize,
    maximum: usize,
    max_commands: usize,
}

impl<'a> Controls<'a> {
    fn parse(&mut self, at: usize) -> Result<Control<'a>> {
        if self.count == self.maximum {
            return Err(Error::Limit {
                requested: self.count.saturating_add(1),
                maximum: self.maximum,
            });
        }
        let date = DvdDate(word(self.packet.raw, at)?);
        let next_offset = word(self.packet.raw, at.checked_add(2).ok_or(Error::Overflow)?)?;
        let next = usize::from(next_offset);
        let start = at.checked_add(4).ok_or(Error::Overflow)?;
        let end = if next == at {
            self.next = None;
            self.packet.raw.len()
        } else {
            if next < start.checked_add(1).ok_or(Error::Overflow)?
                || next.checked_add(5).ok_or(Error::Overflow)? > self.packet.raw.len()
            {
                return Err(Error::Invalid(
                    "DVD backward, overlapping or out-of-range control link",
                ));
            }
            self.next = Some(next);
            next
        };
        let commands = self.packet.raw.get(start..end).ok_or(Error::Truncated)?;
        self.count += 1;
        Ok(Control {
            offset: at,
            date,
            next_offset,
            raw: &self.packet.raw[at..end],
            commands,
            max_commands: self.max_commands,
        })
    }
}

impl<'a> Iterator for Controls<'a> {
    type Item = Result<Control<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        let at = self.next?;
        let result = self.parse(at);
        if result.is_err() {
            self.next = None;
        }
        Some(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    ForcedStart,
    Start,
    Stop,
    /// Raw packed source nibbles, without guessing a palette or reversing them.
    Palette(u16),
    Alpha(u16),
    Area(DvdArea),
    Fields(FieldOffsets),
    End,
    /// Operand lengths for unsupported commands are not guessed. The bounded
    /// remainder stays available, and no following bytes become fake commands.
    Opaque {
        opcode: u8,
        remainder: &'a [u8],
    },
}

#[derive(Debug)]
pub struct Commands<'a> {
    input: &'a [u8],
    at: usize,
    count: usize,
    maximum: usize,
    done: bool,
}

impl<'a> Commands<'a> {
    pub(super) fn resume(input: &'a [u8], at: usize, count: usize, maximum: usize) -> Self {
        Self {
            input,
            at,
            count,
            maximum,
            done: false,
        }
    }

    pub(super) fn position(&self) -> usize {
        self.at
    }

    pub(super) fn is_done(&self) -> bool {
        self.done
    }

    pub(super) fn read_at(
        input: &'a [u8],
        at: usize,
        count: usize,
        maximum: usize,
    ) -> Result<(Command<'a>, usize, bool)> {
        let mut commands = Self::resume(input, at, count, maximum);
        let command = commands.parse()?;
        Ok((command, commands.at, commands.done))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(Error::Overflow)?;
        let bytes = self.input.get(self.at..end).ok_or(Error::Truncated)?;
        self.at = end;
        Ok(bytes)
    }

    fn parse(&mut self) -> Result<Command<'a>> {
        if self.count == self.maximum {
            return Err(Error::Limit {
                requested: self.count.saturating_add(1),
                maximum: self.maximum,
            });
        }
        self.count += 1;
        let opcode = self.take(1)?[0];
        Ok(match opcode {
            0 => Command::ForcedStart,
            1 => Command::Start,
            2 => Command::Stop,
            3 | 4 => {
                let b = self.take(2)?;
                let value = u16::from_be_bytes([b[0], b[1]]);
                if opcode == 3 {
                    Command::Palette(value)
                } else {
                    Command::Alpha(value)
                }
            }
            5 => {
                let b = self.take(6)?;
                Command::Area(DvdArea {
                    left: u16::from(b[0]) << 4 | u16::from(b[1] >> 4),
                    right: u16::from(b[1] & 15) << 8 | u16::from(b[2]),
                    top: u16::from(b[3]) << 4 | u16::from(b[4] >> 4),
                    bottom: u16::from(b[4] & 15) << 8 | u16::from(b[5]),
                })
            }
            6 => {
                let b = self.take(4)?;
                Command::Fields(FieldOffsets {
                    top: u16::from_be_bytes([b[0], b[1]]),
                    bottom: u16::from_be_bytes([b[2], b[3]]),
                })
            }
            0xff => {
                self.done = true;
                Command::End
            }
            _ => {
                self.done = true;
                Command::Opaque {
                    opcode,
                    remainder: &self.input[self.at..],
                }
            }
        })
    }

    /// Uninterpreted bytes after END or an opaque command remain source data.
    pub fn remainder(&self) -> &'a [u8] {
        &self.input[self.at..]
    }
}

impl<'a> Iterator for Commands<'a> {
    type Item = Result<Command<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = self.parse();
        if result.is_err() {
            self.done = true;
        }
        Some(result)
    }
}
