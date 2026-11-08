//! The glyph definition table: glyph classes, mark attachment classes and mark
//! glyph sets.

use super::data::Data;
use super::layout::{class, coverage};
use crate::buffer::glyph_props;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Gdef<'a> {
    classes: Data<'a>,
    mark_classes: Data<'a>,
    mark_sets: Data<'a>,
}

impl<'a> Gdef<'a> {
    /// Reads `GDEF`. HarfBuzz ignores the GDEF of a few old fonts whose glyph
    /// classes are known to be wrong, identified by the lengths of their GDEF,
    /// GSUB and GPOS; so does this.
    pub(crate) fn new(table: Option<&'a [u8]>, gsub_len: usize, gpos_len: usize) -> Self {
        let Some(table) = table else {
            return Self::default();
        };
        if is_blocklisted(table.len(), gsub_len, gpos_len) {
            return Self::default();
        }
        let data = Data(table);
        if data.u16(0) != 1 {
            return Self::default();
        }
        Self {
            classes: data.offset16(4),
            mark_classes: data.offset16(10),
            mark_sets: if data.u16(2) >= 2 {
                data.offset16(12)
            } else {
                Data::EMPTY
            },
        }
    }

    pub(crate) fn has_glyph_classes(&self) -> bool {
        !self.classes.is_empty()
    }

    /// HarfBuzz's glyph props for `glyph`: its class bit, and a mark's attachment
    /// class in the high byte.
    pub(crate) fn glyph_props(&self, glyph: u32) -> u16 {
        match class(self.classes, glyph) {
            1 => glyph_props::BASE_GLYPH,
            2 => glyph_props::LIGATURE,
            3 => glyph_props::MARK | (class(self.mark_classes, glyph) << 8),
            _ => 0,
        }
    }

    fn mark_set_count(&self) -> usize {
        if self.mark_sets.u16(0) == 1 {
            self.mark_sets.count(2, 4, 4)
        } else {
            0
        }
    }

    /// Whether mark glyph set `set` holds `glyph`.
    pub(crate) fn mark_set_covers(&self, set: usize, glyph: u32) -> bool {
        set < self.mark_set_count()
            && coverage(self.mark_sets.offset32(4 + 4 * set), glyph).is_some()
    }

    /// Lookup props with an out-of-range mark filtering set dropped, as HarfBuzz
    /// sanitises them.
    pub(crate) fn sanitize_lookup_props(&self, props: u32) -> u32 {
        use super::layout::Lookup;
        if props & Lookup::USE_MARK_FILTERING_SET != 0
            && (props >> 16) as usize >= self.mark_set_count()
        {
            props & !Lookup::USE_MARK_FILTERING_SET
        } else {
            props
        }
    }
}

/// GDEF, GSUB and GPOS lengths of fonts whose GDEF HarfBuzz ignores (versions of
/// Times New Roman, Tahoma, Microsoft Himalaya, Cantarell, Padauk and others).
fn is_blocklisted(gdef: usize, gsub: usize, gpos: usize) -> bool {
    const LISTED: [(usize, usize, usize); 40] = [
        (442, 2874, 42038),
        (430, 2874, 40662),
        (442, 2874, 39116),
        (430, 2874, 39374),
        (490, 3046, 41638),
        (478, 3046, 41902),
        (898, 12554, 46470),
        (910, 12566, 47732),
        (928, 23298, 59332),
        (940, 23310, 60732),
        (964, 23836, 60072),
        (976, 23832, 61456),
        (994, 24474, 60336),
        (1006, 24470, 61740),
        (1006, 24576, 61346),
        (1018, 24572, 62828),
        (1006, 24576, 61352),
        (1018, 24572, 62834),
        (832, 7324, 47162),
        (844, 7302, 45474),
        (180, 13054, 7254),
        (192, 12638, 7254),
        (192, 12690, 7254),
        (188, 248, 3852),
        (188, 264, 3426),
        (1058, 47032, 11818),
        (1046, 47030, 12600),
        (1058, 71796, 16770),
        (1046, 71790, 17862),
        (1046, 71788, 17112),
        (1058, 71794, 17514),
        (1330, 109904, 57938),
        (1330, 109904, 58972),
        (1004, 59092, 14836),
        (588, 5078, 14418),
        (588, 5078, 14238),
        (894, 17162, 33960),
        (894, 17154, 34472),
        (816, 7868, 17052),
        (816, 7868, 17138),
    ];
    LISTED.contains(&(gdef, gsub, gpos))
}
