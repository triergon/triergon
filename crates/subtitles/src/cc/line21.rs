//! CEA-608 (line 21) byte pairs: parity, the control-code repertoire and the
//! character sets.
//!
//! A pair is two bytes, each seven data bits with odd parity in bit 7. A first
//! byte from `0x10` to `0x1F` starts a control code, `0x01` to `0x0F` an XDS
//! code (field 2 only) and `0x20` and above one or two characters. Control codes
//! name their data channel in bit 3 of the first byte (`0x14` is channel 1,
//! `0x1C` channel 2 in the same field); [`Pair::decode`] reports it and folds the
//! code into the channel 1 form.

/// True when `byte` has odd parity, as every transmitted line 21 byte must.
#[must_use]
pub(crate) const fn odd_parity(byte: u8) -> bool {
    byte.count_ones() % 2 == 1
}

/// The data channel a control code addresses within its field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Channel {
    /// CC1 on field 1, CC3 on field 2 (first byte `0x10` to `0x17`).
    First,
    /// CC2 on field 1, CC4 on field 2 (first byte `0x18` to `0x1F`).
    Second,
}

/// One of the eight foreground colours.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Color {
    #[default]
    White,
    Green,
    Blue,
    Cyan,
    Red,
    Yellow,
    Magenta,
    Black,
}

impl Color {
    /// The colour with index `n` (0 white to 6 magenta) of the attribute tables.
    const fn indexed(n: u8) -> Self {
        match n {
            0 => Self::White,
            1 => Self::Green,
            2 => Self::Blue,
            3 => Self::Cyan,
            4 => Self::Red,
            5 => Self::Yellow,
            _ => Self::Magenta,
        }
    }
}

/// The colour attribute of a preamble or mid-row code: a colour, or white italics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Ink {
    Color(Color),
    Italics,
}

/// What a preamble address code sets besides the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Preamble {
    /// Cursor to column 0 with this ink.
    Ink(Ink),
    /// White text, cursor to this column (0, 4, ..., 28).
    Indent(u8),
}

/// A background attribute: colour and whether it is semi-transparent, or the
/// transparent background.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Background {
    Opaque(Color),
    SemiTransparent(Color),
    Transparent,
}

/// Miscellaneous control codes (first byte `0x14`/`0x15`, second `0x20` to `0x2F`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Command {
    /// Resume caption loading: pop-on.
    ResumeCaptionLoading,
    Backspace,
    AlarmOff,
    AlarmOn,
    DeleteToEndOfRow,
    /// Roll-up captions with this many rows (2, 3 or 4).
    RollUp(u8),
    FlashOn,
    /// Resume direct captioning: paint-on.
    ResumeDirectCaptioning,
    TextRestart,
    ResumeTextDisplay,
    EraseDisplayedMemory,
    CarriageReturn,
    EraseNonDisplayedMemory,
    EndOfCaption,
}

impl Command {
    const fn from_code(lo: u8) -> Self {
        match lo & 0x0f {
            0x0 => Self::ResumeCaptionLoading,
            0x1 => Self::Backspace,
            0x2 => Self::AlarmOff,
            0x3 => Self::AlarmOn,
            0x4 => Self::DeleteToEndOfRow,
            n @ 0x5..=0x7 => Self::RollUp(n - 3),
            0x8 => Self::FlashOn,
            0x9 => Self::ResumeDirectCaptioning,
            0xa => Self::TextRestart,
            0xb => Self::ResumeTextDisplay,
            0xc => Self::EraseDisplayedMemory,
            0xd => Self::CarriageReturn,
            0xe => Self::EraseNonDisplayedMemory,
            _ => Self::EndOfCaption,
        }
    }
}

/// A character set of the line 21 repertoire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Charset {
    /// The standard characters, `0x20` to `0x7F`.
    #[default]
    Basic,
    /// Special North American characters (`0x11 0x30` to `0x3F`).
    Special,
    /// Extended Spanish, French and miscellaneous (`0x12 0x20` to `0x3F`).
    SpanishFrench,
    /// Extended Portuguese, German and Danish (`0x13 0x20` to `0x3F`).
    PortugueseGerman,
}

/// A displayable character: its set and code. Extended characters replace the
/// character before them (decoders that cannot show them keep the basic
/// fallback the caption author sent first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Character {
    pub charset: Charset,
    pub code: u8,
}

impl Character {
    /// The character's Unicode scalar as CEA-608 defines it, `None` for a code
    /// outside its set.
    #[must_use]
    pub(crate) const fn to_char(self) -> Option<char> {
        let index = self.code as usize;
        Some(match self.charset {
            Charset::Basic => match self.code {
                0x2a => 'á',
                0x5c => 'é',
                0x5e => 'í',
                0x5f => 'ó',
                0x60 => 'ú',
                0x7b => 'ç',
                0x7c => '÷',
                0x7d => 'Ñ',
                0x7e => 'ñ',
                0x7f => '■',
                0x20..=0x7e => self.code as char,
                _ => return None,
            },
            Charset::Special if matches!(self.code, 0x30..=0x3f) => SPECIAL[index - 0x30],
            Charset::SpanishFrench if matches!(self.code, 0x20..=0x3f) => {
                SPANISH_FRENCH[index - 0x20]
            }
            Charset::PortugueseGerman if matches!(self.code, 0x20..=0x3f) => {
                PORTUGUESE_GERMAN[index - 0x20]
            }
            _ => return None,
        })
    }
}

/// `0x11 0x30` to `0x3F`; `0x39` is the transparent space.
const SPECIAL: [char; 16] = [
    '®', '°', '½', '¿', '™', '¢', '£', '♪', 'à', '\u{a0}', 'è', 'â', 'ê', 'î', 'ô', 'û',
];
const SPANISH_FRENCH: [char; 32] = [
    'Á', 'É', 'Ó', 'Ú', 'Ü', 'ü', '‘', '¡', '*', '’', '—', '©', '℠', '•', '“', '”', 'À', 'Â', 'Ç',
    'È', 'Ê', 'Ë', 'ë', 'Î', 'Ï', 'ï', 'Ô', 'Ù', 'ù', 'Û', '«', '»',
];
const PORTUGUESE_GERMAN: [char; 32] = [
    'Ã', 'ã', 'Í', 'Ì', 'ì', 'Ò', 'ò', 'Õ', 'õ', '{', '}', '\\', '^', '_', '|', '~', 'Ä', 'ä', 'Ö',
    'ö', 'ß', '¥', '¤', '¦', 'Å', 'å', 'Ø', 'ø', '┌', '┐', '└', '┘',
];

/// The text attributes a mid-row code sets. A mid-row code occupies one column,
/// shown as a space; a colour turns italics off and underline follows the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MidRow {
    pub ink: Ink,
    pub underline: bool,
}

/// One decoded byte pair, parity bits removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Pair {
    /// Both bytes zero after parity: filler.
    Null,
    /// One or two basic characters (`second` is 0 when only one was sent).
    Text {
        first: u8,
        second: u8,
    },
    /// A special or extended character.
    Character {
        channel: Channel,
        character: Character,
    },
    /// A preamble address code: row 0 to 14 (spec rows 1 to 15) and attribute.
    Preamble {
        channel: Channel,
        row: u8,
        preamble: Preamble,
        underline: bool,
    },
    MidRow {
        channel: Channel,
        attributes: MidRow,
    },
    Background {
        channel: Channel,
        background: Background,
    },
    /// Black foreground (`0x17 0x2E`, underlined with `0x2F`).
    BlackText {
        channel: Channel,
        underline: bool,
    },
    /// Tab offset by one to three columns.
    TabOffset {
        channel: Channel,
        columns: u8,
    },
    Command {
        channel: Channel,
        command: Command,
    },
    /// An XDS control or data pair (first byte below `0x10`).
    Xds {
        first: u8,
        second: u8,
    },
    /// A control code without a meaning in CEA-608.
    Unassigned {
        first: u8,
        second: u8,
    },
}

/// The spec rows of the preamble address codes: first byte (channel 1 form, low
/// three bits) and bit 5 of the second byte select a row.
const PREAMBLE_ROWS: [[u8; 2]; 8] = [
    [10, 10], // 0x10: row 11 only
    [0, 1],
    [2, 3],
    [11, 12],
    [13, 14],
    [4, 5],
    [6, 7],
    [8, 9],
];

impl Pair {
    /// Decodes `first` and `second` with their parity bits cleared. Parity is
    /// the caller's check ([`odd_parity`]); this only interprets the seven
    /// data bits of each byte.
    #[must_use]
    pub(crate) const fn decode(first: u8, second: u8) -> Self {
        let (hi, lo) = (first & 0x7f, second & 0x7f);
        if hi == 0 && lo == 0 {
            return Self::Null;
        }
        if hi >= 0x20 {
            return Self::Text {
                first: hi,
                second: lo,
            };
        }
        if hi < 0x10 {
            return Self::Xds {
                first: hi,
                second: lo,
            };
        }
        let channel = if hi & 0x08 == 0 {
            Channel::First
        } else {
            Channel::Second
        };
        let group = hi & 0x07;
        match (group, lo) {
            (_, 0x40..=0x7f) if group != 0 || lo < 0x60 => {
                let attribute = (lo >> 1) & 0x0f;
                let preamble = if attribute < 8 {
                    Preamble::Ink(if attribute == 7 {
                        Ink::Italics
                    } else {
                        Ink::Color(Color::indexed(attribute))
                    })
                } else {
                    Preamble::Indent((attribute - 8) * 4)
                };
                Self::Preamble {
                    channel,
                    row: PREAMBLE_ROWS[group as usize][((lo >> 5) & 1) as usize],
                    preamble,
                    underline: lo & 1 == 1,
                }
            }
            (1, 0x20..=0x2f) => {
                let attribute = (lo >> 1) & 0x07;
                Self::MidRow {
                    channel,
                    attributes: MidRow {
                        ink: if attribute == 7 {
                            Ink::Italics
                        } else {
                            Ink::Color(Color::indexed(attribute))
                        },
                        underline: lo & 1 == 1,
                    },
                }
            }
            (1, 0x30..=0x3f) => Self::Character {
                channel,
                character: Character {
                    charset: Charset::Special,
                    code: lo,
                },
            },
            (2 | 3, 0x20..=0x3f) => Self::Character {
                channel,
                character: Character {
                    charset: if group == 2 {
                        Charset::SpanishFrench
                    } else {
                        Charset::PortugueseGerman
                    },
                    code: lo,
                },
            },
            (0, 0x20..=0x2f) => {
                let color = Color::indexed((lo >> 1) & 0x07);
                let color = if lo & 0x0e == 0x0e {
                    Color::Black
                } else {
                    color
                };
                Self::Background {
                    channel,
                    background: if lo & 1 == 0 {
                        Background::Opaque(color)
                    } else {
                        Background::SemiTransparent(color)
                    },
                }
            }
            (4 | 5, 0x20..=0x2f) => Self::Command {
                channel,
                command: Command::from_code(lo),
            },
            (7, 0x21..=0x23) => Self::TabOffset {
                channel,
                columns: lo - 0x20,
            },
            (7, 0x2d) => Self::Background {
                channel,
                background: Background::Transparent,
            },
            (7, 0x2e | 0x2f) => Self::BlackText {
                channel,
                underline: lo == 0x2f,
            },
            _ => Self::Unassigned {
                first: hi,
                second: lo,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_control_repertoire() {
        assert_eq!(
            Pair::decode(0x94, 0x2c),
            Pair::Command {
                channel: Channel::First,
                command: Command::EraseDisplayedMemory
            }
        );
        assert_eq!(
            Pair::decode(0x1c, 0x25),
            Pair::Command {
                channel: Channel::Second,
                command: Command::RollUp(2)
            }
        );
        assert_eq!(
            Pair::decode(0x94, 0x70),
            Pair::Preamble {
                channel: Channel::First,
                row: 14,
                preamble: Preamble::Indent(0),
                underline: false
            }
        );
        assert_eq!(
            Pair::decode(0x10, 0x4e),
            Pair::Preamble {
                channel: Channel::First,
                row: 10,
                preamble: Preamble::Ink(Ink::Italics),
                underline: false
            }
        );
        assert_eq!(
            Pair::decode(0x10, 0x60),
            Pair::Unassigned {
                first: 0x10,
                second: 0x60
            }
        );
        assert_eq!(
            Pair::decode(0x97, 0xa2),
            Pair::TabOffset {
                channel: Channel::First,
                columns: 2
            }
        );
        assert_eq!(Pair::decode(0x80, 0x80), Pair::Null);
    }

    #[test]
    fn maps_the_character_sets() {
        assert_eq!(
            Character {
                charset: Charset::Basic,
                code: 0x2a
            }
            .to_char(),
            Some('á')
        );
        assert_eq!(
            Character {
                charset: Charset::Special,
                code: 0x37
            }
            .to_char(),
            Some('♪')
        );
        assert_eq!(
            Character {
                charset: Charset::PortugueseGerman,
                code: 0x3f
            }
            .to_char(),
            Some('┘')
        );
        assert!(odd_parity(0x94) && !odd_parity(0x14));
    }
}
