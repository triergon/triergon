//! Teletext packets (EN 300 706 sections 7 and 9): addresses and page headers.

use super::coding::hamming_8_4;

/// A magazine, 1 to 8 (magazine 8 is transmitted as 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Magazine(u8);

impl Magazine {
    /// The magazine of a transmitted three-bit magazine number.
    #[must_use]
    pub(crate) fn from_transmitted(bits: u8) -> Self {
        Self(match bits & 7 {
            0 => 8,
            m => m,
        })
    }

    /// The magazine number, 1 to 8.
    #[must_use]
    pub(crate) fn number(self) -> u8 {
        self.0
    }

    /// Zero-based index for per-magazine state.
    #[must_use]
    pub(crate) fn index(self) -> usize {
        usize::from(self.0 - 1)
    }
}

/// A page number: its magazine and the two hexadecimal page digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PageNumber {
    pub magazine: Magazine,
    /// Tens digit in the high nibble, units digit in the low nibble.
    pub page: u8,
}

impl PageNumber {
    /// Whether both page digits are decimal, as on every displayable page.
    #[must_use]
    pub(crate) fn is_decimal(self) -> bool {
        self.page >> 4 <= 9 && self.page & 0xF <= 9
    }

    /// Page `FF`, which only ends the previous page of its magazine.
    #[must_use]
    pub(crate) fn is_time_filler(self) -> bool {
        self.page == 0xFF
    }

    /// The page number as three hexadecimal digits, such as `0x888`.
    #[must_use]
    pub(crate) fn hex(self) -> u16 {
        u16::from(self.magazine.number()) << 8 | u16::from(self.page)
    }
}

/// A page subcode: S1 in bits 0-3, S2 in 4-6, S3 in 8-11 and S4 in 12-13.
/// Pages without subpages carry 0000 or 3F7F.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Subcode(pub u16);

/// The control bits of a page header (EN 300 706 table 2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Control {
    /// C4: erase the page before its rows are received.
    pub erase: bool,
    /// C5: newsflash, shown boxed into the picture.
    pub newsflash: bool,
    /// C6: subtitle, shown boxed into the picture.
    pub subtitle: bool,
    /// C7: do not display the header row.
    pub suppress_header: bool,
    /// C8: the page content changed.
    pub update: bool,
    /// C9: the page interrupts a sequence of pages in numerical order.
    pub interrupted: bool,
    /// C10: do not display rows 1 to 24.
    pub inhibit_display: bool,
    /// C11: the service transmits its magazines serially.
    pub serial: bool,
    /// C12 to C14, the national option character subset (C12 most significant).
    pub national_option: u8,
}

/// The page header packet X/0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub page: PageNumber,
    pub subcode: Subcode,
    pub control: Control,
    /// Columns 8 to 39 of the header row as received, with parity bits.
    pub text: [u8; 32],
}

/// A packet's magazine and packet number (row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Address {
    pub magazine: Magazine,
    /// Packet number, 0 to 31.
    pub row: u8,
}

/// One teletext packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Packet<'a> {
    /// X/0, the header of the next page of the magazine.
    Header(Header),
    /// X/0 whose page address or control bits could not be corrected.
    DamagedHeader,
    /// X/1 to X/25, display rows.
    Row { row: u8, data: &'a [u8; 40] },
    /// X/26, X/27 and X/28: page enhancement, links and page enhancement
    /// data, each with its designation code.
    PageData {
        row: u8,
        designation: u8,
        data: &'a [u8; 39],
    },
    /// M/29: magazine-related data, with its designation code.
    MagazineData { designation: u8, data: &'a [u8; 39] },
    /// Any other packet: broadcast service data (30), independent data (31)
    /// or a page- or magazine-related packet with a damaged designation.
    Other { row: u8 },
}

impl<'a> Packet<'a> {
    /// The address and content of one 42-byte packet; `None` when its address
    /// cannot be corrected.
    #[must_use]
    pub(crate) fn parse(packet: &'a [u8; 42]) -> Option<(Address, Self)> {
        let (address, data) = packet.split_first_chunk::<2>()?;
        let low = hamming_8_4(address[0])?;
        let high = hamming_8_4(address[1])?;
        let address = Address {
            magazine: Magazine::from_transmitted(low & 7),
            row: low >> 3 | high << 1,
        };
        let data: &'a [u8; 40] = data.try_into().ok()?;
        let content = match address.row {
            0 => parse_header(address.magazine, data).map_or(Self::DamagedHeader, Self::Header),
            row @ 1..=25 => Self::Row { row, data },
            row @ 26..=29 => match (hamming_8_4(data[0]), data[1..].try_into()) {
                (Some(designation), Ok(data)) if row == 29 => {
                    Self::MagazineData { designation, data }
                }
                (Some(designation), Ok(data)) => Self::PageData {
                    row,
                    designation,
                    data,
                },
                _ => Self::Other { row },
            },
            row => Self::Other { row },
        };
        Some((address, content))
    }
}

fn parse_header(magazine: Magazine, data: &[u8; 40]) -> Option<Header> {
    let mut nibbles = [0u8; 8];
    for (nibble, &byte) in nibbles.iter_mut().zip(data) {
        *nibble = hamming_8_4(byte)?;
    }
    let [units, tens, s1, s2, s3, s4, c7_10, c11_14] = nibbles;
    let page = units | tens << 4;
    let bit = |nibble: u8, n: u8| nibble >> n & 1 != 0;
    let control = Control {
        erase: bit(s2, 3),
        newsflash: bit(s4, 2),
        subtitle: bit(s4, 3),
        suppress_header: bit(c7_10, 0),
        update: bit(c7_10, 1),
        interrupted: bit(c7_10, 2),
        inhibit_display: bit(c7_10, 3),
        serial: bit(c11_14, 0),
        national_option: (c11_14 >> 1 & 1) << 2 | (c11_14 >> 2 & 1) << 1 | c11_14 >> 3,
    };
    let subcode =
        u16::from(s1) | u16::from(s2 & 7) << 4 | u16::from(s3) << 8 | u16::from(s4 & 3) << 12;
    Some(Header {
        page: PageNumber { magazine, page },
        subcode: Subcode(subcode),
        control,
        text: data[8..].try_into().ok()?,
    })
}
