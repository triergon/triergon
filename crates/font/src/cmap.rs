//! Character to glyph mapping: OpenType `cmap` formats 0, 2, 4, 6, 8, 10, 12 and
//! 13, the two charmaps FreeType adds to a CFF face, one built from its glyph
//! names and one from its encoding, and the variation sequences of formats 14
//! and 15.
//!
//! Subtable headers are checked when the face is parsed; lookups read the font bytes
//! directly, bounded by the end of the `cmap` table. A declared subtable length is
//! not trusted, because fonts with more than 64 KiB of format 4 data overflow it.

use crate::GlyphId;
use crate::cff::Encoding;
use crate::name::Platform;
use crate::postscript::UnicodeMap;
use crate::read::{Reader, bsearch, slice, u16_at, u24_at, u32_at};
use crate::sfnt::Table;
use crate::work::charge;
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// The last Unicode scalar value; mapping iteration stops there.
const MAX_CODE: u32 = 0x10_ffff;

/// A `cmap` subtable format FreeType makes a selectable charmap of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmapFormat {
    /// Format 0: one byte per code 0 to 255.
    ByteEncoding,
    /// Format 2: one- and two-byte codes through high-byte subheaders.
    HighByte,
    /// Format 4: segments of 16-bit codes with deltas or glyph arrays.
    SegmentDelta,
    /// Format 6: one run of 16-bit codes.
    Trimmed,
    /// Format 8: groups of mixed 16- and 32-bit codes.
    Mixed,
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
            2 => Self::HighByte,
            4 => Self::SegmentDelta,
            6 => Self::Trimmed,
            8 => Self::Mixed,
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
            Self::HighByte => 2,
            Self::SegmentDelta => 4,
            Self::Trimmed => 6,
            Self::Mixed => 8,
            Self::TrimmedArray => 10,
            Self::SegmentedCoverage => 12,
            Self::ManyToOne => 13,
        }
    }

    /// Whether FreeType's default validation accepts the subtable, given the
    /// bytes from its start to the end of the `cmap` table, with the flags it
    /// finds (format 4's unsorted or overlapping segments); `steps` counts
    /// the records read.
    fn check(self, table: &[u8], steps: &mut usize) -> Option<u8> {
        let short = |at| u16_at(table, at).map(usize::from);
        let long = |at| u32_at(table, at).and_then(|n| usize::try_from(n).ok());
        let limit = table.len();
        let fits = |ok: bool| ok.then_some(());
        match self {
            Self::SegmentDelta => return segment_delta_valid(table, steps),
            Self::ByteEncoding => {
                let length = short(2)?;
                fits(length <= limit && length >= 262)
            }
            Self::HighByte => high_byte_valid(table, steps),
            Self::Mixed => {
                fits(limit >= 16 + 8192)?;
                let length = long(4)?;
                fits(length <= limit && length >= 16 + 8192)?;
                let groups = long(8204)?;
                fits(groups <= (limit - 8208) / 12)?;
                ascending_groups(table, 8208, groups, steps)
            }
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
                ascending_groups(table, 16, groups, steps)
            }
        }
        .map(|()| 0)
    }
}

/// What a charmap reads codes through: a `cmap` subtable, or one of the
/// charmaps FreeType adds to a CFF face.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharmapKind {
    /// A `cmap` subtable.
    Subtable(CmapFormat),
    /// The Unicode charmap FreeType builds from the glyph names of a CFF face
    /// without a Unicode subtable (Windows platform, encoding 1).
    GlyphNames,
    /// The encoding of a name-keyed CFF face, as FreeType's Adobe charmap
    /// (platform 7, encoding 0 Standard, 1 Expert or 2 Custom).
    CffEncoding,
}

impl CharmapKind {
    /// The format `FT_Get_CMap_Format` reports: the subtable's, or `None`
    /// (-1) for the charmaps FreeType adds.
    pub const fn format_number(self) -> Option<u16> {
        match self {
            Self::Subtable(format) => Some(format.number()),
            Self::GlyphNames | Self::CffEncoding => None,
        }
    }
}

/// Groups of formats 8, 12 and 13 at `at` ascend without overlapping.
fn ascending_groups(table: &[u8], at: usize, groups: usize, steps: &mut usize) -> Option<()> {
    let mut last = None;
    for g in 0..groups {
        *steps += 1;
        let start = u32_at(table, at + 12 * g)?;
        let end = u32_at(table, at + 4 + 12 * g)?;
        (start <= end && last.is_none_or(|last| start > last)).then_some(())?;
        last = Some(end);
    }
    Some(())
}

/// `tt_cmap2_validate` at FreeType's default level: the keys and the
/// subheaders they reach fit the table, and each subheader's glyph array
/// lies within the subtable after the subheaders.
fn high_byte_valid(table: &[u8], steps: &mut usize) -> Option<()> {
    let fits = |ok: bool| ok.then_some(());
    fits(table.len() >= 4)?;
    let length = usize::from(u16_at(table, 2)?);
    fits(length <= table.len() && length >= 6 + 512)?;
    let subheaders = (0..256)
        .map(|n| u16_at(table, 6 + 2 * n).map(|key| usize::from(key >> 3)))
        .try_fold(0, |most, key| key.map(|key| most.max(key)))?;
    let glyph_ids = 518 + (subheaders + 1) * 8;
    fits(glyph_ids <= table.len())?;
    *steps += 256 + subheaders;
    for n in 0..=subheaders {
        let header = HighByteHeader::at(table, 518 + 8 * n);
        if header.count == 0 || header.offset == 0 {
            continue;
        }
        let ids = header.array;
        fits(ids >= glyph_ids && ids + 2 * header.count as usize <= length)?;
    }
    Some(())
}

/// `tt_cmap14_validate_internal` at FreeType's default level, for format 14
/// or, `extended`, its format 15 with three-byte glyphs: the selector records
/// ascend and their default and non-default tables fit and ascend below
/// U+110000.
fn variations_valid(table: &[u8], extended: bool, steps: &mut usize) -> Option<()> {
    let fits = |ok: bool| ok.then_some(());
    let long = |at: usize| u32_at(table, at).and_then(|n| usize::try_from(n).ok());
    let record = if extended { 6 } else { 5 };
    fits(table.len() >= 10)?;
    let (length, selectors) = (long(2)?, long(6)?);
    fits(length <= table.len() && length >= 10 && (length - 10) / 11 >= selectors)?;
    let mut last_selector = 1;
    for n in 0..selectors {
        *steps += 1;
        let at = 10 + 11 * n;
        let selector = u24_at(table, at)?;
        let (defaults, mappings) = (long(at + 3)?, long(at + 7)?);
        fits(defaults < length && mappings < length && selector >= last_selector)?;
        last_selector = selector + 1;
        if defaults != 0 {
            let ranges = long(defaults)?;
            fits(ranges <= table.len().checked_sub(defaults + 4)? / 4)?;
            let mut last = 0;
            for i in 0..ranges {
                *steps += 1;
                let at = defaults + 4 + 4 * i;
                let (base, count) = (u24_at(table, at)?, u32::from(*table.get(at + 3)?));
                fits(base + count < 0x11_0000 && base >= last)?;
                last = base + count + 1;
            }
        }
        if mappings != 0 {
            let count = long(mappings)?;
            fits(count <= table.len().checked_sub(mappings + 4)? / record)?;
            let mut last = 0;
            for i in 0..count {
                *steps += 1;
                let code = u24_at(table, mappings + 4 + record * i)?;
                fits(code < 0x11_0000 && code >= last)?;
                last = code + 1;
            }
        }
    }
    Some(())
}

/// `tt_cmap4_validate` at FreeType's default level: the segment arrays fit the
/// rest of the `cmap` table, no segment starts after it ends, and glyph-array
/// offsets stay within the table (FreeType lets a final 0xFFFF segment off).
fn segment_delta_valid(table: &[u8], steps: &mut usize) -> Option<u8> {
    let limit = table.len();
    let fits = |ok: bool| ok.then_some(());
    // FreeType measures the subtable to the end of `cmap`, whatever its length says.
    fits(limit >= 16)?;
    let segments = usize::from(u16_at(table, 6)? / 2);
    fits(limit >= 16 + 8 * segments)?;
    *steps += segments;
    let (starts, offsets) = (16 + 2 * segments, 16 + 6 * segments);
    let glyph_ids = offsets + 2 * segments;
    let (mut flags, mut last_start, mut last_end) = (0, 0, 0);
    for n in 0..segments {
        let end = usize::from(u16_at(table, 14 + 2 * n)?);
        let start = usize::from(u16_at(table, starts + 2 * n)?);
        let offset = usize::from(u16_at(table, offsets + 2 * n)?);
        fits(start <= end)?;
        // Overlapping segments pass at the default level, ordered or not.
        if n > 0 && start <= last_end {
            flags |= if last_start > start || last_end > end {
                UNSORTED
            } else {
                OVERLAPPING
            };
        }
        (last_start, last_end) = (start, end);
        let sentinel = n == segments - 1 && start == 0xffff && end == 0xffff;
        if offset == 0xffff {
            fits(sentinel)?;
        } else if offset != 0 && !sentinel {
            let array = offsets + 2 * n + offset;
            fits(array >= glyph_ids && array + 2 * (end - start + 1) <= limit)?;
        }
    }
    Some(flags)
}

/// What a platform and encoding number mean, for cmap subtables and name
/// records alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharEncoding {
    /// Unicode BMP: Windows encoding 1, or Unicode-platform encodings 0 to 3.
    Unicode,
    /// Full Unicode: Windows encoding 10, or Unicode-platform encodings 4 and 6.
    UnicodeFull,
    /// Unicode variation sequences (Unicode-platform encoding 5, cmap format
    /// 14, or FreeType's format 15).
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

/// One selectable subtable of a face; [`Charmap`] is its public view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Subtable {
    platform: Platform,
    encoding: u16,
    format: CmapFormat,
    /// What validation found: format 4's unsorted or overlapping segments.
    flags: u8,
    /// The subtable's offset in the file.
    offset: usize,
    /// The end of the `cmap` table in the file.
    end: usize,
}

/// A variation-sequence subtable FreeType keeps (format 14, or 15 with
/// three-byte glyphs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Variations {
    pub(crate) table: Table,
    pub(crate) extended: bool,
}

/// The subtables of a `cmap` table FreeType makes charmaps of.
#[derive(Debug)]
pub(crate) struct Subtables {
    /// The subtables that map codes, in directory order.
    pub(crate) list: BudgetVec<Subtable>,
    /// Whether a variation-sequence subtable FreeType lists (format 14 or
    /// 15, which cannot be selected) is a Unicode-platform or Windows (3, 1)
    /// charmap: it keeps FreeType from building one from glyph names.
    pub(crate) unicode_variations: bool,
    /// The first (0, 5) variation-sequence subtable FreeType keeps, which it
    /// reads variation sequences from.
    pub(crate) variation_sequences: Option<Variations>,
}

/// What validating one subtable gave: a selectable format, a variation-sequence
/// subtable, or nothing FreeType keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kept {
    /// A selectable format, with its validation flags.
    Format(CmapFormat, u8),
    Variations {
        extended: bool,
    },
    None,
}

/// Reads the encoding records, keeping the subtables FreeType keeps, in
/// directory order. As in FreeType, a table too short for its header has no
/// subtables, records stop at the table's end, and a record at offset 0 is
/// skipped. A subtable is validated once however many records point at it,
/// and the records it reads are charged to `work`.
pub(crate) fn parse(
    data: &[u8],
    table: Table,
    budget: &Budget,
    work: &mut WorkBudget,
) -> Result<Subtables> {
    let bytes = slice(data, table.offset, table.len)?;
    let mut subtables = Subtables {
        list: BudgetVec::new(budget),
        unicode_variations: false,
        variation_sequences: None,
    };
    // The version is ignored: the format allows newer subtables under 0.
    let Some(count) = u16_at(bytes, 2) else {
        return Ok(subtables);
    };
    let records = usize::from(count).min(bytes.len().saturating_sub(4) / 8);
    charge(work, records)?;
    let mut list = BudgetVec::new(budget);
    list.try_reserve_exact(records)?;
    let mut r = Reader::at(bytes, 4)?;
    for _ in 0..records {
        let platform = Platform::from_id(r.u16()?);
        let encoding = r.u16()?;
        let offset = usize::try_from(r.u32()?).map_err(|_| Error::Overflow)?;
        if offset != 0 {
            list.try_push((platform, encoding, offset))
                .map_err(|e| e.error)?;
        }
    }
    // Each distinct subtable, validated once.
    let mut validated: BudgetVec<(usize, Kept)> = BudgetVec::new(budget);
    validated.try_reserve_exact(list.len())?;
    for &(_, _, offset) in list.as_slice() {
        validated
            .try_push((offset, Kept::None))
            .map_err(|e| e.error)?;
    }
    let validated = validated.as_mut_slice();
    validated.sort_unstable_by_key(|&(offset, _)| offset);
    let mut last: Option<(usize, Kept)> = None;
    for (offset, kept) in validated.iter_mut() {
        if let Some((_, result)) = last.filter(|&(seen, _)| seen == *offset) {
            *kept = result;
            continue;
        }
        let mut steps = 0;
        let sub = bytes.get(*offset..).unwrap_or_default();
        *kept = match u16_at(sub, 0) {
            Some(n @ (14 | 15)) => variations_valid(sub, n == 15, &mut steps)
                .map_or(Kept::None, |()| Kept::Variations { extended: n == 15 }),
            number => number
                .and_then(CmapFormat::from_number)
                .and_then(|format| format.check(sub, &mut steps).map(|flags| (format, flags)))
                .map_or(Kept::None, |(format, flags)| Kept::Format(format, flags)),
        };
        last = Some((*offset, *kept));
        charge(work, steps)?;
    }
    let validated = &*validated;
    for &(platform, encoding, offset) in list.as_slice() {
        let at = validated.partition_point(|&(o, _)| o < offset);
        match validated[at].1 {
            Kept::Format(format, flags) => {
                let subtable = Subtable {
                    platform,
                    encoding,
                    format,
                    flags,
                    offset: table.offset + offset,
                    end: table.offset + table.len,
                };
                subtables.list.try_push(subtable).map_err(|e| e.error)?;
            }
            Kept::Variations { extended } => {
                subtables.unicode_variations |= platform == Platform::Unicode
                    || (platform == Platform::Windows && encoding == 1);
                if platform == Platform::Unicode && encoding == 5 {
                    subtables.variation_sequences.get_or_insert(Variations {
                        table: Table {
                            offset: table.offset + offset,
                            len: table.len - offset,
                        },
                        extended,
                    });
                }
            }
            Kept::None => {}
        }
    }
    Ok(subtables)
}

/// The charmap libass selects for text when it opens a face
/// (`ass_charmap_magic`): the first Windows (3, 10), else the first (3, 1),
/// else the first Windows charmap of another encoding, else FreeType's
/// default, else the first charmap. Its index in `charmaps`.
pub(crate) fn text_charmap<'f>(
    charmaps: impl Iterator<Item = Charmap<'f>> + Clone,
) -> Option<usize> {
    let windows = |encoding: Option<u16>| {
        charmaps.clone().position(|c| {
            c.platform() == Platform::Windows && encoding.is_none_or(|e| c.encoding_id() == e)
        })
    };
    windows(Some(10))
        .or_else(|| windows(Some(1)))
        .or_else(|| windows(None))
        .or_else(|| freetype_default(charmaps.clone()))
        .or_else(|| charmaps.clone().next().map(|_| 0))
}

/// The charmap FreeType selects when it opens a face (`find_unicode_charmap`):
/// the last full-Unicode charmap, else the last of the encodings FreeType
/// calls Unicode (every Unicode- and ISO-platform subtable, Windows 1 and 10,
/// and the charmap built from glyph names).
pub(crate) fn freetype_default<'f>(
    charmaps: impl Iterator<Item = Charmap<'f>> + Clone,
) -> Option<usize> {
    let full = |c: &Charmap<'_>| match (c.platform(), c.encoding_id()) {
        (Platform::Windows, 10) | (Platform::Unicode, 4) => true,
        (Platform::Unicode, 6) => c.kind() == CharmapKind::Subtable(CmapFormat::ManyToOne),
        _ => false,
    };
    let unicode = |c: &Charmap<'_>| match c.platform() {
        Platform::Unicode | Platform::Iso => true,
        Platform::Windows => matches!(c.encoding_id(), 1 | 10),
        _ => false,
    };
    // FreeType skips formats 14 and 15 there, which are not listed.
    let last = |test: &dyn Fn(&Charmap<'_>) -> bool| {
        charmaps
            .clone()
            .enumerate()
            .filter(|(_, c)| test(c))
            .last()
            .map(|(i, _)| i)
    };
    last(&full).or_else(|| last(&unicode))
}

impl Subtable {
    pub(crate) const fn platform(&self) -> Platform {
        self.platform
    }

    pub(crate) const fn encoding_id(&self) -> u16 {
        self.encoding
    }

    fn table<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        data.get(self.offset..self.end).unwrap_or_default()
    }

    /// The glyph `code` maps to, without the glyph-count check.
    fn lookup(&self, table: &[u8], code: u32, glyph_count: u32) -> Option<u32> {
        match self.format {
            CmapFormat::ByteEncoding => {
                let code = usize::try_from(code).ok().filter(|&c| c < 256)?;
                table.get(6 + code).map(|&g| u32::from(g))
            }
            CmapFormat::HighByte => Some(u32::from(high_byte(table, code))),
            CmapFormat::SegmentDelta => {
                Some(SegmentDelta::new(table, self.flags, glyph_count).char_index(code))
            }
            CmapFormat::Trimmed => {
                let first = u32::from(u16_at(table, 6)?);
                let count = u32::from(u16_at(table, 8)?);
                trimmed(table, 10, first, count, code)
            }
            CmapFormat::Mixed => Some(mixed(table, code)),
            CmapFormat::TrimmedArray => {
                trimmed(table, 20, u32_at(table, 12)?, u32_at(table, 16)?, code)
            }
            CmapFormat::SegmentedCoverage => groups(table, code, true),
            CmapFormat::ManyToOne => groups(table, code, false),
        }
    }

    /// `char_next` of formats 2 and 8: the first code after `code` that maps
    /// to a glyph other than zero, and that glyph; `None` past the last.
    fn next_mapping(
        &self,
        table: &[u8],
        code: u32,
        glyph_count: u32,
        state: &mut SegmentState,
        steps: &mut usize,
    ) -> Option<(u32, u32)> {
        match self.format {
            CmapFormat::HighByte => high_byte_next(table, code, steps),
            CmapFormat::SegmentDelta => {
                let (code, glyph) =
                    SegmentDelta::new(table, self.flags, glyph_count).char_next(state, code, steps);
                (glyph != 0).then_some((code, glyph))
            }
            CmapFormat::Mixed => mixed_next(table, code, glyph_count, steps),
            _ => None,
        }
    }

    /// Range `i` of codes the subtable can map, in subtable order; formats 2,
    /// 4 and 8 are stepped through instead.
    fn code_range(&self, table: &[u8], i: usize) -> Option<(u32, u32)> {
        match self.format {
            CmapFormat::ByteEncoding => (i == 0).then_some((0, 255)),
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
            CmapFormat::HighByte | CmapFormat::SegmentDelta | CmapFormat::Mixed => None,
        }
    }
}

/// Where a charmap's mappings come from.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Source<'f> {
    Table(Subtable),
    Names(&'f UnicodeMap),
    Encoding(&'f Encoding),
}

/// A charmap of one face, as FreeType lists it: a `cmap` subtable, or a charmap
/// FreeType adds to a CFF face ([`CharmapKind`]). It borrows the face, so it
/// cannot be used with another face.
#[derive(Clone, Copy, Debug)]
pub struct Charmap<'f> {
    data: &'f [u8],
    glyph_count: u32,
    source: Source<'f>,
    work: u64,
}

impl<'f> Charmap<'f> {
    pub(crate) const fn new(
        data: &'f [u8],
        glyph_count: u32,
        source: Source<'f>,
        work: u64,
    ) -> Self {
        Self {
            data,
            glyph_count,
            source,
            work,
        }
    }

    pub const fn platform(&self) -> Platform {
        match self.source {
            Source::Table(subtable) => subtable.platform,
            Source::Names(_) => Platform::Windows,
            Source::Encoding(_) => Platform::Other(7),
        }
    }

    /// The encoding record's platform-specific encoding number.
    pub const fn encoding_id(&self) -> u16 {
        match self.source {
            Source::Table(subtable) => subtable.encoding,
            Source::Names(_) => 1,
            Source::Encoding(encoding) => encoding.id,
        }
    }

    pub const fn encoding(&self) -> CharEncoding {
        CharEncoding::of(self.platform(), self.encoding_id())
    }

    pub const fn kind(&self) -> CharmapKind {
        match self.source {
            Source::Table(subtable) => CharmapKind::Subtable(subtable.format),
            Source::Names(_) => CharmapKind::GlyphNames,
            Source::Encoding(_) => CharmapKind::CffEncoding,
        }
    }

    /// The glyph `code` maps to, looked up unchanged. Unmapped codes and glyphs at or
    /// above the face's glyph count give `None`.
    pub fn glyph(&self, code: u32) -> Option<GlyphId> {
        let glyph = match self.source {
            Source::Table(subtable) => {
                subtable.lookup(subtable.table(self.data), code, self.glyph_count)?
            }
            Source::Names(map) => u32::from(map.char_index(code)),
            Source::Encoding(encoding) => {
                u32::from(*encoding.glyphs.get(usize::try_from(code).ok()?)?)
            }
        };
        self.valid(glyph)
    }

    /// `glyph` unless it is 0 or at or above the glyph count, which FreeType
    /// reads as no glyph.
    fn valid(&self, glyph: u32) -> Option<GlyphId> {
        let glyph = u16::try_from(glyph)
            .ok()
            .filter(|&g| g != 0 && u32::from(g) < self.glyph_count)?;
        Some(GlyphId(glyph))
    }

    /// `char_next`: the first code after `code` the charmap gives a glyph
    /// other than zero, perhaps one past the glyph count, and that glyph.
    /// `steps` counts the records and codes it passes over.
    fn next_mapping(
        &self,
        code: u32,
        state: &mut SegmentState,
        steps: &mut usize,
    ) -> Option<(u32, u32)> {
        match self.source {
            Source::Table(subtable) => subtable.next_mapping(
                subtable.table(self.data),
                code,
                self.glyph_count,
                state,
                steps,
            ),
            Source::Names(map) => match map.char_next(code) {
                (_, 0) => None,
                (code, glyph) => Some((code, u32::from(glyph))),
            },
            Source::Encoding(encoding) => {
                let start = usize::try_from(code).ok()?.checked_add(1)?;
                let found = encoding
                    .glyphs
                    .iter()
                    .enumerate()
                    .skip(start)
                    .find(|&(_, &glyph)| glyph != 0);
                *steps += found
                    .map_or(encoding.glyphs.len(), |(at, _)| at + 1)
                    .saturating_sub(start);
                let (code, &glyph) = found?;
                Some((u32::try_from(code).ok()?, u32::from(glyph)))
            }
        }
    }

    /// Whether the mappings are read as FreeType steps through them, with
    /// `char_next`, rather than by the subtable's ranges.
    const fn steps(&self) -> bool {
        !matches!(
            self.source,
            Source::Table(Subtable {
                format: CmapFormat::ByteEncoding
                    | CmapFormat::Trimmed
                    | CmapFormat::TrimmedArray
                    | CmapFormat::SegmentedCoverage
                    | CmapFormat::ManyToOne,
                ..
            })
        )
    }

    /// Every (code, glyph) pair without glyph 0, in the order FreeType's
    /// `FT_Get_First_Char` and `FT_Get_Next_Char` give them for formats 2 and
    /// 8 and the charmaps it adds, and by segment or group in subtable order
    /// and by code within each for the other formats; codes above U+10FFFF are
    /// not visited there. Each code visited or step taken costs one unit of
    /// the face's work limit; when it runs out the iterator yields
    /// `Error::Policy` once and ends.
    pub fn mappings(&self) -> Mappings<'f> {
        Mappings {
            charmap: *self,
            range: 0,
            next: None,
            stepped: None,
            segments: SegmentState::default(),
            work: WorkBudget::new(self.work),
            done: false,
        }
    }
}

/// A charmap read as libass 0.17.5 reads text through it: a character is
/// looked up at the code `ass_font_index_magic` gives it, as
/// `FT_Get_Char_Index` looks codes up. [`Font::text_lookup`](crate::Font::text_lookup)
/// makes one.
#[derive(Clone, Copy, Debug)]
pub struct TextLookup<'f> {
    charmap: Charmap<'f>,
    /// The bits set in every code looked up: `0xF000` in a Windows Symbol
    /// charmap, none in others.
    prefix: u32,
}

impl<'f> TextLookup<'f> {
    pub(crate) const fn new(charmap: Charmap<'f>, symbol: bool) -> Self {
        let prefix = if symbol { 0xf000 } else { 0 };
        Self { charmap, prefix }
    }

    pub const fn charmap(&self) -> Charmap<'f> {
        self.charmap
    }

    /// The code `c` is looked up at: `0xF000 | c` in a Windows Symbol charmap,
    /// `c` in any other; `None` for 0, which libass never looks up.
    pub const fn code(&self, c: u32) -> Option<u32> {
        let code = c | self.prefix;
        if code == 0 { None } else { Some(code) }
    }

    /// The glyph `c` maps to.
    pub fn glyph(&self, c: u32) -> Option<GlyphId> {
        self.charmap.glyph(self.code(c)?)
    }
}

/// The iterator of [`Charmap::mappings`].
#[derive(Debug)]
pub struct Mappings<'f> {
    charmap: Charmap<'f>,
    range: usize,
    /// The next code and the last code of the current range.
    next: Option<(u32, u32)>,
    /// Stepping through the charmap: the last code given.
    stepped: Option<u32>,
    /// Where format 4's `char_next` continues from.
    segments: SegmentState,
    work: WorkBudget,
    done: bool,
}

impl Mappings<'_> {
    /// `FT_Get_First_Char` and `FT_Get_Next_Char`: code 0 when it maps, then
    /// each next code whose glyph is below the glyph count.
    fn step(&mut self) -> Option<Result<(u32, GlyphId)>> {
        let charmap = self.charmap;
        let mut code = match self.stepped {
            None => {
                self.stepped = Some(0);
                if let Some(glyph) = charmap.glyph(0) {
                    return Some(Ok((0, glyph)));
                }
                0
            }
            Some(code) => code,
        };
        loop {
            let mut steps = 1;
            let next = charmap.next_mapping(code, &mut self.segments, &mut steps);
            if !self.work.try_charge(steps as u64) {
                self.done = true;
                return Some(Err(Error::Policy("font work")));
            }
            let Some((next, glyph)) = next else {
                self.done = true;
                return None;
            };
            code = next;
            if let Some(glyph) = charmap.valid(glyph) {
                self.stepped = Some(code);
                return Some(Ok((code, glyph)));
            }
        }
    }
}

impl Iterator for Mappings<'_> {
    type Item = Result<(u32, GlyphId)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.charmap.steps() {
            return if self.done { None } else { self.step() };
        }
        let Source::Table(subtable) = self.charmap.source else {
            return None;
        };
        let table = subtable.table(self.charmap.data);
        while !self.done {
            let Some((code, last)) = self.next else {
                let Some((first, last)) = subtable.code_range(table, self.range) else {
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

/// How a Unicode variation sequence maps, in a `cmap` format 14 or 15 subtable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// To the base character's glyph in the face's Unicode charmap.
    Default,
    /// To this glyph.
    Glyph(GlyphId),
    /// Not at all.
    None,
}

/// A `cmap` variation-sequence subtable: format 14, or FreeType's format 15,
/// whose mappings name three-byte glyphs.
/// [`Font::variation_sequences`](crate::Font::variation_sequences) gives
/// FreeType's; [`Self::format14`] reads one another reader selects.
#[derive(Clone, Copy, Debug)]
pub struct VariationSequences<'f> {
    table: &'f [u8],
    extended: bool,
}

impl<'f> VariationSequences<'f> {
    pub(crate) const fn new(table: &'f [u8], extended: bool) -> Self {
        Self { table, extended }
    }

    /// The sequences of the format 14 subtable starting `table`, read to the
    /// end of the `cmap` table, for a reader that selects the subtable as
    /// HarfBuzz does.
    pub const fn format14(table: &'f [u8]) -> Self {
        Self::new(table, false)
    }

    /// How `c` followed by `selector` maps, as FreeType's
    /// `tt_cmap14_char_var_index` and HarfBuzz's `get_glyph_variant` read it:
    /// the selector's record, then its default ranges, then its mappings,
    /// where glyph 0 is none, each found with HarfBuzz's binary search, which
    /// FreeType's agrees with on the ascending tables its validation keeps.
    /// An offset of zero is no table, as is one whose entries run past the
    /// end of the `cmap` table (neither FreeType's validation nor HarfBuzz's
    /// sanitizer keeps such a subtable), and a glyph beyond 65535 is none.
    pub fn variant(&self, c: u32, selector: u32) -> Variant {
        let table = self.table;
        let u24 = |t: &[u8], at: usize| u24_at(t, at).unwrap_or(0);
        let long = |t: &[u8], at: usize| u32_at(t, at).map_or(0, |n| n as usize);
        // Entries of `size` bytes after a 32-bit count at `at`, if they fit.
        let entries = |t: &[u8], at: usize, size: usize| {
            let count = long(t, at);
            (count <= t.len().saturating_sub(at + 4) / size).then_some(count)
        };
        let records = entries(table, 6, 11).unwrap_or(0);
        let Some(record) = bsearch(records, |i| u24(table, 10 + 11 * i).cmp(&selector)) else {
            return Variant::None;
        };
        let at = 10 + 11 * record;
        let part = |offset: usize| (offset != 0).then(|| table.get(offset..)).flatten();
        if let Some(defaults) = part(long(table, at + 3)) {
            let count = entries(defaults, 0, 4).unwrap_or(0);
            let found = bsearch(count, |i| {
                let first = u24(defaults, 4 + 4 * i);
                let last = first + u32::from(defaults.get(4 + 4 * i + 3).copied().unwrap_or(0));
                if c < first {
                    std::cmp::Ordering::Greater
                } else if c > last {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            });
            if found.is_some() {
                return Variant::Default;
            }
        }
        let Some(mappings) = part(long(table, at + 7)) else {
            return Variant::None;
        };
        let record = if self.extended { 6 } else { 5 };
        let count = entries(mappings, 0, record).unwrap_or(0);
        let glyph = bsearch(count, |i| u24(mappings, 4 + record * i).cmp(&c)).map(|i| {
            let at = 4 + record * i + 3;
            if self.extended {
                u24(mappings, at)
            } else {
                u32::from(u16_at(mappings, at).unwrap_or(0))
            }
        });
        match glyph.map(u16::try_from) {
            Some(Ok(glyph)) if glyph != 0 => Variant::Glyph(GlyphId(glyph)),
            _ => Variant::None,
        }
    }
}

/// Formats 6 and 10: `count` glyph words for codes from `first`, at `array`.
fn trimmed(table: &[u8], array: usize, first: u32, count: u32, code: u32) -> Option<u32> {
    let index = code.checked_sub(first).filter(|&i| i < count)?;
    let at = array + 2 * usize::try_from(index).ok()?;
    u16_at(table, at).map(u32::from)
}

/// `TT_CMAP_FLAG_UNSORTED`: format 4 segments whose starts or ends descend.
const UNSORTED: u8 = 1;
/// `TT_CMAP_FLAG_OVERLAPPING`: format 4 segments that overlap in order.
const OVERLAPPING: u8 = 2;

/// Where FreeType's format 4 `char_next` continues from (`TT_CMap4Rec`): the
/// last code it gave and the segment it is in.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SegmentState {
    charcode: u32,
    gindex: u32,
    range: usize,
    start: u32,
    end: u32,
    delta: i32,
    /// Where the segment's glyph array starts in the subtable, if it has one.
    values: Option<usize>,
}

impl Default for SegmentState {
    fn default() -> Self {
        Self {
            charcode: u32::MAX,
            gindex: 0,
            range: 0,
            start: 0,
            end: 0,
            delta: 0,
            values: None,
        }
    }
}

/// A format 4 subtable as FreeType reads it (`tt_cmap4_char_map_linear`,
/// `tt_cmap4_char_map_binary`, `tt_cmap4_set_range` and `tt_cmap4_next`): a
/// linear search through segments its validation found unsorted, else a
/// binary search, which for overlapping segments takes the first that holds
/// the code and has glyphs. The arrays are read to the end of the `cmap`
/// table, which bounds glyph arrays; reads past it give zero.
#[derive(Clone, Copy, Debug)]
struct SegmentDelta<'a> {
    table: &'a [u8],
    segments: usize,
    flags: u8,
    glyph_count: u32,
}

impl<'a> SegmentDelta<'a> {
    fn new(table: &'a [u8], flags: u8, glyph_count: u32) -> Self {
        let segments = usize::from(u16_at(table, 6).unwrap_or(0) >> 1);
        Self {
            table,
            segments,
            flags,
            glyph_count,
        }
    }

    fn word(&self, at: usize) -> u32 {
        u16_at(self.table, at).map_or(0, u32::from)
    }

    fn short(&self, at: usize) -> i32 {
        i32::from(self.word(at) as u16 as i16)
    }

    /// FreeType's fix for a last segment 0xFFFF..0xFFFF whose glyph array
    /// lies past the table: delta 1 and no array. `at` is its offset word.
    fn last_fix(
        &self,
        i: usize,
        start: u32,
        end: u32,
        at: usize,
        delta: &mut i32,
        offset: &mut u32,
    ) {
        if i + 1 >= self.segments
            && start == 0xffff
            && end == 0xffff
            && *offset != 0
            && at + *offset as usize + 2 > self.table.len()
        {
            *delta = 1;
            *offset = 0;
        }
    }

    /// A glyph array entry adjusted by `delta`: zero for an empty entry or
    /// one past the glyph count.
    fn array_glyph(&self, at: usize, delta: i32) -> u32 {
        match self.word(at) {
            0 => 0,
            g => match (g as i32).wrapping_add(delta) as u32 & 0xffff {
                g if g >= self.glyph_count => 0,
                g => g,
            },
        }
    }

    /// `tt_cmap4_char_index`.
    fn char_index(&self, code: u32) -> u32 {
        if code >= 0x1_0000 {
            0
        } else if self.flags & UNSORTED != 0 {
            self.linear(code, false, &mut 0).1
        } else {
            self.binary(code, false, &mut 0).1
        }
    }

    /// `tt_cmap4_char_next`: the code after `code` that maps, and its glyph
    /// (zero at the end).
    /// `steps` counts the segments and codes it passes over.
    fn char_next(&self, state: &mut SegmentState, code: u32, steps: &mut usize) -> (u32, u32) {
        if code >= 0xffff {
            return (code, 0);
        }
        if self.flags & UNSORTED != 0 {
            return self.linear(code, true, steps);
        }
        if code == state.charcode {
            self.next(state, steps);
            let gindex = state.gindex;
            return (if gindex != 0 { state.charcode } else { code }, gindex);
        }
        let (charcode, gindex, mid, end) = self.binary(code, true, steps);
        if self.segments == 0 {
            return (code, 0);
        }
        // Where the search stopped is the nearest segment to the code.
        let mid = if charcode > end { mid + 1 } else { mid };
        if mid == self.segments && charcode > end {
            return (code, 0);
        }
        if !self.set_range(state, mid, steps) {
            return (if gindex != 0 { charcode } else { code }, gindex);
        }
        state.charcode = charcode;
        let gindex = if gindex != 0 {
            state.gindex = gindex;
            gindex
        } else {
            self.next(state, steps);
            state.gindex
        };
        (if gindex != 0 { state.charcode } else { code }, gindex)
    }

    /// `tt_cmap4_char_map_linear`.
    fn linear(&self, code: u32, next: bool, steps: &mut usize) -> (u32, u32) {
        let n = self.segments;
        if n == 0 {
            return (code, 0);
        }
        let mut charcode = code + u32::from(next);
        let mut gindex = 0;
        'segments: for i in 0..n {
            *steps += 1;
            let end = self.word(14 + 2 * i);
            let start = self.word(16 + 2 * n + 2 * i);
            if charcode < start {
                if next {
                    charcode = start;
                } else {
                    break;
                }
            }
            while charcode <= end {
                *steps += 1;
                let at = 16 + 6 * n + 2 * i;
                let mut delta = self.short(16 + 4 * n + 2 * i);
                let mut offset = self.word(at);
                self.last_fix(i, start, end, at, &mut delta, &mut offset);
                if offset == 0xffff {
                    continue 'segments;
                }
                if offset != 0 {
                    let r = at + offset as usize + 2 * (charcode - start) as usize;
                    if next && r > self.table.len() {
                        continue 'segments;
                    }
                    gindex = self.array_glyph(r, delta);
                } else {
                    gindex = (charcode as i32).wrapping_add(delta) as u32 & 0xffff;
                    if next && gindex >= self.glyph_count {
                        gindex = 0;
                        let (code, last) = (charcode as i32 + delta, end as i32 + delta);
                        if code < 0 && last >= 0 {
                            charcode = (-delta) as u32;
                        } else if code < 0x1_0000 && last >= 0x1_0000 {
                            charcode = (0x1_0000 - delta) as u32;
                        } else {
                            continue 'segments;
                        }
                    }
                }
                if next && gindex == 0 {
                    if charcode >= 0xffff {
                        break 'segments;
                    }
                    charcode += 1;
                    continue;
                }
                break 'segments;
            }
        }
        (if next { charcode } else { code }, gindex)
    }

    /// `tt_cmap4_char_map_binary` up to its `next` tail: the code, its glyph,
    /// and the segment and end the search stopped at.
    fn binary(&self, code: u32, next: bool, steps: &mut usize) -> (u32, u32, usize, u32) {
        let n = self.segments;
        if n == 0 {
            return (code, 0, 0, 0);
        }
        let n2 = 2 * n;
        let mut charcode = code + u32::from(next);
        let mut gindex = 0;
        let (mut min, mut max) = (0, n);
        let (mut mid, mut end);
        // Reads segment `i` as FreeType's pointer walks it, leaving `p` at its
        // offset word.
        let read = |i: usize, p: &mut usize| {
            *p = 14 + 2 * i;
            let end = self.word(*p);
            *p += 2 + n2;
            let start = self.word(*p);
            *p += n2;
            let delta = self.short(*p);
            *p += n2;
            (end, start, delta, self.word(*p))
        };
        loop {
            *steps += 1;
            mid = (min + max) >> 1;
            let mut p = 14 + 2 * mid;
            end = self.word(p);
            p += 2 + n2;
            let mut start = self.word(p);
            if charcode < start {
                max = mid;
            } else if charcode > end {
                min = mid + 1;
            } else {
                p += n2;
                let mut delta = self.short(p);
                p += n2;
                let mut offset = self.word(p);
                self.last_fix(mid, start, end, p, &mut delta, &mut offset);
                if self.flags & OVERLAPPING != 0 {
                    // The first segment holding the code, whose glyphs are
                    // not missing.
                    max = mid;
                    if offset == 0xffff {
                        mid = max + 1;
                    }
                    let mut i = max;
                    while i > 0 {
                        *steps += 1;
                        let old = p;
                        let mut q = 14 + 2 * (i - 1);
                        let prev_end = self.word(q);
                        if charcode > prev_end {
                            p = old;
                            break;
                        }
                        (end, start, delta, offset) = read(i - 1, &mut q);
                        p = q;
                        if offset != 0xffff {
                            mid = i - 1;
                        }
                        i -= 1;
                    }
                    if mid == max + 1 {
                        if i != max {
                            (end, start, delta, offset) = read(max, &mut p);
                        }
                        mid = max;
                        i = max + 1;
                        while i < n {
                            *steps += 1;
                            p = 14 + 2 * i;
                            let next_end = self.word(p);
                            p += 2 + n2;
                            let next_start = self.word(p);
                            if charcode < next_start {
                                break;
                            }
                            (end, start) = (next_end, next_start);
                            p += n2;
                            delta = self.short(p);
                            p += n2;
                            offset = self.word(p);
                            if offset != 0xffff {
                                mid = i;
                            }
                            i += 1;
                        }
                        i -= 1;
                        if mid == max {
                            mid = i;
                            break;
                        }
                    }
                    if mid != i {
                        (end, start, delta, offset) = read(mid, &mut p);
                    }
                } else if offset == 0xffff {
                    break;
                }
                if offset != 0 {
                    let r = p + offset as usize + 2 * charcode.wrapping_sub(start) as usize;
                    if next && r > self.table.len() {
                        break;
                    }
                    gindex = self.array_glyph(r, delta);
                } else {
                    gindex = (charcode as i32).wrapping_add(delta) as u32 & 0xffff;
                    if next && gindex >= self.glyph_count {
                        gindex = 0;
                        let (code, last) = (charcode as i32 + delta, end as i32 + delta);
                        if code < 0 && last >= 0 {
                            charcode = (-delta) as u32;
                        } else if code < 0x1_0000 && last >= 0x1_0000 {
                            charcode = (0x1_0000 - delta) as u32;
                        }
                    }
                }
                break;
            }
            if min >= max {
                break;
            }
        }
        (charcode, gindex, mid, end)
    }

    /// `tt_cmap4_set_range`: the first segment from `range` whose glyphs are
    /// not missing, or `false` past the last.
    fn set_range(&self, state: &mut SegmentState, mut range: usize, steps: &mut usize) -> bool {
        let n = self.segments;
        while range < n {
            *steps += 1;
            let mut p = 14 + 2 * range;
            state.end = self.word(p);
            p += 2 + 2 * n;
            state.start = self.word(p);
            p += 2 * n;
            state.delta = self.short(p);
            p += 2 * n;
            let mut offset = self.word(p);
            let (start, end) = (state.start, state.end);
            self.last_fix(range, start, end, p, &mut state.delta, &mut offset);
            if offset != 0xffff {
                state.values = (offset != 0).then_some(p + offset as usize);
                state.range = range;
                return true;
            }
            range += 1;
        }
        false
    }

    /// `tt_cmap4_next`: the code after the state's that maps, from its
    /// segment on.
    fn next(&self, state: &mut SegmentState, steps: &mut usize) {
        let mut charcode = state.charcode.wrapping_add(1);
        if charcode < state.start {
            charcode = state.start;
        }
        loop {
            let (end, delta) = (state.end, state.delta);
            'segment: {
                if charcode > end {
                    break 'segment;
                }
                if let Some(values) = state.values {
                    let mut p = values + 2 * (charcode - state.start) as usize;
                    if p > self.table.len() {
                        break 'segment;
                    }
                    loop {
                        *steps += 1;
                        let g = self.word(p);
                        p += 2;
                        if g != 0 {
                            let g = (g as i32).wrapping_add(delta) as u32 & 0xffff;
                            if g != 0 {
                                (state.charcode, state.gindex) = (charcode, g);
                                return;
                            }
                        }
                        charcode += 1;
                        if charcode > end {
                            break;
                        }
                    }
                } else {
                    loop {
                        *steps += 1;
                        let mut g = (charcode as i32).wrapping_add(delta) as u32 & 0xffff;
                        if g >= self.glyph_count {
                            g = 0;
                            let (code, last) = (charcode as i32 + delta, end as i32 + delta);
                            if code < 0 && last >= 0 {
                                charcode = (-delta) as u32;
                            } else if code < 0x1_0000 && last >= 0x1_0000 {
                                charcode = (0x1_0000 - delta) as u32;
                            } else {
                                break 'segment;
                            }
                        }
                        if g != 0 {
                            (state.charcode, state.gindex) = (charcode, g);
                            return;
                        }
                        charcode += 1;
                        if charcode > end {
                            break;
                        }
                    }
                }
            }
            if !self.set_range(state, state.range + 1, steps) {
                break;
            }
            if charcode < state.start {
                charcode = state.start;
            }
        }
        (state.charcode, state.gindex) = (u32::MAX, 0);
    }
}

/// Format 2: the subheader of `code` (`tt_cmap2_get_subheader`): the first
/// one for a single byte whose key is zero, else the one its high byte's key
/// names, but not the first; `None` for codes above 0xFFFF.
fn high_byte_subheader(table: &[u8], code: u32) -> Option<usize> {
    if code >= 0x1_0000 {
        return None;
    }
    let (low, high) = ((code & 0xff) as usize, (code >> 8) as usize);
    let key = |byte: usize| usize::from(u16_at(table, 6 + 2 * byte).unwrap_or(0));
    if high == 0 {
        (key(low) == 0).then_some(518)
    } else {
        let sub = 518 + (key(high) & !7);
        (sub != 518).then_some(sub)
    }
}

/// A format 2 subheader.
#[derive(Clone, Copy, Debug)]
struct HighByteHeader {
    /// The first low byte it maps.
    first: u32,
    count: u32,
    delta: i32,
    /// `idRangeOffset`: zero for no glyph array.
    offset: usize,
    /// Where its glyph array starts in the subtable: the offset counts from
    /// the `idRangeOffset` word itself.
    array: usize,
}

impl HighByteHeader {
    fn at(table: &[u8], sub: usize) -> Self {
        let word = |at: usize| u16_at(table, sub + at).unwrap_or(0);
        let offset = usize::from(word(6));
        Self {
            first: u32::from(word(0)),
            count: u32::from(word(2)),
            delta: i32::from(word(4).cast_signed()),
            offset,
            array: sub + 6 + offset,
        }
    }

    /// The glyph at `position` of its array, the delta added, zero for none.
    fn glyph(&self, table: &[u8], position: u32) -> u16 {
        match u16_at(table, self.array + 2 * position as usize).unwrap_or(0) {
            0 => 0,
            glyph => (i32::from(glyph) + self.delta) as u16,
        }
    }
}

/// Format 2 (`tt_cmap2_char_index`).
fn high_byte(table: &[u8], code: u32) -> u16 {
    let Some(sub) = high_byte_subheader(table, code) else {
        return 0;
    };
    let header = HighByteHeader::at(table, sub);
    let index = (code & 0xff).wrapping_sub(header.first);
    if index >= header.count || header.offset == 0 {
        return 0;
    }
    header.glyph(table, index)
}

/// Format 2 (`tt_cmap2_char_next`), with its walk through the subheaders.
fn high_byte_next(table: &[u8], code: u32, steps: &mut usize) -> Option<(u32, u32)> {
    let mut code = code.checked_add(1)?;
    while code < 0x1_0000 {
        *steps += 1;
        if let Some(sub) = high_byte_subheader(table, code) {
            let header = HighByteHeader::at(table, sub);
            let (start, count) = (header.first, header.count);
            let low = code & 0xff;
            if low >= start + count && code <= 0xff {
                code = 0x100;
                continue;
            }
            if header.offset == 0 {
                // Only a malformed table has no glyphs for its first two-byte code.
                if code == 0x100 {
                    return None;
                }
            } else {
                let (low, mut position) = if low < start {
                    (start, 0)
                } else {
                    (low, low - start)
                };
                code = (code & !0xff) + low;
                while position < count {
                    *steps += 1;
                    let glyph = header.glyph(table, position);
                    if glyph != 0 {
                        return Some((code, u32::from(glyph)));
                    }
                    position += 1;
                    code += 1;
                }
                if count != 0 {
                    code -= 1;
                }
            }
        }
        code = if code <= 0xff {
            code + 1
        } else {
            (code & !0xff) + 0x100
        };
    }
    None
}

/// The groups of a format 8 subtable: start, end and first glyph.
fn mixed_groups(table: &[u8]) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
    let count = u32_at(table, 8204).unwrap_or(0) as usize;
    (0..count).map_while(move |g| {
        let at = 8208 + 12 * g;
        Some((
            u32_at(table, at)?,
            u32_at(table, at + 4)?,
            u32_at(table, at + 8)?,
        ))
    })
}

/// Format 8 (`tt_cmap8_char_index`): a linear search of the groups.
fn mixed(table: &[u8], code: u32) -> u32 {
    for (start, end, first) in mixed_groups(table) {
        if code < start {
            break;
        }
        if code <= end {
            return first.checked_add(code - start).unwrap_or(0);
        }
    }
    0
}

/// Format 8 (`tt_cmap8_char_next`): a group whose glyphs would overflow, or
/// whose next glyph is at or above the glyph count, is passed over for the
/// next one from the same code.
fn mixed_next(table: &[u8], code: u32, glyph_count: u32, steps: &mut usize) -> Option<(u32, u32)> {
    let mut code = code.checked_add(1)?;
    for (start, end, first) in mixed_groups(table) {
        *steps += 1;
        code = code.max(start);
        while code <= end {
            *steps += 1;
            let Some(glyph) = first.checked_add(code - start) else {
                break;
            };
            if glyph == 0 {
                code = code.checked_add(1)?;
                continue;
            }
            if glyph >= glyph_count {
                break;
            }
            return Some((code, glyph));
        }
    }
    None
}

/// Formats 12 and 13: binary search over sorted groups. Format 12 groups map to
/// consecutive glyphs, format 13 groups to one.
fn groups(table: &[u8], code: u32, consecutive: bool) -> Option<u32> {
    let count = usize::try_from(u32_at(table, 12)?).ok()?;
    let word = |at: usize| u32_at(table, at).unwrap_or(0);
    let found = bsearch(count, |g| {
        let (start, end) = (word(16 + 12 * g), word(20 + 12 * g));
        if code < start {
            std::cmp::Ordering::Greater
        } else if code > end {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    });
    let Some(g) = found else {
        return Some(0);
    };
    let (start, glyph) = (word(16 + 12 * g), word(24 + 12 * g));
    if consecutive {
        glyph.checked_add(code - start)
    } else {
        Some(glyph)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CharEncoding, Charmap, CharmapKind, CmapFormat, Source, Subtable, parse, text_charmap,
    };
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
            .list
            .as_slice()
            .iter()
            .map(|&s| Charmap::new(data, glyphs, Source::Table(s), work))
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
        assert_eq!(
            charmap.kind(),
            CharmapKind::Subtable(CmapFormat::SegmentDelta)
        );
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
        // The work runs out within the first `char_next`.
        let [limited] = charmaps(&data, 0x43, 1)[..] else {
            panic!()
        };
        let results: Vec<_> = limited.mappings().collect();
        assert_eq!(results.last(), Some(&Err(Error::Policy("font work"))));
    }

    /// A format 4 subtable of (start, end, delta) segments without glyph
    /// arrays.
    fn segments(segments: &[(u16, u16, u16)]) -> Vec<u8> {
        let n = segments.len() as u16;
        let mut subtable = words(&[4, 0, 0, 2 * n, 0, 0, 0]);
        subtable.extend(words(&segments.iter().map(|s| s.1).collect::<Vec<_>>()));
        subtable.extend(words(&[0]));
        subtable.extend(words(&segments.iter().map(|s| s.0).collect::<Vec<_>>()));
        subtable.extend(words(&segments.iter().map(|s| s.2).collect::<Vec<_>>()));
        subtable.extend(words(&vec![0; segments.len()]));
        subtable
    }

    #[test]
    fn format_4_searches_unsorted_segments_linearly_and_overlapping_ones_first() {
        // Unsorted: FreeType's linear search stops at the first segment that
        // starts past the code, so the later 'A'..='E' never maps it.
        let data = cmap(&segments(&[
            (0x50, 0x60, 1),
            (0x41, 0x45, 2),
            (0xffff, 0xffff, 1),
        ]));
        let [unsorted] = charmaps(&data, 0x100, u64::MAX)[..] else {
            panic!("one charmap");
        };
        assert_eq!(unsorted.glyph(0x42), None);
        assert_eq!(unsorted.glyph(0x55), Some(GlyphId(0x56)));
        let pairs: Vec<_> = unsorted.mappings().map(Result::unwrap).collect();
        assert_eq!(pairs.first(), Some(&(0x50, GlyphId(0x51))));
        assert!(pairs.iter().all(|&(code, _)| code >= 0x50), "{pairs:?}");
        // Overlapping in order: the first segment holding the code maps it.
        let data = cmap(&segments(&[
            (0x41, 0x48, 1),
            (0x45, 0x50, 100),
            (0xffff, 0xffff, 1),
        ]));
        let [overlapping] = charmaps(&data, 0x100, u64::MAX)[..] else {
            panic!("one charmap");
        };
        assert_eq!(overlapping.glyph(0x46), Some(GlyphId(0x47)));
        assert_eq!(overlapping.glyph(0x4a), Some(GlyphId(0x4a + 100)));
        // Stepping goes on through the next segment from where the first ends.
        let pairs: Vec<_> = overlapping.mappings().map(Result::unwrap).collect();
        let codes: Vec<u32> = pairs.iter().map(|&(code, _)| code).collect();
        assert_eq!(codes, (0x41..=0x50).collect::<Vec<_>>());
        assert_eq!(pairs[8], (0x49, GlyphId(0x49 + 100)));
    }

    #[test]
    fn each_step_is_charged_the_segments_and_groups_it_reads() {
        // Unsorted format 4: after U+F000, the next step reads every segment
        // from the first, 32 of them, before it finds nothing.
        let mut list = vec![(0xf000, 0xf000, 0x1001)];
        list.extend((0..30).map(|i| (0x20 + i, 0x20 + i, 0)));
        list.push((0xffff, 0xffff, 1));
        let data = cmap(&segments(&list));
        let all: Vec<_> = charmaps(&data, 0x100, u64::MAX)[0].mappings().collect();
        assert_eq!(all, [Ok((0xf000, GlyphId(1)))]);
        let limited: Vec<_> = charmaps(&data, 0x100, 10)[0].mappings().collect();
        assert_eq!(
            limited,
            [Ok((0xf000, GlyphId(1))), Err(Error::Policy("font work"))]
        );
        // Format 8: the first step reads 30 groups whose glyphs are past the
        // glyph count before the one that maps.
        let mut mixed = words(&[8, 0]);
        let groups: Vec<(u32, u32, u32)> = (0..30)
            .map(|i| (0x100 + 2 * i, 0x100 + 2 * i, 1000))
            .chain([(0x200, 0x200, 5)])
            .collect();
        mixed.extend((8208 + 12 * groups.len() as u32).to_be_bytes());
        mixed.extend([0; 4]);
        mixed.extend([0; 8192]);
        mixed.extend((groups.len() as u32).to_be_bytes());
        for (start, end, glyph) in &groups {
            for v in [start, end, glyph] {
                mixed.extend(v.to_be_bytes());
            }
        }
        let data = cmap(&mixed);
        let all: Vec<_> = charmaps(&data, 100, u64::MAX)[0].mappings().collect();
        assert_eq!(all, [Ok((0x200, GlyphId(5)))]);
        let limited: Vec<_> = charmaps(&data, 100, 20)[0].mappings().collect();
        assert_eq!(limited, [Err(Error::Policy("font work"))]);
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
        assert_eq!(
            charmap.kind(),
            CharmapKind::Subtable(CmapFormat::SegmentedCoverage)
        );
        assert_eq!(charmap.glyph(0x21), Some(GlyphId(6)));
        assert_eq!(charmap.glyph(0x1_f600), Some(GlyphId(9)));
        assert_eq!(charmap.glyph(0x22), None);
        let mut many = data.clone();
        many[13] = 13;
        let charmap = charmaps(&many, 100, u64::MAX)[0];
        assert_eq!(
            (charmap.kind(), charmap.glyph(0x21)),
            (
                CharmapKind::Subtable(CmapFormat::ManyToOne),
                Some(GlyphId(5))
            )
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
            (charmap.kind(), charmap.glyph(0x41)),
            (
                CharmapKind::Subtable(CmapFormat::ByteEncoding),
                Some(GlyphId(0x20))
            )
        );
        assert!(charmaps(&cmap(&words(&[2, 0, 0])), 10, u64::MAX).is_empty());
        assert!(charmaps(&cmap(&words(&[0, 262, 0])), 10, u64::MAX).is_empty());
    }

    #[test]
    fn text_charmap_follows_libass_then_freetype() {
        use CmapFormat::{ByteEncoding, ManyToOne, SegmentDelta, SegmentedCoverage};
        let charmaps = |records: &[(u16, u16, CmapFormat)]| -> Vec<Charmap<'static>> {
            records
                .iter()
                .enumerate()
                .map(|(offset, &(platform, encoding, format))| {
                    let subtable = Subtable {
                        platform: Platform::from_id(platform),
                        encoding,
                        format,
                        flags: 0,
                        offset,
                        end: 0,
                    };
                    Charmap::new(&[], 10, Source::Table(subtable), 0)
                })
                .collect()
        };
        let chosen =
            |records: &[(u16, u16, CmapFormat)]| text_charmap(charmaps(records).into_iter());
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
        // The charmap FreeType builds from glyph names is a Windows Unicode one;
        // an Adobe encoding charmap is neither.
        let budget = Budget::new(1 << 16).unwrap();
        let names: [Option<&[u8]>; 1] = [Some(b"A")];
        let map = crate::postscript::UnicodeMap::new(names.into_iter(), &budget)
            .unwrap()
            .unwrap();
        let encoding = crate::cff::Encoding {
            id: 0,
            glyphs: [0; 256],
        };
        let mut built = charmaps(&[(1, 0, ByteEncoding)]);
        built.extend([
            Charmap::new(&[], 10, Source::Names(&map), 0),
            Charmap::new(&[], 10, Source::Encoding(&encoding), 0),
        ]);
        assert_eq!(text_charmap(built.iter().copied()), Some(1));
        built.remove(1);
        assert_eq!(text_charmap(built.iter().copied()), Some(0));
    }
}
