use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DvdArea {
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
}

impl DvdArea {
    pub(crate) fn dimensions(self) -> Result<(u16, u16)> {
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
pub(crate) struct FieldOffsets {
    pub top: u16,
    pub bottom: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command<'a> {
    ForcedStart,
    Start,
    Stop,
    /// Raw packed source nibbles, without guessing a palette or reversing them.
    Palette(u16),
    Alpha(u16),
    Area(DvdArea),
    Fields(FieldOffsets),
    /// `0x83`: 256 (Y, Cr, Cb) colours of an eight-bit subpicture.
    ExtendedPalette(&'a [u8; 768]),
    /// `0x84`: the contrast of each of the 256 colours.
    ExtendedContrast(&'a [u8; 256]),
    /// `0x85`: the display area of eight-bit pixels.
    ExtendedArea(DvdArea),
    /// `0x86`: 32-bit field offsets.
    ExtendedFields {
        top: u32,
        bottom: u32,
    },
    End,
    /// Operand lengths for unsupported commands are not guessed. The bounded
    /// remainder stays available, and no following bytes become fake commands.
    Opaque {
        opcode: u8,
        remainder: &'a [u8],
    },
}

/// A display area's 12-bit inclusive columns and rows.
fn area(b: &[u8]) -> DvdArea {
    DvdArea {
        left: u16::from(b[0]) << 4 | u16::from(b[1] >> 4),
        right: u16::from(b[1] & 15) << 8 | u16::from(b[2]),
        top: u16::from(b[3]) << 4 | u16::from(b[4] >> 4),
        bottom: u16::from(b[4] & 15) << 8 | u16::from(b[5]),
    }
}

#[derive(Debug)]
pub(crate) struct Commands<'a> {
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

    fn take_array<const N: usize>(&mut self) -> Result<&'a [u8; N]> {
        self.take(N)?.try_into().map_err(|_| Error::Truncated)
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
            5 => Command::Area(area(self.take(6)?)),
            0x85 => Command::ExtendedArea(area(self.take(6)?)),
            0x83 => Command::ExtendedPalette(self.take_array()?),
            0x84 => Command::ExtendedContrast(self.take_array()?),
            0x86 => {
                let b = self.take(8)?;
                Command::ExtendedFields {
                    top: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
                    bottom: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
                }
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
