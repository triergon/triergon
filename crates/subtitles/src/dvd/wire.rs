//! The display control commands of a DVD subpicture unit's control sequences.

/// A display area's inclusive columns and rows, 12 bits each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DvdArea {
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
}

impl DvdArea {
    /// Width and height, or `None` for a reversed area.
    pub(crate) fn dimensions(self) -> Option<(u16, u16)> {
        let width = self.right.checked_sub(self.left)? + 1;
        let height = self.bottom.checked_sub(self.top)? + 1;
        Some((width, height))
    }
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
    /// The offsets of the top and bottom fields' pixels: `0x06` codes them in
    /// 16 bits, `0x86` in 32.
    Fields {
        top: u32,
        bottom: u32,
    },
    /// `0x83`: 256 (Y, Cr, Cb) colours of an eight-bit subpicture.
    ExtendedPalette(&'a [u8; 768]),
    /// `0x84`: the contrast of each of the 256 colours.
    ExtendedContrast(&'a [u8; 256]),
    /// `0x85`: the display area of eight-bit pixels.
    ExtendedArea(DvdArea),
    End,
    /// Any other opcode. Its operands' length is unknown, so the control
    /// sequence ends here.
    Unknown,
}

/// A display area's 12-bit inclusive columns and rows.
fn area(b: &[u8; 6]) -> DvdArea {
    DvdArea {
        left: u16::from(b[0]) << 4 | u16::from(b[1] >> 4),
        right: u16::from(b[1] & 15) << 8 | u16::from(b[2]),
        top: u16::from(b[3]) << 4 | u16::from(b[4] >> 4),
        bottom: u16::from(b[4] & 15) << 8 | u16::from(b[5]),
    }
}

/// The commands of one control sequence, read from a position.
#[derive(Debug)]
pub(crate) struct Commands<'a> {
    input: &'a [u8],
    at: usize,
}

impl<'a> Commands<'a> {
    pub(super) fn resume(input: &'a [u8], at: usize) -> Self {
        Self { input, at }
    }

    pub(super) fn position(&self) -> usize {
        self.at
    }

    fn take<const N: usize>(&mut self) -> Option<&'a [u8; N]> {
        let bytes = self.input.get(self.at..)?.first_chunk::<N>()?;
        self.at += N;
        Some(bytes)
    }

    /// The next command; `None` when its operands are cut short.
    pub(super) fn next_command(&mut self) -> Option<Command<'a>> {
        let &[opcode] = self.take::<1>()?;
        Some(match opcode {
            0 => Command::ForcedStart,
            1 => Command::Start,
            2 => Command::Stop,
            3 => Command::Palette(u16::from_be_bytes(*self.take()?)),
            4 => Command::Alpha(u16::from_be_bytes(*self.take()?)),
            5 => Command::Area(area(self.take()?)),
            6 => {
                let &[t0, t1, b0, b1] = self.take()?;
                Command::Fields {
                    top: u32::from(u16::from_be_bytes([t0, t1])),
                    bottom: u32::from(u16::from_be_bytes([b0, b1])),
                }
            }
            0x83 => Command::ExtendedPalette(self.take()?),
            0x84 => Command::ExtendedContrast(self.take()?),
            0x85 => Command::ExtendedArea(area(self.take()?)),
            0x86 => {
                let &[t0, t1, t2, t3, b0, b1, b2, b3] = self.take()?;
                Command::Fields {
                    top: u32::from_be_bytes([t0, t1, t2, t3]),
                    bottom: u32::from_be_bytes([b0, b1, b2, b3]),
                }
            }
            0xff => Command::End,
            _ => Command::Unknown,
        })
    }
}
