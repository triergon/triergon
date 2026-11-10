//! The `cmap` table as HarfBuzz 14.6.0's built-in font functions read it
//! (`hb-ot-cmap-table.hh`): sanitized as `hb_sanitize_context_t` sanitizes
//! it, subtables found by binary search over the raw encoding records, and
//! looked up with each format's `get_glyph` and the accelerator's remapping
//! for symbol and Macintosh subtables.
//!
//! HarfBuzz does not validate subtables as FreeType does, and its sanitizer
//! never edits a table: a table any of whose subtables or variation-sequence
//! tables lies outside it, whose version is not 0, or whose checks exceed the
//! sanitizer's operation budget is no table, and maps nothing.

#[rustfmt::skip] // Laid out by the generator.
mod tables;

use crate::ot::data::{Data, bsearch};
use std::cmp::Ordering;
use tables::{ARABIC_PUA_U8, ARABIC_PUA_U16, MACROMAN};
use triergon_font::GlyphId;

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
    remap: Remap,
}

/// How the accelerator reads the chosen subtable's codes
/// (`get_glyph_from_symbol`, `_from_macroman` and `_from_ascii`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Remap {
    /// Codes as they are.
    None,
    /// Windows Symbol in no font page: U+0000..U+00FF also at U+F000.
    SymbolPua,
    /// Windows Symbol in the Simplified Arabic font page.
    ArabicSimplified,
    /// Windows Symbol in the Traditional Arabic font page.
    ArabicTraditional,
    /// Macintosh Roman: ASCII, then Unicode converted to Macintosh Roman.
    MacRoman,
    /// Another Macintosh encoding: ASCII only.
    Ascii,
}

/// `hb_sanitize_context_t` over the `cmap` table: bounds checks, and the
/// budget of bytes its range checks may cover (`max_ops`).
struct Sanitizer {
    len: usize,
    ops: i64,
}

impl Sanitizer {
    fn new(len: usize) -> Self {
        // HB_SANITIZE_MAX_OPS_FACTOR, _MIN and _MAX.
        let ops = u32::try_from(len)
            .ok()
            .and_then(|n| n.checked_mul(64))
            .map_or(0x3FFF_FFFF, |m| m.clamp(16384, 0x3FFF_FFFF));
        Self {
            len,
            ops: i64::from(ops),
        }
    }

    /// `check_struct`: `size` bytes at `at`, uncharged.
    fn fits(&self, at: usize, size: usize) -> bool {
        at.checked_add(size).is_some_and(|end| end <= self.len)
    }

    /// `check_range`: `size` bytes at `at`, charged to the budget.
    fn range(&mut self, at: Option<usize>, size: Option<usize>) -> bool {
        let (Some(at), Some(size)) = (at, size) else {
            return false;
        };
        if at > self.len || self.len - at < size {
            return false;
        }
        self.ops -= i64::try_from(size).unwrap_or(i64::MAX);
        self.ops > 0
    }

    /// The whole table (`cmap::sanitize`).
    fn table(&mut self, t: Data<'_>) -> bool {
        if !self.fits(0, 4) || t.u16(0) != 0 {
            return false;
        }
        let count = usize::from(t.u16(2));
        if !self.range(Some(4), Some(8 * count)) {
            return false;
        }
        (0..count).all(|i| {
            let offset = t.u32(4 + 8 * i + 4) as usize;
            offset == 0 || self.subtable(t, offset)
        })
    }

    /// `CmapSubtable::sanitize` of the subtable at `at`.
    fn subtable(&mut self, t: Data<'_>, at: usize) -> bool {
        let field = |n: usize| at.checked_add(n);
        // A 32-bit count of `size`-byte entries, as `check_array_sized`
        // multiplies it.
        let entries = |count: u32, size: u32| count.checked_mul(size).map(|n| n as usize);
        if !self.fits(at, 2) {
            return false;
        }
        let s = t.at(at);
        match s.u16(0) {
            0 => self.fits(at, 262),
            4 => self.fits(at, 14) && self.range(field(14), Some(2 + 4 * usize::from(s.u16(6)))),
            6 => self.fits(at, 10) && self.range(field(10), Some(2 * usize::from(s.u16(8)))),
            10 => self.fits(at, 20) && self.range(field(20), entries(s.u32(16), 2)),
            12 | 13 => self.fits(at, 16) && self.range(field(16), entries(s.u32(12), 12)),
            14 => {
                let records = s.u32(6);
                if !(self.fits(at, 10) && self.range(field(10), entries(records, 11))) {
                    return false;
                }
                (0..records as usize).all(|i| {
                    let record = 10 + 11 * i;
                    self.uvs(t, at, s.u32(record + 3), 4) && self.uvs(t, at, s.u32(record + 7), 5)
                })
            }
            _ => true,
        }
    }

    /// A default (`size` 4) or mapping (`size` 5) table at `offset` from the
    /// format 14 subtable at `base`.
    fn uvs(&mut self, t: Data<'_>, base: usize, offset: u32, size: u32) -> bool {
        if offset == 0 {
            return true;
        }
        let Some(at) = base.checked_add(offset as usize) else {
            return false;
        };
        self.fits(at, 4)
            && self.range(
                at.checked_add(4),
                t.u32(at).checked_mul(size).map(|n| n as usize),
            )
    }
}

impl<'a> Cmap<'a> {
    /// The table, or `None` where HarfBuzz's sanitizer rejects it.
    pub(crate) fn new(table: &'a [u8]) -> Option<Self> {
        let table = Data(table);
        Sanitizer::new(table.0.len())
            .table(table)
            .then_some(Self { table })
    }

    /// `find_subtable`: the record of `platform` and `encoding` (any for
    /// 0xFFFF) by binary search, as the records stand; `None` for an offset
    /// of zero.
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
        (offset != 0).then_some(offset)
    }

    /// `find_best_subtable`: Symbol, then the 32-bit and 16-bit Unicode
    /// subtables in HarfBuzz's order, then Macintosh Roman, then any
    /// Macintosh subtable. A Symbol subtable's codes are remapped by the
    /// font page of `os2`, the face's `OS/2` table.
    pub(crate) fn best(self, os2: &[u8]) -> Option<Subtable<'a>> {
        let subtable = |at: usize, remap: Remap| Subtable {
            table: self.table,
            at,
            remap,
        };
        if let Some(at) = self.find(3, 0) {
            // `OS2::get_font_page`: the high byte of `fsSelection` in a
            // version 0 table that sanitizes, else none.
            let os2 = Data(os2);
            let page = if os2.0.len() >= 78 && os2.u16(0) == 0 {
                os2.u16(62) & 0xFF00
            } else {
                0
            };
            let remap = match page {
                0 => Remap::SymbolPua,
                0xB200 => Remap::ArabicSimplified,
                0xB300 => Remap::ArabicTraditional,
                _ => Remap::None,
            };
            return Some(subtable(at, remap));
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
            return Some(subtable(at, Remap::None));
        }
        if let Some(at) = self.find(1, 0) {
            return Some(subtable(at, Remap::MacRoman));
        }
        self.find(1, 0xFFFF).map(|at| subtable(at, Remap::Ascii))
    }

    /// `find_variation_selector_subtable`: the last (0, 5) record whose
    /// subtable is format 14 (format 15 is compiled out,
    /// `HB_NO_BEYOND_64K`).
    pub(crate) fn variation_sequences(self) -> Option<&'a [u8]> {
        let count = usize::from(self.table.u16(2));
        (0..count)
            .filter(|&i| self.table.u16(4 + 8 * i) == 0 && self.table.u16(4 + 8 * i + 2) == 5)
            .filter_map(|i| {
                // An offset of zero reads the table's own header.
                let at = self.table.u32(4 + 8 * i + 4) as usize;
                (self.table.u16(at) == 14).then(|| &self.table.0[at..])
            })
            .next_back()
    }
}

impl Subtable<'_> {
    /// The glyph `c` maps to, through the accelerator's remapping.
    pub(crate) fn glyph(self, c: u32) -> Option<GlyphId> {
        let remapped = |code: u32| (code != 0).then(|| self.get(code)).flatten();
        match self.remap {
            Remap::None => self.get(c),
            Remap::SymbolPua => self
                .get(c)
                .or_else(|| (c <= 0xFF).then(|| self.get(0xF000 + c)).flatten()),
            Remap::ArabicSimplified => self.get(c).or_else(|| remapped(arabic_simplified(c))),
            Remap::ArabicTraditional => self.get(c).or_else(|| remapped(arabic_traditional(c))),
            Remap::MacRoman => (c < 0x80)
                .then(|| self.get(c))
                .flatten()
                .or_else(|| remapped(macroman(c))),
            Remap::Ascii => (c < 0x80).then(|| self.get(c)).flatten(),
        }
    }

    /// `CmapSubtable::get_glyph` of the subtable's format; formats 2, 8 and
    /// 14 map nothing.
    fn get(self, code: u32) -> Option<GlyphId> {
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
                let group = |i: usize| 16 + 12 * i;
                let found = bsearch(count, |i| {
                    let (first, last) = (t.u32(group(i)), t.u32(group(i) + 4));
                    if code < first {
                        Ordering::Greater
                    } else if code > last {
                        Ordering::Less
                    } else {
                        Ordering::Equal
                    }
                });
                found.map(|i| {
                    let (first, start) = (t.u32(group(i)), t.u32(group(i) + 8));
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

/// Format 4 through HarfBuzz's accelerator: the segment found by binary
/// search, its glyph array read to the end of the `cmap` table (`size`
/// bytes from the subtable).
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
    let at = (index as usize).checked_mul(2)?.checked_add(array)?;
    Some(u32::from(t.u16(at)))
}

/// `unicode_to_macroman`: zero for a code point Macintosh Roman lacks.
fn macroman(u: u32) -> u32 {
    let Ok(u) = u16::try_from(u) else {
        return 0;
    };
    bsearch(MACROMAN.len(), |i| MACROMAN[i].0.cmp(&u)).map_or(0, |i| u32::from(MACROMAN[i].1))
}

/// `_hb_arabic_pua_b4`: the 4-bit entry `i` of `a`.
fn nibble(a: &[u8], i: usize) -> usize {
    usize::from(a.get(i >> 1).copied().unwrap_or(0) >> ((i & 1) << 2)) & 15
}

fn byte(i: usize) -> usize {
    usize::from(ARABIC_PUA_U8.get(i).copied().unwrap_or(0))
}

fn code_point(i: usize) -> u32 {
    u32::from(ARABIC_PUA_U16.get(i).copied().unwrap_or(0))
}

/// `_hb_arabic_pua_simp_map`: the Unicode Arabic character a Simplified
/// Arabic font page's private-use code stands for, or zero.
fn arabic_simplified(u: u32) -> u32 {
    if u >= 65277 {
        return 0;
    }
    let u = u as usize;
    let top = ((13_835_058_055_282_164_225_u64 >> ((u >> 11) << 1)) & 3) as usize;
    let middle = nibble(&ARABIC_PUA_U8, (top << 4) + ((u >> 7) & 15));
    let leaf = byte(31 + (middle << 4) + ((u >> 3) & 15));
    code_point((leaf << 3) + (u & 7))
}

/// `_hb_arabic_pua_trad_map`: the same for a Traditional Arabic font page.
fn arabic_traditional(u: u32) -> u32 {
    if u >= 65277 {
        return 0;
    }
    let u = u as usize;
    let top = nibble(&ARABIC_PUA_U8[127..], u >> 10);
    let middle = nibble(&ARABIC_PUA_U8[159..], (top << 4) + ((u >> 6) & 15));
    let leaf = byte(197 + (middle << 4) + ((u >> 2) & 15));
    code_point(317 + (leaf << 2) + (u & 3))
}

#[cfg(test)]
mod tests {
    use super::{Cmap, arabic_simplified, arabic_traditional, macroman};
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

    /// A format 14 subtable of `records` variation-selector records, none of
    /// them written.
    fn format14(records: u32) -> Vec<u8> {
        let mut t = vec![0, 14, 0, 0, 0, 10];
        t.extend(records.to_be_bytes());
        t
    }

    #[test]
    fn records_are_found_as_they_stand_without_validation() {
        // HarfBuzz finds (3, 1) even when its subtable is format 14, which
        // maps nothing, and does not fall back to (0, 3).
        let data = cmap(&[(0, 3, format6(0x41, &[1])), (3, 1, format14(0))]);
        let best = Cmap::new(&data).unwrap().best(&[]).unwrap();
        assert_eq!(best.glyph(0x41), None);
        let data = cmap(&[(0, 3, format6(0x41, &[1])), (3, 1, format6(0x41, &[2]))]);
        let best = Cmap::new(&data).unwrap().best(&[]).unwrap();
        assert_eq!(best.glyph(0x41), Some(GlyphId(2)));
    }

    #[test]
    fn a_subtable_outside_the_table_drops_the_table() {
        // A glyph array running past the table: nothing maps.
        let mut data = cmap(&[(0, 3, format6(0x41, &[1])), (3, 1, format6(0x41, &[2]))]);
        let at = data.len() - 4;
        data[at..at + 2].copy_from_slice(&9_u16.to_be_bytes());
        assert!(Cmap::new(&data).is_none());
        // So do variation-selector records that run past it, a group count
        // whose bytes overflow 32 bits, and a version other than 0.
        assert!(Cmap::new(&cmap(&[(0, 5, format14(1))])).is_none());
        let mut format12 = vec![0, 12, 0, 0, 0, 0, 0, 16, 0, 0, 0, 0];
        format12.extend(0x1555_5556_u32.to_be_bytes());
        assert!(Cmap::new(&cmap(&[(3, 10, format12)])).is_none());
        let mut data = cmap(&[(0, 3, format6(0x41, &[1]))]);
        data[1] = 1;
        assert!(Cmap::new(&data).is_none());
    }

    #[test]
    fn records_pointing_at_one_large_subtable_exhaust_the_budget() {
        // Each record checks the subtable's glyph array again, charging it
        // to a budget of 64 bytes per byte of the table.
        let glyphs = vec![1_u16; 4000];
        assert!(Cmap::new(&cmap(&[(3, 1, format6(0, &glyphs))])).is_some());
        let records = 200_u16;
        let mut many = vec![0, 0];
        many.extend(records.to_be_bytes());
        let subtable = 4 + 8 * u32::from(records);
        for _ in 0..records {
            many.extend([0, 3, 0, 1]);
            many.extend(subtable.to_be_bytes());
        }
        many.extend(format6(0, &glyphs));
        assert!(Cmap::new(&many).is_none());
    }

    #[test]
    fn the_last_format_14_record_has_the_variation_sequences() {
        let data = cmap(&[
            (0, 5, format14(0)),
            (0, 5, format6(0, &[])),
            (0, 5, format14(0)),
        ]);
        let table = Cmap::new(&data).unwrap().variation_sequences().unwrap();
        assert_eq!(table.len(), 10, "the last record's subtable");
    }

    #[test]
    fn macintosh_and_symbol_subtables_remap_codes_as_harfbuzz_does() {
        assert_eq!(
            (macroman(0xE9), macroman(0x41), macroman(0x1_0000)),
            (0x8E, 0, 0)
        );
        // 'é' through a Macintosh Roman subtable mapping 0x8E.
        let mut format0 = vec![0, 0, 1, 6, 0, 0];
        format0.extend([0; 256]);
        format0[6 + 0x8E] = 5;
        format0[6 + 0x41] = 3;
        let data = cmap(&[(1, 0, format0.clone())]);
        let mac = Cmap::new(&data).unwrap().best(&[]).unwrap();
        assert_eq!(
            (mac.glyph(0xE9), mac.glyph(0x41)),
            (Some(GlyphId(5)), Some(GlyphId(3)))
        );
        // Another Macintosh encoding maps ASCII only.
        let data = cmap(&[(1, 1, format0)]);
        let other = Cmap::new(&data).unwrap().best(&[]).unwrap();
        assert_eq!(
            (other.glyph(0xE9), other.glyph(0x41)),
            (None, Some(GlyphId(3)))
        );
        // A symbol subtable at U+F041: by the OS/2 font page, U+0041 maps
        // there, or not at all on a Hebrew page.
        let data = cmap(&[(3, 0, format6(0xF041, &[7]))]);
        let mut os2 = vec![0_u8; 78];
        let symbol = Cmap::new(&data).unwrap().best(&os2).unwrap();
        assert_eq!(symbol.glyph(0x41), Some(GlyphId(7)));
        os2[62] = 0xB1;
        let hebrew = Cmap::new(&data).unwrap().best(&os2).unwrap();
        assert_eq!(hebrew.glyph(0x41), None);
        // The Arabic font pages look Unicode text up at their private-use
        // codes, as HarfBuzz's packed tables give them.
        assert_eq!(
            [0x20, 0xFEFB, 0xFEFC, 0x10].map(arabic_simplified),
            [0xF120, 0xF1BD, 0xF1BC, 0]
        );
        assert_eq!(
            [0x20, 0xFEFB, 0xFEFC, 0xFFFF].map(arabic_traditional),
            [0xF220, 0xF2DC, 0xF2DD, 0]
        );
    }
}
