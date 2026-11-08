//! Page presentation data (EN 300 706 sections 9.4.2 and 9.5): the format 1
//! packets X/28/0 and M/29/0 for a page or a magazine, and X/28/4 and M/29/4,
//! which Level 3.5 adds for colour lookup tables 0 and 1.
//!
//! The side panels, default screen and row colours and black background
//! substitution they also carry are not decoded.

use super::coding::hamming_24_18;
use super::display::Colour;
use super::page::Triplets;

/// The character set designations (table 32) of a format 1 packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Designations {
    /// The default G0 and G2 set.
    pub default: u8,
    /// The second G0 set, which the escape code switches to.
    pub second: u8,
}

/// A 12-bit colour lookup table entry, four bits for each component.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Rgb {
    /// The entry as 24-bit RGB.
    #[must_use]
    pub const fn rgb24(self) -> u32 {
        let (red, green, blue) = (self.red as u32, self.green as u32, self.blue as u32);
        (red * 0x11) << 16 | (green * 0x11) << 8 | (blue * 0x11)
    }
}

/// The colour lookup tables Level 1 colour codes select from (table 34): the
/// table of the foreground and the table of the background colour codes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Remapping {
    foreground: u8,
    background: u8,
}

impl Remapping {
    /// The remapping a three-bit colour table remapping code selects.
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        let (foreground, background) = match code & 7 {
            0 => (0, 0),
            1 => (0, 1),
            2 => (0, 2),
            3 => (1, 1),
            4 => (1, 2),
            5 => (2, 1),
            6 => (2, 2),
            _ => (2, 3),
        };
        Self {
            foreground,
            background,
        }
    }

    /// The colour of Level 1 foreground colour code `code` (0 to 7).
    #[must_use]
    pub const fn foreground(self, code: u8) -> Colour {
        Colour(self.foreground * 8 + (code & 7))
    }

    /// The colour of Level 1 background colour code `code` (0 to 7).
    #[must_use]
    pub const fn background(self, code: u8) -> Colour {
        Colour(self.background * 8 + (code & 7))
    }
}

/// The decoded content of a format 1 packet for a basic Level 1 page (page
/// function 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Presentation {
    pub designations: Designations,
    /// Entries for two colour lookup tables: tables 2 and 3 from designation
    /// 0, tables 0 and 1 from designation 4.
    pub colours: [Rgb; 16],
    pub remapping: Remapping,
}

impl Presentation {
    /// Decodes a packet's triplets; a packet with a triplet Hamming 24/18
    /// cannot correct or for another page function is unusable.
    #[must_use]
    pub fn parse(triplets: &Triplets) -> Option<Self> {
        let mut values = [0; 13];
        for (value, triplet) in values.iter_mut().zip(triplets) {
            *value = hamming_24_18(*triplet)?;
        }
        // The triplets form one field, least significant bit first.
        let bits = |start: usize, width: usize| -> u8 {
            (start..start + width).rev().fold(0, |field, bit| {
                field << 1 | (values[bit / 18] >> (bit % 18) & 1) as u8
            })
        };
        if bits(0, 4) != 0 {
            return None;
        }
        let mut colours = [Rgb::default(); 16];
        for (index, colour) in colours.iter_mut().enumerate() {
            let start = 28 + 12 * index;
            *colour = Rgb {
                red: bits(start, 4),
                green: bits(start + 4, 4),
                blue: bits(start + 8, 4),
            };
        }
        Some(Self {
            designations: Designations {
                default: bits(7, 7),
                second: bits(14, 7),
            },
            colours,
            remapping: Remapping::from_code(bits(231, 3)),
        })
    }
}

/// The format 1 packets of a page (X/28) or a magazine (M/29) received last.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Presentations {
    pub designation_0: Option<Presentation>,
    pub designation_4: Option<Presentation>,
}

impl Presentations {
    /// Receives the packet with designation `designation` and reports
    /// whether it was taken; an unusable packet, or one of another
    /// designation, leaves the last one.
    pub fn receive(&mut self, designation: u8, triplets: &Triplets) -> bool {
        let slot = match designation {
            0 => &mut self.designation_0,
            4 => &mut self.designation_4,
            _ => return false,
        };
        let presentation = Presentation::parse(triplets);
        if presentation.is_some() {
            *slot = presentation;
        }
        presentation.is_some()
    }

    /// These packets over `defaults`: each designation of `defaults` counts
    /// where this has none.
    #[must_use]
    pub fn over(self, defaults: &Self) -> Self {
        Self {
            designation_0: self.designation_0.or(defaults.designation_0),
            designation_4: self.designation_4.or(defaults.designation_4),
        }
    }

    /// Whether any packet was received.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.designation_0.is_none() && self.designation_4.is_none()
    }
}
