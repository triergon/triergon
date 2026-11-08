//! Character to glyph mapping (OpenType `cmap`), formats 0, 4, 6, 10, 12 and 13.
//!
//! Subtable headers are checked when the face is parsed; lookups read the font bytes
//! directly, bounded by the end of the `cmap` table. A declared subtable length is
//! not trusted, because fonts with more than 64 KiB of format 4 data overflow it.

use crate::GlyphId;
use crate::name::Platform;
use crate::read::{Reader, slice, u16_at, u32_at};
use crate::sfnt::Table;
use crate::work::charge;
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// The last Unicode scalar value; mapping iteration stops there.
const MAX_CODE: u32 = 0x10_ffff;

/// A supported subtable format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmapFormat {
    /// Format 0: one byte per code 0 to 255.
    ByteEncoding,
    /// Format 4: segments of 16-bit codes with deltas or glyph arrays.
    SegmentDelta,
    /// Format 6: one run of 16-bit codes.
    Trimmed,
    /// Format 10: one run of 32-bit codes.
    TrimmedArray,
    /// Format 12: groups of consecutive codes mapping to consecutive glyphs.
    SegmentedCoverage,
    /// Format 13: groups of codes mapping to one glyph.
    ManyToOne,
}

impl CmapFormat {
    const fn from_number(number: u16) -> Option<Self> {
        Some(match number {
            0 => Self::ByteEncoding,
            4 => Self::SegmentDelta,
            6 => Self::Trimmed,
            10 => Self::TrimmedArray,
            12 => Self::SegmentedCoverage,
            13 => Self::ManyToOne,
            _ => return None,
        })
    }

    /// The format number the subtable stores.
    pub const fn number(self) -> u16 {
        match self {
            Self::ByteEncoding => 0,
            Self::SegmentDelta => 4,
            Self::Trimmed => 6,
            Self::TrimmedArray => 10,
            Self::SegmentedCoverage => 12,
            Self::ManyToOne => 13,
        }
    }

    /// Whether FreeType's default validation accepts the subtable, given the
    /// bytes from its start to the end of the `cmap` table.
    fn valid(self, table: &[u8]) -> bool {
        self.check(table).is_some()
    }

    fn check(self, table: &[u8]) -> Option<()> {
        let short = |at| u16_at(table, at).map(usize::from);
        let long = |at| u32_at(table, at).and_then(|n| usize::try_from(n).ok());
        let limit = table.len();
        let fits = |ok: bool| ok.then_some(());
        match self {
            Self::ByteEncoding => {
                let length = short(2)?;
                fits(length <= limit && length >= 262)
            }
            Self::SegmentDelta => segment_delta_valid(table),
            Self::Trimmed => {
                fits(limit >= 10)?;
                let (length, count) = (short(2)?, short(8)?);
                fits(length <= limit && length >= 10 + 2 * count)
            }
            Self::TrimmedArray => {
                fits(limit >= 20)?;
                let (length, count) = (long(4)?, long(16)?);
                fits(length <= limit && length >= 20 && (length - 20) / 2 >= count)
            }
            Self::SegmentedCoverage | Self::ManyToOne => {
                fits(limit >= 16)?;
                let (length, groups) = (long(4)?, long(12)?);
                fits(length <= limit && length >= 16 && (length - 16) / 12 >= groups)?;
                // Groups ascend without overlapping.
                let mut last = None;
                for g in 0..groups {
                    let start = u32_at(table, 16 + 12 * g)?;
                    let end = u32_at(table, 20 + 12 * g)?;
                    fits(start <= end && last.is_none_or(|last| start > last))?;
                    last = Some(end);
                }
                Some(())
            }
        }
    }
}

/// `tt_cmap4_validate` at FreeType's default level: the segment arrays fit the
/// rest of the `cmap` table, no segment starts after it ends, and glyph-array
/// offsets stay within the table (FreeType lets a final 0xFFFF segment off).
fn segment_delta_valid(table: &[u8]) -> Option<()> {
    let limit = table.len();
    let fits = |ok: bool| ok.then_some(());
    // FreeType measures the subtable to the end of `cmap`, whatever its length says.
    fits(limit >= 16)?;
    let segments = usize::from(u16_at(table, 6)? / 2);
    fits(limit >= 16 + 8 * segments)?;
    let (starts, offsets) = (16 + 2 * segments, 16 + 6 * segments);
    let glyph_ids = offsets + 2 * segments;
    for n in 0..segments {
        let end = usize::from(u16_at(table, 14 + 2 * n)?);
        let start = usize::from(u16_at(table, starts + 2 * n)?);
        let offset = usize::from(u16_at(table, offsets + 2 * n)?);
        fits(start <= end)?;
        let sentinel = n == segments - 1 && start == 0xffff && end == 0xffff;
        if offset == 0xffff {
            fits(sentinel)?;
        } else if offset != 0 && !sentinel {
            let array = offsets + 2 * n + offset;
            fits(array >= glyph_ids && array + 2 * (end - start + 1) <= limit)?;
        }
    }
    Some(())
}

/// What a platform and encoding number mean, for cmap subtables and name
/// records alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharEncoding {
    /// Unicode BMP: Windows encoding 1, or Unicode-platform encodings 0 to 3.
    Unicode,
    /// Full Unicode: Windows encoding 10, or Unicode-platform encodings 4 and 6.
    UnicodeFull,
    /// Unicode variation sequences (Unicode-platform encoding 5, cmap format 14).
    UnicodeVariations,
    /// Windows Symbol (encoding 0): codes usually in U+F020..U+F0FF.
    Symbol,
    /// Macintosh Roman (Macintosh encoding 0).
    MacRoman,
    /// Any other platform-specific encoding.
    Other,
}

impl CharEncoding {
    /// The encoding `id` names on `platform`.
    pub const fn of(platform: Platform, id: u16) -> Self {
        match (platform, id) {
            (Platform::Windows, 1) | (Platform::Unicode, 0..=3) => Self::Unicode,
            (Platform::Windows, 10) | (Platform::Unicode, 4 | 6) => Self::UnicodeFull,
            (Platform::Unicode, 5) => Self::UnicodeVariations,
            (Platform::Windows, 0) => Self::Symbol,
            (Platform::Macintosh, 0) => Self::MacRoman,
            _ => Self::Other,
        }
    }
}

/// One supported subtable of a face; [`Charmap`] is its public view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Subtable {
    platform: Platform,
    encoding: u16,
    format: CmapFormat,
    /// The subtable's offset in the file.
    offset: usize,
    /// The end of the `cmap` table in the file.
    end: usize,
}

/// Reads the encoding records, keeping the supported subtables in directory order.
/// As in FreeType, a table too short for its header has no subtables, records
/// stop at the table's end, and a record at offset 0 is skipped.
pub(crate) fn parse(
    data: &[u8],
    table: Table,
    budget: &Budget,
    work: &mut WorkBudget,
) -> Result<BudgetVec<Subtable>> {
    let bytes = slice(data, table.offset, table.len)?;
    let mut subtables = BudgetVec::new(budget);
    // The version is ignored: the format allows newer subtables under 0.
    let Some(count) = u16_at(bytes, 2) else {
        return Ok(subtables);
    };
    let records = usize::from(count).min(bytes.len().saturating_sub(4) / 8);
    charge(work, records)?;
    let mut r = Reader::at(bytes, 4)?;
    for _ in 0..records {
        let platform = Platform::from_id(r.u16()?);
        let encoding = r.u16()?;
        let offset = usize::try_from(r.u32()?).map_err(|_| Error::Overflow)?;
        if offset == 0 {
            continue;
        }
        let Some(format) = u16_at(bytes, offset).and_then(CmapFormat::from_number) else {
            continue;
        };
        if bytes.get(offset..).is_some_and(|sub| format.valid(sub)) {
            let subtable = Subtable {
                platform,
                encoding,
                format,
                offset: table.offset + offset,
                end: table.offset + table.len,
            };
            subtables.try_push(subtable).map_err(|e| e.error)?;
        }
    }
    Ok(subtables)
}

/// The subtable libass selects for text when it opens a face
/// (`ass_charmap_magic`): the first Windows (3, 10), else the first (3, 1),
/// else the first Windows subtable of another encoding, else FreeType's
/// default, else the first subtable.
pub(crate) fn text_subtable(subtables: &[Subtable]) -> Option<&Subtable> {
    let windows = |encoding: Option<u16>| {
        subtables.iter().find(move |s| {
            s.platform == Platform::Windows && encoding.is_none_or(|e| s.encoding == e)
        })
    };
    windows(Some(10))
        .or_else(|| windows(Some(1)))
        .or_else(|| windows(None))
        .or_else(|| freetype_default(subtables))
        .or_else(|| subtables.first())
}

/// The charmap FreeType selects when it opens a face (`find_unicode_charmap`):
/// the last full-Unicode subtable, else the last of the encodings FreeType
/// calls Unicode (every Unicode- and ISO-platform subtable, Windows 1 and 10).
fn freetype_default(subtables: &[Subtable]) -> Option<&Subtable> {
    let full = |s: &&Subtable| match (s.platform, s.encoding) {
        (Platform::Windows, 10) | (Platform::Unicode, 4) => true,
        (Platform::Unicode, 6) => s.format == CmapFormat::ManyToOne,
        _ => false,
    };
    let unicode = |s: &&Subtable| match s.platform {
        Platform::Unicode | Platform::Iso => true,
        Platform::Windows => matches!(s.encoding, 1 | 10),
        _ => false,
    };
    // FreeType skips formats 14 and 15 there, which are not listed.
    let mut last = subtables.iter().rev();
    last.clone().find(full).or_else(|| last.find(unicode))
}

impl Subtable {
    pub(crate) const fn platform(&self) -> Platform {
        self.platform
    }

    pub(crate) const fn encoding_id(&self) -> u16 {
        self.encoding
    }

    pub(crate) const fn encoding(&self) -> CharEncoding {
        CharEncoding::of(self.platform, self.encoding)
    }

    fn table<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        data.get(self.offset..self.end).unwrap_or_default()
    }

    /// The glyph `code` maps to, without the glyph-count check.
    fn lookup(&self, table: &[u8], code: u32) -> Option<u32> {
        match self.format {
            CmapFormat::ByteEncoding => {
                let code = usize::try_from(code).ok().filter(|&c| c < 256)?;
                table.get(6 + code).map(|&g| u32::from(g))
            }
            CmapFormat::SegmentDelta => {
                segment_delta(table, u16::try_from(code).ok()?).map(u32::from)
            }
            CmapFormat::Trimmed => {
                let first = u32::from(u16_at(table, 6)?);
                let count = u32::from(u16_at(table, 8)?);
                trimmed(table, 10, first, count, code)
            }
            CmapFormat::TrimmedArray => {
                trimmed(table, 20, u32_at(table, 12)?, u32_at(table, 16)?, code)
            }
            CmapFormat::SegmentedCoverage => groups(table, code, true),
            CmapFormat::ManyToOne => groups(table, code, false),
        }
    }

    /// Range `i` of codes the subtable can map, in subtable order.
    fn code_range(&self, table: &[u8], i: usize) -> Option<(u32, u32)> {
        match self.format {
            CmapFormat::ByteEncoding => (i == 0).then_some((0, 255)),
            CmapFormat::SegmentDelta => {
                let count = usize::from(u16_at(table, 6)? / 2);
                (i < count).then_some(())?;
                let end = u16_at(table, 14 + 2 * i)?;
                let start = u16_at(table, 16 + 2 * count + 2 * i)?;
                Some((u32::from(start), u32::from(end.max(start))))
            }
            CmapFormat::Trimmed | CmapFormat::TrimmedArray => {
                (i == 0).then_some(())?;
                let (first, count) = if self.format == CmapFormat::Trimmed {
                    (u32::from(u16_at(table, 6)?), u32::from(u16_at(table, 8)?))
                } else {
                    (u32_at(table, 12)?, u32_at(table, 16)?)
                };
                Some((first, first.checked_add(count.checked_sub(1)?)?))
            }
            CmapFormat::SegmentedCoverage | CmapFormat::ManyToOne => {
                let count = usize::try_from(u32_at(table, 12)?).ok()?;
                (i < count).then_some(())?;
                let at = 16 + 12 * i;
                let (first, last) = (u32_at(table, at)?, u32_at(table, at + 4)?);
                Some((first, last.max(first)))
            }
        }
    }
}

/// A `cmap` subtable of one face. It borrows the face's bytes, so it cannot be used
/// with another face.
#[derive(Clone, Copy, Debug)]
pub struct Charmap<'f> {
    data: &'f [u8],
    glyph_count: u32,
    subtable: Subtable,
    work: u64,
}

impl<'f> Charmap<'f> {
    pub(crate) const fn new(
        data: &'f [u8],
        glyph_count: u32,
        subtable: Subtable,
        work: u64,
    ) -> Self {
        Self {
            data,
            glyph_count,
            subtable,
            work,
        }
    }

    pub const fn platform(&self) -> Platform {
        self.subtable.platform()
    }

    /// The encoding record's platform-specific encoding number.
    pub const fn encoding_id(&self) -> u16 {
        self.subtable.encoding_id()
    }

    pub const fn encoding(&self) -> CharEncoding {
        self.subtable.encoding()
    }

    pub const fn format(&self) -> CmapFormat {
        self.subtable.format
    }

    /// The glyph `code` maps to, looked up unchanged. Unmapped codes and glyphs at or
    /// above the face's glyph count give `None`.
    pub fn glyph(&self, code: u32) -> Option<GlyphId> {
        let glyph = self.subtable.lookup(self.subtable.table(self.data), code)?;
        let glyph = u16::try_from(glyph)
            .ok()
            .filter(|&g| g != 0 && u32::from(g) < self.glyph_count)?;
        Some(GlyphId(glyph))
    }

    /// Every (code, glyph) pair without glyph 0, by segment or group in subtable
    /// order and by code within each; codes above U+10FFFF are not visited. Each
    /// code visited costs one unit of the face's work limit; when it runs out the
    /// iterator yields `Error::Policy` once and ends.
    pub fn mappings(&self) -> Mappings<'f> {
        Mappings {
            charmap: *self,
            range: 0,
            next: None,
            work: WorkBudget::new(self.work),
            done: false,
        }
    }
}

/// The iterator of [`Charmap::mappings`].
#[derive(Debug)]
pub struct Mappings<'f> {
    charmap: Charmap<'f>,
    range: usize,
    /// The next code and the last code of the current range.
    next: Option<(u32, u32)>,
    work: WorkBudget,
    done: bool,
}

impl Iterator for Mappings<'_> {
    type Item = Result<(u32, GlyphId)>;

    fn next(&mut self) -> Option<Self::Item> {
        let table = self.charmap.subtable.table(self.charmap.data);
        while !self.done {
            let Some((code, last)) = self.next else {
                let Some((first, last)) = self.charmap.subtable.code_range(table, self.range)
                else {
                    self.done = true;
                    break;
                };
                self.range += 1;
                if first <= MAX_CODE {
                    self.next = Some((first, last.min(MAX_CODE)));
                }
                continue;
            };
            self.next = (code < last).then_some((code + 1, last));
            if !self.work.try_charge(1) {
                self.done = true;
                return Some(Err(Error::Policy("font work")));
            }
            if let Some(glyph) = self.charmap.glyph(code) {
                return Some(Ok((code, glyph)));
            }
        }
        None
    }
}

/// Formats 6 and 10: `count` glyph words for codes from `first`, at `array`.
fn trimmed(table: &[u8], array: usize, first: u32, count: u32, code: u32) -> Option<u32> {
    let index = code.checked_sub(first).filter(|&i| i < count)?;
    let at = array + 2 * usize::try_from(index).ok()?;
    u16_at(table, at).map(u32::from)
}

/// Format 4: binary search for the first segment whose end is at or after `code`.
fn segment_delta(table: &[u8], code: u16) -> Option<u16> {
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

/// Formats 12 and 13: binary search over sorted groups. Format 12 groups map to
/// consecutive glyphs, format 13 groups to one.
fn groups(table: &[u8], code: u32, consecutive: bool) -> Option<u32> {
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
            return if consecutive {
                glyph.checked_add(code - start)
            } else {
                Some(glyph)
            };
        }
    }
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::{CharEncoding, Charmap, CmapFormat, Subtable, parse, text_subtable};
    use crate::GlyphId;
    use crate::name::Platform;
    use crate::sfnt::Table;
    use triergon_core::{Budget, Error, WorkBudget};

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

    fn charmaps(data: &[u8], glyphs: u32, work: u64) -> Vec<Charmap<'_>> {
        let budget = Budget::new(1 << 20).unwrap();
        let table = Table {
            offset: 0,
            len: data.len(),
        };
        let subtables = parse(data, table, &budget, &mut WorkBudget::new(100)).unwrap();
        subtables
            .as_slice()
            .iter()
            .map(|&s| Charmap::new(data, glyphs, s, work))
            .collect()
    }

    #[test]
    fn segment_delta_glyph_arrays_and_the_sentinel() {
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
        let [charmap] = charmaps(&data, 0x43, u64::MAX)[..] else {
            panic!("one charmap");
        };
        assert_eq!(charmap.platform(), Platform::Windows);
        assert_eq!(charmap.encoding(), CharEncoding::Unicode);
        assert_eq!(charmap.format(), CmapFormat::SegmentDelta);
        assert_eq!(charmap.glyph(0x41), Some(GlyphId(0x42)));
        assert_eq!(
            charmap.glyph(0x42),
            None,
            "glyph 0x43 is past the glyph count"
        );
        assert_eq!(charmap.glyph(0x61), Some(GlyphId(17)));
        assert_eq!(charmap.glyph(0x62), None);
        assert_eq!(charmap.glyph(0x1_0000), None);
        let pairs: Vec<_> = charmap.mappings().map(Result::unwrap).collect();
        assert_eq!(pairs, [(0x41, GlyphId(0x42)), (0x61, GlyphId(17))]);
        // Five codes in the first two segments, then the work runs out.
        let [limited] = charmaps(&data, 0x43, 4)[..] else {
            panic!()
        };
        let results: Vec<_> = limited.mappings().collect();
        assert_eq!(results.last(), Some(&Err(Error::Policy("font work"))));
    }

    #[test]
    fn coverage_and_many_to_one_groups() {
        let mut subtable = words(&[12, 0]);
        subtable.extend(40_u32.to_be_bytes());
        subtable.extend(0_u32.to_be_bytes());
        subtable.extend(2_u32.to_be_bytes());
        for value in [0x20_u32, 0x21, 5, 0x1_f600, 0x1_f600, 9] {
            subtable.extend(value.to_be_bytes());
        }
        let data = cmap(&subtable);
        let charmap = charmaps(&data, 100, u64::MAX)[0];
        assert_eq!(charmap.format(), CmapFormat::SegmentedCoverage);
        assert_eq!(charmap.glyph(0x21), Some(GlyphId(6)));
        assert_eq!(charmap.glyph(0x1_f600), Some(GlyphId(9)));
        assert_eq!(charmap.glyph(0x22), None);
        let mut many = data.clone();
        many[13] = 13;
        let charmap = charmaps(&many, 100, u64::MAX)[0];
        assert_eq!(
            (charmap.format(), charmap.glyph(0x21)),
            (CmapFormat::ManyToOne, Some(GlyphId(5)))
        );
        // A group count beyond the table drops the subtable.
        let mut short = data;
        short[12 + 15] = 3;
        assert!(charmaps(&short, 100, u64::MAX).is_empty());
    }

    #[test]
    fn trimmed_byte_and_unsupported_subtables() {
        let data = cmap(&words(&[6, 14, 0, 0x30, 2, 4, 5]));
        let charmap = charmaps(&data, 10, u64::MAX)[0];
        assert_eq!(
            [0x2f, 0x30, 0x31, 0x32].map(|c| charmap.glyph(c)),
            [None, Some(GlyphId(4)), Some(GlyphId(5)), None]
        );
        assert_eq!(charmap.mappings().count(), 2);
        let mut bytes = words(&[0, 262, 0]);
        bytes.extend((0..=255_u8).map(|c| c / 2));
        let byte_table = cmap(&bytes);
        let charmap = charmaps(&byte_table, 200, u64::MAX)[0];
        assert_eq!(
            (charmap.format(), charmap.glyph(0x41)),
            (CmapFormat::ByteEncoding, Some(GlyphId(0x20)))
        );
        assert!(charmaps(&cmap(&words(&[2, 0, 0])), 10, u64::MAX).is_empty());
        assert!(charmaps(&cmap(&words(&[0, 262, 0])), 10, u64::MAX).is_empty());
    }

    #[test]
    fn text_subtable_follows_libass_then_freetype() {
        use CmapFormat::{ByteEncoding, ManyToOne, SegmentDelta, SegmentedCoverage};
        let subtables = |records: &[(u16, u16, CmapFormat)]| -> Vec<Subtable> {
            records
                .iter()
                .enumerate()
                .map(|(offset, &(platform, encoding, format))| Subtable {
                    platform: Platform::from_id(platform),
                    encoding,
                    format,
                    offset,
                    end: 0,
                })
                .collect()
        };
        let chosen = |records: &[(u16, u16, CmapFormat)]| {
            text_subtable(&subtables(records)).map(|s| s.offset)
        };
        // (3, 10) anywhere, then the first (3, 1), then the first other Windows one.
        let windows = [
            (0, 3, SegmentDelta),
            (3, 0, SegmentDelta),
            (3, 1, SegmentDelta),
            (3, 1, SegmentDelta),
            (3, 10, SegmentedCoverage),
            (3, 10, SegmentedCoverage),
        ];
        assert_eq!(chosen(&windows), Some(4));
        assert_eq!(chosen(&windows[..4]), Some(2));
        assert_eq!(chosen(&windows[..2]), Some(1));
        assert_eq!(
            chosen(&[(1, 0, ByteEncoding), (3, 5, SegmentDelta)]),
            Some(1)
        );
        // Without Windows subtables, FreeType's default: the last full-Unicode
        // one ((0, 6) only in format 13), else the last Unicode or ISO one.
        let unicode = [
            (0, 4, SegmentedCoverage),
            (0, 3, SegmentDelta),
            (0, 6, SegmentedCoverage),
            (2, 1, SegmentDelta),
            (1, 0, ByteEncoding),
        ];
        assert_eq!(chosen(&unicode), Some(0));
        assert_eq!(chosen(&unicode[1..]), Some(2));
        assert_eq!(chosen(&unicode[1..3]), Some(1));
        assert_eq!(chosen(&[(0, 3, SegmentDelta), (0, 6, ManyToOne)]), Some(1));
        // Otherwise the first subtable.
        assert_eq!(
            chosen(&[(1, 0, ByteEncoding), (1, 1, SegmentDelta)]),
            Some(0)
        );
        assert_eq!(chosen(&[]), None);
    }
}
