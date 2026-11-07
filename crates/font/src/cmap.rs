//! Character to glyph mapping (OpenType `cmap`), formats 0, 4, 6, 10, 12 and 13.
//!
//! Subtable headers are checked when the face is parsed; lookups read the font bytes
//! directly, bounded by the end of the `cmap` table. A declared subtable length is
//! not trusted, because fonts with more than 64 KiB of format 4 data overflow it.

use crate::GlyphId;
use crate::read::{Meter, Reader, slice, u16_at, u32_at};
use crate::sfnt::Table;
use triergon_core::{Budget, BudgetVec, Error, Result};

/// The last Unicode scalar value; mapping iteration stops there.
const MAX_CODE: u32 = 0x10_ffff;

/// One supported `cmap` subtable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Charmap {
    /// The encoding record's platform: 0 Unicode, 1 Macintosh, 3 Windows.
    pub platform: u16,
    /// The platform-specific encoding: for Windows, 0 Symbol, 1 Unicode BMP and 10
    /// full Unicode.
    pub encoding: u16,
    /// The subtable format.
    pub format: u16,
    /// The subtable's offset in the file.
    offset: usize,
    /// The end of the `cmap` table in the file.
    end: usize,
}

/// Reads the encoding records, keeping the supported subtables in directory order.
pub(crate) fn parse(
    data: &[u8],
    table: Table,
    budget: &Budget,
    meter: &mut Meter,
) -> Result<BudgetVec<Charmap>> {
    let bytes = slice(data, table.offset, table.len)?;
    let mut r = Reader::new(bytes);
    if r.u16()? != 0 {
        return Err(Error::Invalid("cmap version"));
    }
    let count = r.u16()?;
    meter.charge(usize::from(count))?;
    let mut charmaps = BudgetVec::new(budget);
    for _ in 0..count {
        let platform = r.u16()?;
        let encoding = r.u16()?;
        let offset = usize::try_from(r.u32()?).map_err(|_| Error::Overflow)?;
        let Some(format) = u16_at(bytes, offset) else {
            continue;
        };
        let charmap = Charmap {
            platform,
            encoding,
            format,
            offset: table.offset + offset,
            end: table.offset + table.len,
        };
        if charmap.header_fits(data) {
            charmaps.try_push(charmap).map_err(|e| e.error)?;
        }
    }
    Ok(charmaps)
}

impl Charmap {
    /// Whether the format is supported and its fixed header and arrays lie in the
    /// table. Format 4's glyph array is checked per lookup instead.
    fn header_fits(&self, data: &[u8]) -> bool {
        let Some(table) = data.get(self.offset..self.end) else {
            return false;
        };
        let at = |offset| u16_at(table, offset).map(usize::from);
        let wide = |offset| u32_at(table, offset).and_then(|n| usize::try_from(n).ok());
        let needed = match self.format {
            0 => Some(6 + 256),
            4 => at(6).map(|twice| 16 + 4 * twice),
            6 => at(8).map(|count| 10 + 2 * count),
            10 => wide(16).and_then(|count| count.checked_mul(2)?.checked_add(20)),
            12 | 13 => wide(12).and_then(|count| count.checked_mul(12)?.checked_add(16)),
            _ => None,
        };
        needed.is_some_and(|needed| needed <= table.len())
    }

    fn table<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        data.get(self.offset..self.end).unwrap_or_default()
    }

    /// The glyph `code` maps to, without the glyph-count check.
    pub(crate) fn lookup(&self, data: &[u8], code: u32) -> Option<u32> {
        let table = self.table(data);
        let glyph = match self.format {
            0 => u32::from(*table.get(6 + usize::try_from(code).ok().filter(|&c| c < 256)?)?),
            4 => u32::from(format4(table, u16::try_from(code).ok()?)?),
            6 => {
                let first = u32::from(u16_at(table, 6)?);
                let count = u32::from(u16_at(table, 8)?);
                trimmed(table, 10, first, count, code)?
            }
            10 => trimmed(table, 20, u32_at(table, 12)?, u32_at(table, 16)?, code)?,
            12 | 13 => groups(table, self.format, code)?,
            _ => return None,
        };
        Some(glyph)
    }

    /// The (code, glyph) pairs in code order, skipping glyph 0 and glyphs at or
    /// above `glyph_count`. Codes in unordered format 4 segments follow segment
    /// order; codes above U+10FFFF are not visited.
    pub(crate) fn mappings<'a>(
        &self,
        data: &'a [u8],
        glyph_count: u32,
    ) -> impl Iterator<Item = (u32, GlyphId)> + 'a {
        let charmap = *self;
        let table = self.table(data);
        let ranges = (0..).map_while(move |i| charmap.code_range(table, i));
        ranges
            .filter(|&(first, _)| first <= MAX_CODE)
            .flat_map(|(first, last)| first..=last.min(MAX_CODE))
            .filter_map(move |code| {
                let glyph = charmap.lookup(data, code)?;
                (glyph != 0 && glyph < glyph_count).then_some((code, GlyphId(glyph)))
            })
    }

    /// Range `i` of codes the subtable can map.
    fn code_range(&self, table: &[u8], i: u32) -> Option<(u32, u32)> {
        let index = usize::try_from(i).ok()?;
        match self.format {
            0 => (i == 0).then_some((0, 255)),
            4 => {
                let count = usize::from(u16_at(table, 6)? / 2);
                (index < count).then_some(())?;
                let end = u16_at(table, 14 + 2 * index)?;
                let start = u16_at(table, 16 + 2 * count + 2 * index)?;
                Some((u32::from(start), u32::from(end.max(start))))
            }
            6 | 10 => {
                (i == 0).then_some(())?;
                let (first, count) = if self.format == 6 {
                    (u32::from(u16_at(table, 6)?), u32::from(u16_at(table, 8)?))
                } else {
                    (u32_at(table, 12)?, u32_at(table, 16)?)
                };
                let last = first.checked_add(count.checked_sub(1)?)?;
                Some((first, last))
            }
            12 | 13 => {
                let count = u32_at(table, 12)?;
                (i < count).then_some(())?;
                let at = 16 + 12 * index;
                let (first, last) = (u32_at(table, at)?, u32_at(table, at + 4)?);
                Some((first, last.max(first)))
            }
            _ => None,
        }
    }
}

/// Formats 6 and 10: `count` glyph words for codes from `first`, at `array`.
fn trimmed(table: &[u8], array: usize, first: u32, count: u32, code: u32) -> Option<u32> {
    let index = code.checked_sub(first).filter(|&i| i < count)?;
    let at = array + 2 * usize::try_from(index).ok()?;
    u16_at(table, at).map(u32::from)
}

/// Format 4: binary search for the first segment whose end is at or after `code`.
fn format4(table: &[u8], code: u16) -> Option<u16> {
    let count = usize::from(u16_at(table, 6)? / 2);
    let end = |i: usize| u16_at(table, 14 + 2 * i);
    let (mut low, mut high) = (0, count);
    while low < high {
        let mid = low + (high - low) / 2;
        if end(mid)? < code {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if low == count {
        return Some(0);
    }
    let start = u16_at(table, 16 + 2 * count + 2 * low)?;
    if code < start {
        return Some(0);
    }
    let delta = u16_at(table, 16 + 4 * count + 2 * low)?;
    let range_at = 16 + 6 * count + 2 * low;
    let range = u16_at(table, range_at)?;
    if range == 0 {
        return Some(code.wrapping_add(delta));
    }
    // Some fonts mark the final 0xFFFF segment with an idRangeOffset of 0xFFFF.
    if range == 0xffff {
        return Some(0);
    }
    let glyph = u16_at(
        table,
        range_at + usize::from(range) + 2 * usize::from(code - start),
    )
    .unwrap_or(0);
    Some(if glyph == 0 {
        0
    } else {
        glyph.wrapping_add(delta)
    })
}

/// Formats 12 and 13: binary search over sorted groups.
fn groups(table: &[u8], format: u16, code: u32) -> Option<u32> {
    let count = usize::try_from(u32_at(table, 12)?).ok()?;
    let (mut low, mut high) = (0, count);
    while low < high {
        let mid = low + (high - low) / 2;
        let at = 16 + 12 * mid;
        let (start, end) = (u32_at(table, at)?, u32_at(table, at + 4)?);
        if code < start {
            high = mid;
        } else if code > end {
            low = mid + 1;
        } else {
            let glyph = u32_at(table, at + 8)?;
            return Some(if format == 13 {
                glyph
            } else {
                glyph.checked_add(code - start)?
            });
        }
    }
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::{Charmap, parse};
    use crate::GlyphId;
    use crate::read::Meter;
    use crate::sfnt::Table;
    use triergon_core::Budget;

    fn words(values: &[u16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    /// A `cmap` with one Windows Unicode record pointing at `subtable`.
    fn cmap(subtable: &[u8]) -> Vec<u8> {
        let mut data = words(&[0, 1, 3, 1]);
        data.extend(12_u32.to_be_bytes());
        data.extend(subtable);
        data
    }

    fn charmaps(data: &[u8]) -> Vec<Charmap> {
        let budget = Budget::new(1 << 20).unwrap();
        let table = Table {
            offset: 0,
            len: data.len(),
        };
        parse(data, table, &budget, &mut Meter::new(100))
            .unwrap()
            .as_slice()
            .to_vec()
    }

    #[test]
    fn format4_deltas_glyph_arrays_and_the_sentinel() {
        // Segments: 'A'..='C' delta +1; 'a'..='b' through the glyph array; 0xFFFF.
        let mut subtable = words(&[4, 0, 0, 6, 0, 0, 0]);
        subtable.extend(words(&[0x43, 0x62, 0xffff, 0]));
        subtable.extend(words(&[0x41, 0x61, 0xffff]));
        subtable.extend(words(&[1, 10, 1]));
        // idRangeOffset of segment 1 counts from its own word to the array after
        // the last idRangeOffset word: two words further on.
        subtable.extend(words(&[0, 4, 0]));
        subtable.extend(words(&[7, 0]));
        let data = cmap(&subtable);
        let [charmap] = charmaps(&data)[..] else {
            panic!("one charmap");
        };
        assert_eq!(
            (charmap.platform, charmap.encoding, charmap.format),
            (3, 1, 4)
        );
        let lookup = |code| charmap.lookup(&data, code);
        assert_eq!(lookup(0x41), Some(0x42));
        assert_eq!(lookup(0x44), Some(0));
        assert_eq!(lookup(0x61), Some(17));
        assert_eq!(lookup(0x62), Some(0));
        assert_eq!(lookup(0xffff), Some(0));
        assert_eq!(lookup(0x1_0000), None);
        let pairs: Vec<_> = charmap.mappings(&data, 0x43).collect();
        assert_eq!(pairs, [(0x41, GlyphId(0x42)), (0x61, GlyphId(17))]);
    }

    #[test]
    fn format12_and_13_groups() {
        let mut subtable = words(&[12, 0]);
        subtable.extend(40_u32.to_be_bytes());
        subtable.extend(0_u32.to_be_bytes());
        subtable.extend(2_u32.to_be_bytes());
        for value in [0x20_u32, 0x21, 5, 0x1_f600, 0x1_f600, 9] {
            subtable.extend(value.to_be_bytes());
        }
        let data = cmap(&subtable);
        let charmap = charmaps(&data)[0];
        assert_eq!(charmap.lookup(&data, 0x21), Some(6));
        assert_eq!(charmap.lookup(&data, 0x1_f600), Some(9));
        assert_eq!(charmap.lookup(&data, 0x22), Some(0));
        let mut many = data.clone();
        many[13] = 13;
        assert_eq!(charmaps(&many)[0].lookup(&many, 0x21), Some(5));
        // A group count beyond the table drops the subtable.
        let mut short = data;
        short[12 + 15] = 3;
        assert!(charmaps(&short).is_empty());
    }

    #[test]
    fn trimmed_formats_and_unsupported_subtables() {
        let data = cmap(&words(&[6, 0, 0, 0x30, 2, 4, 5]));
        let charmap = charmaps(&data)[0];
        assert_eq!(
            [0x2f, 0x30, 0x31, 0x32].map(|c| charmap.lookup(&data, c)),
            [None, Some(4), Some(5), None]
        );
        assert_eq!(charmap.mappings(&data, 10).count(), 2);
        assert!(charmaps(&cmap(&words(&[2, 0, 0]))).is_empty());
        assert!(charmaps(&cmap(&words(&[0, 262, 0]))).is_empty());
    }
}
