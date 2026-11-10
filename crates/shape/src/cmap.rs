//! The `cmap` table as HarfBuzz 14.6.0's built-in font functions read it
//! (`hb-ot-cmap-table.hh`): sanitized as `hb_sanitize_context_t` sanitizes
//! it, subtables found by binary search over the raw encoding records, and
//! looked up with each format's `get_glyph`.
//!
//! HarfBuzz does not validate subtables as FreeType does. Sanitizing checks
//! only that each structure lies inside the table; a subtable that does not
//! is neutered (its offset set to zero), as is a variation-sequence table
//! that does not, and a table that needs more than 32 such edits is dropped
//! whole, as is one whose version is not 0.

use crate::ot::data::{Data, bsearch};
use std::cmp::Ordering;
use triergon_font::GlyphId;

/// `HB_SANITIZE_MAX_EDITS`.
const MAX_EDITS: usize = 32;

/// A sanitized `cmap` table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cmap<'a> {
    table: Data<'a>,
}

/// A subtable `find_best_subtable` chose, and how its codes are read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Subtable<'a> {
    table: Data<'a>,
    /// Its offset in the `cmap` table.
    at: usize,
    pub(crate) kind: Kind,
}

/// How the accelerator reads the chosen subtable's codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Windows Symbol (3, 0): also U+0000..U+00FF at U+F000..U+F0FF.
    Symbol,
    /// Macintosh (1, *): ASCII only.
    Mac,
    Unicode,
}

impl<'a> Cmap<'a> {
    /// The table, or `None` where HarfBuzz's sanitizer rejects it.
    pub(crate) fn new(table: &'a [u8]) -> Option<Self> {
        let table = Data(table);
        if table.0.len() < 4 || table.u16(0) != 0 {
            return None;
        }
        let count = usize::from(table.u16(2));
        if 4 + 8 * count > table.0.len() {
            return None;
        }
        let edits: usize = (0..count)
            .map(|i| subtable_edits(table, table.u32(4 + 8 * i + 4) as usize).unwrap_or(1))
            .sum();
        (edits <= MAX_EDITS).then_some(Self { table })
    }

    /// `find_subtable`: the record of `platform` and `encoding` (any for
    /// 0xFFFF) by binary search, as the records stand; `None` for a
    /// neutered subtable.
    fn find(self, platform: u16, encoding: u16) -> Option<usize> {
        let count = usize::from(self.table.u16(2));
        let record = |i: usize| (self.table.u16(4 + 8 * i), self.table.u16(4 + 8 * i + 2));
        let found = bsearch(count, |i| {
            let (p, e) = record(i);
            p.cmp(&platform).then_with(|| {
                if encoding == 0xFFFF {
                    Ordering::Equal
                } else {
                    e.cmp(&encoding)
                }
            })
        })?;
        let offset = self.table.u32(4 + 8 * found + 4) as usize;
        (offset != 0 && subtable_edits(self.table, offset).is_some()).then_some(offset)
    }

    /// `find_best_subtable`: Symbol, then the 32-bit and 16-bit Unicode
    /// subtables in HarfBuzz's order, then Macintosh Roman, then any
    /// Macintosh subtable.
    pub(crate) fn best(self) -> Option<Subtable<'a>> {
        let subtable = |at: usize, kind: Kind| Subtable {
            table: self.table,
            at,
            kind,
        };
        if let Some(at) = self.find(3, 0) {
            return Some(subtable(at, Kind::Symbol));
        }
        let unicode = [
            (3, 10),
            (0, 6),
            (0, 4),
            (3, 1),
            (0, 3),
            (0, 2),
            (0, 1),
            (0, 0),
        ];
        if let Some(at) = unicode.into_iter().find_map(|(p, e)| self.find(p, e)) {
            return Some(subtable(at, Kind::Unicode));
        }
        [(1, 0), (1, 0xFFFF)]
            .into_iter()
            .find_map(|(p, e)| self.find(p, e))
            .map(|at| subtable(at, Kind::Mac))
    }

    /// `find_variation_selector_subtable`: the last (0, 5) record whose
    /// subtable is format 14 (format 15 is compiled out,
    /// `HB_NO_BEYOND_64K`).
    pub(crate) fn variation_sequences(self) -> Option<&'a [u8]> {
        let count = usize::from(self.table.u16(2));
        (0..count)
            .filter(|&i| self.table.u16(4 + 8 * i) == 0 && self.table.u16(4 + 8 * i + 2) == 5)
            .filter_map(|i| {
                let offset = self.table.u32(4 + 8 * i + 4) as usize;
                // A neutered offset reads the table's own header.
                let at = if subtable_edits(self.table, offset).is_some() {
                    offset
                } else {
                    0
                };
                (self.table.u16(at) == 14).then(|| &self.table.0[at..])
            })
            .next_back()
    }
}

impl Subtable<'_> {
    /// `CmapSubtable::get_glyph` of the subtable's format; formats 2, 8 and
    /// 14 map nothing.
    pub(crate) fn glyph(self, code: u32) -> Option<GlyphId> {
        let t = self.table.at(self.at);
        let glyph = match t.u16(0) {
            0 => (code < 256).then(|| u32::from(t.u8(6 + code as usize))),
            4 => format4(t, self.table.0.len() - self.at, code),
            6 => {
                let (first, count) = (u32::from(t.u16(6)), u32::from(t.u16(8)));
                trimmed(t, 10, first, count, code)
            }
            10 => trimmed(t, 20, t.u32(12), t.u32(16), code),
            12 | 13 => {
                let count = t.u32(12) as usize;
                let group = bsearch(count, |i| {
                    let (first, last) = (t.u32(16 + 12 * i), t.u32(20 + 12 * i));
                    if code < first {
                        Ordering::Greater
                    } else if code > last {
                        Ordering::Less
                    } else {
                        Ordering::Equal
                    }
                });
                group.map(|i| {
                    let (first, start) = (t.u32(16 + 12 * i), t.u32(24 + 12 * i));
                    if t.u16(0) == 12 {
                        start.wrapping_add(code - first)
                    } else {
                        start
                    }
                })
            }
            _ => None,
        }?;
        u16::try_from(glyph).ok().filter(|&g| g != 0).map(GlyphId)
    }
}

/// The edits sanitizing the subtable at `offset` makes: `None` when the
/// subtable itself is neutered, else the variation-sequence tables of a
/// format 14 subtable that are. A zero offset needs none.
fn subtable_edits(cmap: Data<'_>, offset: usize) -> Option<usize> {
    if offset == 0 {
        return Some(0);
    }
    let len = cmap.0.len();
    let fits = |at: usize, size: usize| at.checked_add(size).is_some_and(|end| end <= len);
    if !fits(offset, 2) {
        return None;
    }
    let t = cmap.at(offset);
    let ok = match t.u16(0) {
        0 => fits(offset, 262),
        4 => fits(offset, 14) && fits(offset + 14, 2 + 4 * usize::from(t.u16(6))),
        6 => fits(offset, 10) && fits(offset + 10, 2 * usize::from(t.u16(8))),
        10 => fits(offset, 20) && fits(offset + 20, 2 * t.u32(16) as usize),
        12 | 13 => fits(offset, 16) && fits(offset + 16, 12 * t.u32(12) as usize),
        14 => {
            let records = t.u32(6) as usize;
            if !(fits(offset, 10) && fits(offset + 10, 11 * records)) {
                return None;
            }
            // Each default and mapping table outside the table is neutered.
            let table = |at: usize, size: usize| {
                let inner = t.u32(at) as usize;
                let start = offset.saturating_add(inner);
                let whole =
                    fits(start, 4) && fits(start + 4, size * cmap.at(start).u32(0) as usize);
                usize::from(inner != 0 && !whole)
            };
            let edits = (0..records)
                .map(|i| table(10 + 11 * i + 3, 4) + table(10 + 11 * i + 7, 5))
                .sum();
            return Some(edits);
        }
        _ => true,
    };
    ok.then_some(0)
}

/// Format 4 through HarfBuzz's accelerator: the segment found by binary
/// search, its glyph array read to the end of the `cmap` table.
fn format4(t: Data<'_>, size: usize, code: u32) -> Option<u32> {
    let segments = usize::from(t.u16(6) / 2);
    let end = |i: usize| u32::from(t.u16(14 + 2 * i));
    let start = |i: usize| u32::from(t.u16(16 + 2 * segments + 2 * i));
    let i = bsearch(segments, |i| {
        if code > end(i) {
            Ordering::Less
        } else if code < start(i) {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    })?;
    let delta = u32::from(t.u16(16 + 4 * segments + 2 * i));
    let range = usize::from(t.u16(16 + 6 * segments + 2 * i));
    let glyph = if range == 0 {
        code.wrapping_add(delta)
    } else {
        let index = (range / 2 + (code - start(i)) as usize + i).checked_sub(segments)?;
        let values = 16 + 8 * segments;
        if index >= size.saturating_sub(values) / 2 {
            return None;
        }
        match u32::from(t.u16(values + 2 * index)) {
            0 => return None,
            g => g.wrapping_add(delta),
        }
    };
    Some(glyph & 0xFFFF)
}

/// Formats 6 and 10: `count` glyph words for codes from `first`, at `array`.
fn trimmed(t: Data<'_>, array: usize, first: u32, count: u32, code: u32) -> Option<u32> {
    let index = code.checked_sub(first).filter(|&i| i < count)?;
    Some(u32::from(t.u16(array + 2 * index as usize)))
}

#[cfg(test)]
mod tests {
    use super::{Cmap, Kind};
    use triergon_font::GlyphId;

    /// A `cmap` table of `records` (platform, encoding, subtable).
    fn cmap(records: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        let mut t = vec![0, 0];
        t.extend((records.len() as u16).to_be_bytes());
        let mut offset = 4 + 8 * records.len();
        let mut data: Vec<u8> = Vec::new();
        for (p, e, sub) in records {
            t.extend(p.to_be_bytes());
            t.extend(e.to_be_bytes());
            let at = if sub.is_empty() { 0 } else { offset };
            t.extend((at as u32).to_be_bytes());
            offset += sub.len();
            data.extend(sub);
        }
        t.extend(data);
        t
    }

    /// A format 6 subtable mapping codes from `first` to `glyphs`.
    fn format6(first: u16, glyphs: &[u16]) -> Vec<u8> {
        let mut t = Vec::new();
        for v in [
            6,
            10 + 2 * glyphs.len() as u16,
            0,
            first,
            glyphs.len() as u16,
        ] {
            t.extend(v.to_be_bytes());
        }
        for g in glyphs {
            t.extend(g.to_be_bytes());
        }
        t
    }

    #[test]
    fn records_are_found_as_they_stand_without_validation() {
        // HarfBuzz finds (3, 1) even when its subtable is format 14, which
        // maps nothing, and does not fall back to (0, 3).
        let format14 = vec![0, 14, 0, 0, 0, 10, 0, 0, 0, 0];
        let data = cmap(&[(0, 3, format6(0x41, &[1])), (3, 1, format14)]);
        let best = Cmap::new(&data).unwrap().best().unwrap();
        assert_eq!((best.kind, best.glyph(0x41)), (Kind::Unicode, None));
        // A subtable outside the table is neutered and passed over.
        let mut data = cmap(&[(0, 3, format6(0x41, &[1])), (3, 1, format6(0x41, &[2]))]);
        let at = data.len() - 2;
        data[at - 2..at].copy_from_slice(&9_u16.to_be_bytes());
        let best = Cmap::new(&data).unwrap().best().unwrap();
        assert_eq!(best.glyph(0x41), Some(GlyphId(1)));
    }

    #[test]
    fn the_last_format_14_record_has_the_variation_sequences() {
        let format14 = |records: u32| {
            let mut t = vec![0, 14, 0, 0, 0, 10];
            t.extend(records.to_be_bytes());
            t
        };
        let data = cmap(&[
            (0, 5, format14(0)),
            (0, 5, format6(0, &[])),
            (0, 5, format14(0)),
        ]);
        let table = Cmap::new(&data).unwrap().variation_sequences().unwrap();
        assert_eq!(table.len(), 10, "the last record's subtable");
        // A format 14 subtable whose records run out is neutered.
        let data = cmap(&[(0, 5, format14(0)), (0, 5, format14(1))]);
        let table = Cmap::new(&data).unwrap().variation_sequences().unwrap();
        assert_eq!(table.len(), 20, "the first, as the second is neutered");
        // Versions other than 0 drop the table.
        let mut data = cmap(&[(0, 3, format6(0x41, &[1]))]);
        data[1] = 1;
        assert!(Cmap::new(&data).is_none());
    }
}
