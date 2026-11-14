//! The presentation libzvbi gives a page from the X/28 and M/29 format 1
//! packets it is presented with, and its colour table.

use crate::teletext::display::Colour;
use crate::teletext::presentation::{Designations, Presentation, Presentations, Remapping, Rgb};

/// The designations, remapping and colours a page is presented with.
#[derive(Clone, Debug)]
pub(super) struct Settings {
    pub(super) designations: Option<Designations>,
    pub(super) remapping: Remapping,
    pub(super) palette: Palette,
}

impl Settings {
    /// Designations and the remapping come from designation 0, else
    /// designation 4; colour lookup tables 2 and 3 from designation 0, tables
    /// 0 and 1 from designation 4.
    pub(super) fn of(presentations: &Presentations) -> Self {
        let Presentations {
            designation_0,
            designation_4,
        } = *presentations;
        let first = designation_0.or(designation_4);
        let mut palette = Palette::DEFAULT;
        for (start, presentation) in [(0, designation_4), (16, designation_0)] {
            if let Some(presentation) = presentation {
                palette.redefine(start, &presentation.colours);
            }
        }
        Self {
            designations: first.map(|p| p.designations),
            remapping: first.map_or_else(Remapping::default, |p: Presentation| p.remapping),
            palette,
        }
    }
}

/// The 32 colours as 24-bit RGB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Palette([u32; 32]);

impl Palette {
    /// EN 300 706 table 30.
    const DEFAULT: Self = {
        const TABLE: [u16; 32] = [
            0x000, 0xF00, 0x0F0, 0xFF0, 0x00F, 0xF0F, 0x0FF, 0xFFF, //
            0x000, 0x700, 0x070, 0x770, 0x007, 0x707, 0x077, 0x777, //
            0xF05, 0xF70, 0x0F7, 0xFFB, 0x0CA, 0x500, 0x652, 0xC77, //
            0x333, 0xF77, 0x7F7, 0xFF7, 0x77F, 0xF7F, 0x7FF, 0xDDD,
        ];
        let mut colours = [0; 32];
        let mut index = 0;
        while index < 32 {
            let rgb = TABLE[index];
            colours[index] = Rgb {
                red: (rgb >> 8) as u8,
                green: (rgb >> 4 & 0xF) as u8,
                blue: (rgb & 0xF) as u8,
            }
            .rgb24();
            index += 1;
        }
        Self(colours)
    };

    fn redefine(&mut self, start: usize, colours: &[Rgb; 16]) {
        for (entry, colour) in self.0[start..start + 16].iter_mut().zip(colours) {
            *entry = colour.rgb24();
        }
    }

    /// The ASS colour (blue, green, red) of `colour`. The reference shows
    /// colour 8 as black, colour 13 as colour 11, and colour 14 as the red and
    /// green of colour 12 with blue 0x20.
    pub(super) fn bgr(&self, colour: Colour) -> u32 {
        let rgb = match colour.0 & 31 {
            8 => 0,
            13 => self.0[11],
            14 => self.0[12] & 0xFF_FF00 | 0x20,
            index => self.0[usize::from(index)],
        };
        (rgb & 0xFF) << 16 | (rgb & 0xFF00) | rgb >> 16
    }
}
