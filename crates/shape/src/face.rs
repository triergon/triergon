//! A face's layout tables.

use crate::ot::gdef::Gdef;
use crate::ot::kern::Kern;
use crate::ot::layout::{LayoutTable, Stage};
use triergon_font::Font;

/// The layout tables of one face, read from its bytes without copying.
#[derive(Debug)]
pub struct Face<'a> {
    pub(crate) font: &'a Font,
    pub(crate) gdef: Gdef<'a>,
    pub(crate) gsub: LayoutTable<'a>,
    pub(crate) gpos: LayoutTable<'a>,
    pub(crate) kern: Kern<'a>,
}

impl<'a> Face<'a> {
    /// The layout tables of `font`. Nothing is read beyond their headers until
    /// a plan or a shaping call asks.
    #[must_use]
    pub fn new(font: &'a Font) -> Self {
        let gsub = font.table(*b"GSUB");
        let gpos = font.table(*b"GPOS");
        let len = |t: Option<&[u8]>| t.map_or(0, <[u8]>::len);
        Self {
            font,
            gdef: Gdef::new(font.table(*b"GDEF"), len(gsub), len(gpos)),
            gsub: LayoutTable::new(Stage::Substitute, gsub),
            gpos: LayoutTable::new(Stage::Position, gpos),
            kern: Kern::new(font.table(*b"kern")),
        }
    }

    /// The font the face reads.
    #[must_use]
    pub fn font(&self) -> &'a Font {
        self.font
    }

    /// Whether HarfBuzz would shape this face with Apple Advanced Typography:
    /// `morx`, or `kerx` without both GSUB and GPOS, or `trak` with `STAT`.
    pub(crate) fn has_aat_layout(&self) -> bool {
        let has = |tag: &[u8; 4]| self.font.table(*tag).is_some_and(|t| t.len() >= 8);
        has(b"morx")
            || (has(b"kerx") && !(self.gsub.has_data() && self.gpos.has_data()))
            || (has(b"trak") && has(b"STAT"))
    }
}
