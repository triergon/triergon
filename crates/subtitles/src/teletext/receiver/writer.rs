//! Writing one page as an ASS event line in the configured format.

use super::Format;
use super::ass::{self, Alignment};
use super::grid::{self, Grid};
use super::out::Out;
use super::settings::Settings;
use super::text;
use crate::teletext::charset::{CharacterSet, G0, G2, NationalSubset};
use crate::teletext::display::{Charsets, Display, Style};
use crate::teletext::page::Page;
use crate::teletext::presentation::Presentations;
use triergon_core::Result;

#[derive(Debug)]
pub(super) struct Writer {
    pub(super) format: Format,
    /// The default character set designation.
    pub(super) region: u8,
    pub(super) chop_top: bool,
    pub(super) chop_spaces: bool,
    read_order: u32,
    alignment: Alignment,
}

impl Writer {
    pub(super) fn new(format: Format, region: u8, chop_top: bool, chop_spaces: bool) -> Self {
        Self {
            format,
            region,
            chop_top,
            chop_spaces,
            read_order: 0,
            alignment: Alignment::default(),
        }
    }

    /// Writes `page` as `ReadOrder,0,Style,,0,0,0,,Text`; `false` when it is
    /// empty and nothing was written. `presentations` are the X/28 and M/29
    /// packets it is presented with and `subtitle` its classification.
    pub(super) fn write(
        &mut self,
        page: &Page,
        presentations: &Presentations,
        subtitle: bool,
        out: &mut Out<'_>,
    ) -> Result<bool> {
        let settings = Settings::of(presentations);
        let designations = settings.designations;
        let national = page.header.control.national_option;
        let default = designations.map_or(self.region, |d| d.default);
        let primary = character_set(default, national);
        // Without X/28 or M/29 the escape code switches to designation 0.
        let secondary = character_set(designations.map_or(0, |d| d.second), national);
        let start = grid::header_start(page.header.page.hex(), page.header.subcode.0);
        let style = Style {
            charsets: Charsets { primary, secondary },
            remapping: settings.remapping,
            enhanced: true,
        };
        let display = Display::present(page, style, &start);
        // With an X/26/0 packet it presents, libzvbi's text shows only
        // visible positions.
        let visible_only = display.enhanced && page.has_enhancement(0);
        let grid = Grid::new(&display, visible_only, settings.remapping);
        // Subtitle pages never show their header row.
        let chop_top = self.chop_top || subtitle;
        let first_row = usize::from(chop_top);
        let rows = &grid.rows[first_row..];
        let (style, chop_spaces) = (self.style(subtitle), self.chop_spaces);
        let alignment = &mut self.alignment;
        let palette = &settings.palette;
        out.number(self.read_order)?;
        out.str(",0,")?;
        out.str(style)?;
        out.str(",,0,0,0,,")?;
        let written = match self.format {
            Format::Text => text::write(&grid, visible_only, chop_top, chop_spaces, out)?,
            Format::Ass if subtitle => {
                ass::write_subtitle(rows, first_row, alignment, palette, out)?
            }
            Format::Ass => {
                ass::write_page(rows, first_row, alignment, palette, out)?;
                true
            }
        };
        if written {
            self.read_order += 1;
        } else {
            out.0.clear();
        }
        Ok(written)
    }

    fn style(&self, subtitle: bool) -> &'static str {
        match self.format {
            Format::Text => "Default",
            Format::Ass if subtitle => "Subtitle",
            Format::Ass => "Teletext",
        }
    }
}

/// libzvbi's character set for a designation (the default region, or one from
/// X/28 or M/29) and a page's national option bits: the designation of its
/// group with those bits, else the designation itself, else English; groups 0
/// to 3 have no national subset where table 32 leaves a code undefined.
fn character_set(designation: u8, national: u8) -> CharacterSet {
    let designate = |code: u8| {
        CharacterSet::from_designation(code).or((code < 32).then_some(CharacterSet::PLAIN_LATIN))
    };
    designate(designation & !7 | national)
        .or_else(|| designate(designation))
        .unwrap_or(CharacterSet {
            g0: G0::Latin(NationalSubset::English),
            g2: G2::Latin,
        })
}
