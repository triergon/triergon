//! The face-wide tables: `head`, `hhea` and `vhea`, `maxp`, `OS/2`, and `hmtx`
//! and `vmtx` (OpenType).

use crate::read::{Reader, u16_at, u32_at};
use crate::sfnt::Table;
use triergon_core::{Error, Result};

/// The `head` fields a face needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    /// Bit 3: scale by integer pixels per em.
    pub(crate) flags: u16,
    pub(crate) units_per_em: u16,
    pub(crate) bounds: [i16; 4],
    pub(crate) mac_style: MacStyle,
    /// `indexToLocFormat` is not 0: 32-bit `loca` offsets.
    pub(crate) long_offsets: bool,
}

pub(crate) fn head(data: &[u8]) -> Result<Head> {
    let mut r = Reader::new(data);
    r.skip(16)?; // version .. magicNumber
    let flags = r.u16()?;
    let units_per_em = r.u16()?;
    if !(16..=16384).contains(&units_per_em) {
        return Err(Error::Invalid("unitsPerEm outside 16..16384"));
    }
    r.skip(16)?; // created, modified
    let bounds = [r.i16()?, r.i16()?, r.i16()?, r.i16()?];
    let mac_style = MacStyle(r.u16()?);
    r.skip(4)?; // lowestRecPPEM, fontDirectionHint
    // FreeType reads every format other than 0 as long offsets.
    let long_offsets = r.i16()? != 0;
    Ok(Head {
        flags,
        units_per_em,
        bounds,
        mac_style,
        long_offsets,
    })
}

/// The `hhea` fields a face needs, or those of `vhea`, which has the same
/// layout: vertical ascender, descender and line gap, the largest advance
/// height and the number of `vmtx` records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hhea {
    pub(crate) ascender: i16,
    pub(crate) descender: i16,
    pub(crate) line_gap: i16,
    pub(crate) advance_max: u16,
    pub(crate) metrics: u32,
}

/// A metrics header and the table of its records as FreeType's
/// `sfnt_load_face` finds them (`tt_face_load_hhea`, `tt_face_load_hmtx`):
/// the lower-case header (`hhea` or `vhea`) with the lower-case records, else
/// the extended upper-case header with the upper-case records. `None` when
/// either is missing.
///
/// FreeType reads the header's 34 bytes and its record count, 16-bit or for
/// an extended header 32-bit, from the file at the table's offset rather than
/// from the table: a short header reads on into the bytes after it.
///
/// # Errors
///
/// `Invalid` when the header runs past the end of the file, where FreeType
/// fails to open the face.
pub(crate) fn header_and_records(
    file: &[u8],
    header: [Option<Table>; 2],
    records: [Option<Table>; 2],
) -> Result<Option<(Hhea, Table)>> {
    let (table, extended) = match header {
        [Some(table), _] => (table, false),
        [None, Some(table)] => (table, true),
        [None, None] => return Ok(None),
    };
    let short = |_| Error::Invalid("metrics header past the end of the file");
    let mut r = Reader::at(file, table.offset).map_err(short)?;
    let mut read = || -> Result<Hhea> {
        r.skip(4)?; // version
        let ascender = r.i16()?;
        let descender = r.i16()?;
        let line_gap = r.i16()?;
        let advance_max = r.u16()?;
        r.skip(22)?; // minLeftSideBearing .. metricDataFormat
        let metrics = if extended {
            r.u32()?
        } else {
            u32::from(r.u16()?)
        };
        Ok(Hhea {
            ascender,
            descender,
            line_gap,
            advance_max,
            metrics,
        })
    };
    let header = read().map_err(short)?;
    Ok(records[usize::from(extended)].map(|records| (header, records)))
}

/// `maxp.numGlyphs`; FreeType does not check the table version.
pub(crate) fn glyph_count(data: &[u8]) -> Result<u16> {
    let mut r = Reader::new(data);
    r.u32()?; // version
    match r.u16()? {
        0 => Err(Error::Invalid("face has no glyphs")),
        count => Ok(count),
    }
}

/// The `OS/2` fields a face exposes. Fields a table version lacks are zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Os2 {
    pub version: u16,
    pub weight_class: u16,
    pub width_class: u16,
    pub fs_type: u16,
    pub strikeout_size: i16,
    pub strikeout_position: i16,
    pub selection: FsSelection,
    pub typo_ascender: i16,
    pub typo_descender: i16,
    pub typo_line_gap: i16,
    pub win_ascent: u16,
    pub win_descent: u16,
}

/// The `head.macStyle` flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MacStyle(u16);

impl MacStyle {
    /// Bit 0.
    pub const fn bold(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 1.
    pub const fn italic(self) -> bool {
        self.0 & 1 << 1 != 0
    }

    /// The stored bits, including those without a name here.
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// The `OS/2.fsSelection` flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FsSelection(u16);

impl FsSelection {
    /// Bit 0.
    pub const fn italic(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 5.
    pub const fn bold(self) -> bool {
        self.0 & 1 << 5 != 0
    }

    /// Bit 6.
    pub const fn regular(self) -> bool {
        self.0 & 1 << 6 != 0
    }

    /// Bit 7, USE_TYPO_METRICS: line spacing comes from the typographic metrics.
    pub const fn use_typo_metrics(self) -> bool {
        self.0 & 1 << 7 != 0
    }

    /// Bit 8, WWS: the family name already separates weight, width and slope.
    pub const fn wws(self) -> bool {
        self.0 & 1 << 8 != 0
    }

    /// Bit 9.
    pub const fn oblique(self) -> bool {
        self.0 & 1 << 9 != 0
    }

    /// The stored bits, including those without a name here.
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// The `OS/2` table, or `None` when it is shorter than version 0's 78 bytes.
pub(crate) fn os2(data: &[u8]) -> Option<Os2> {
    let at = |offset| u16_at(data, offset);
    if data.len() < 78 {
        return None;
    }
    Some(Os2 {
        version: at(0)?,
        weight_class: at(4)?,
        width_class: at(6)?,
        fs_type: at(8)?,
        strikeout_size: at(26)?.cast_signed(),
        strikeout_position: at(28)?.cast_signed(),
        selection: FsSelection(at(62)?),
        typo_ascender: at(68)?.cast_signed(),
        typo_descender: at(70)?.cast_signed(),
        typo_line_gap: at(72)?.cast_signed(),
        win_ascent: at(74)?,
        win_descent: at(76)?,
    })
}

/// The `post` underline position and thickness, both zero when the table is
/// absent or shorter than its 32-byte header, as FreeType reports them.
pub(crate) fn underline(data: Option<&[u8]>) -> (i16, i16) {
    match data {
        Some(post) if post.len() >= 32 => (
            u16_at(post, 8).map_or(0, u16::cast_signed),
            u16_at(post, 10).map_or(0, u16::cast_signed),
        ),
        _ => (0, 0),
    }
}

/// Whether `post` declares the face fixed pitch (`isFixedPitch`), which
/// FreeType reports as `FT_FACE_FLAG_FIXED_WIDTH`; false when the table is
/// absent or shorter than its 32-byte header.
pub(crate) fn fixed_pitch(data: Option<&[u8]>) -> bool {
    match data {
        Some(post) if post.len() >= 32 => u32_at(post, 12).is_some_and(|v| v != 0),
        _ => false,
    }
}

/// `hmtx` or `vmtx`: long records of an advance (width or height) and a side
/// bearing (left or top), then side bearings that reuse the last advance.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LongMetrics {
    table: Table,
    metrics: u16,
}

impl LongMetrics {
    /// Records beyond the glyph count are ignored. Reads past the table give
    /// zero, as FreeType reads a short table.
    pub(crate) fn new(table: Table, metrics: u32, glyphs: u16) -> Self {
        Self {
            table,
            metrics: u16::try_from(metrics).map_or(glyphs, |m| m.min(glyphs)),
        }
    }

    /// The advance of `glyph`, which must be below the glyph count: zero when
    /// its long record (or, past the records, the last record's advance) is
    /// cut off or there are no records.
    pub(crate) fn advance(self, data: &[u8], glyph: u16) -> u16 {
        let Some(last) = self.metrics.checked_sub(1) else {
            return 0;
        };
        let (record, needed) = if glyph <= last { (glyph, 4) } else { (last, 2) };
        let at = 4 * usize::from(record);
        let bytes = self.table.bytes(data);
        if bytes.len() < at + needed {
            return 0;
        }
        u16_at(bytes, at).unwrap_or(0)
    }

    /// The side bearing of `glyph`, left or top: from its long record, else
    /// from the side-bearing array after the records; zero where the table
    /// lacks it or there are no records, as FreeType reads them.
    pub(crate) fn bearing(self, data: &[u8], glyph: u16) -> i16 {
        let (glyph, metrics) = (usize::from(glyph), usize::from(self.metrics));
        if metrics == 0 {
            return 0;
        }
        let at = if glyph < metrics {
            4 * glyph + 2
        } else {
            4 * metrics + 2 * (glyph - metrics)
        };
        u16_at(self.table.bytes(data), at).map_or(0, u16::cast_signed)
    }
}

#[cfg(test)]
mod tests {
    use super::{LongMetrics, glyph_count, head, header_and_records, os2};
    use crate::sfnt::Table;

    #[test]
    fn head_checks_units_and_location_format() {
        let mut data = [0_u8; 54];
        data[18..20].copy_from_slice(&1000_u16.to_be_bytes());
        data[50..52].copy_from_slice(&1_i16.to_be_bytes());
        let parsed = head(&data).unwrap();
        assert_eq!((parsed.units_per_em, parsed.long_offsets), (1000, true));
        data[50] = 2;
        assert!(head(&data).unwrap().long_offsets);
        data[18..20].copy_from_slice(&15_u16.to_be_bytes());
        assert!(head(&data).is_err());
        assert!(head(&data[..53]).is_err());
    }

    #[test]
    fn advances_repeat_the_last_record() {
        // Two records (100, 200) and one side bearing.
        let data = [0, 100, 0, 0, 0, 200, 0, 0, 0, 5];
        let table = Table { offset: 0, len: 10 };
        let hmtx = LongMetrics::new(table, 2, 3);
        assert_eq!([0, 1, 2].map(|g| hmtx.advance(&data, g)), [100, 200, 200]);
        assert_eq!([0, 1, 2, 3].map(|g| hmtx.bearing(&data, g)), [0, 0, 5, 0]);
        // A record cut off reads as zero; so does a table without records.
        let short = LongMetrics::new(Table { offset: 0, len: 7 }, 2, 3);
        assert_eq!([0, 1, 2].map(|g| short.advance(&data, g)), [100, 0, 200]);
        let empty = LongMetrics::new(table, 0, 3);
        assert_eq!((empty.advance(&data, 1), empty.bearing(&data, 1)), (0, 0));
    }

    #[test]
    fn metrics_headers_read_from_the_file_as_freetype_reads_them() {
        let mut file = vec![0_u8; 34];
        file[4..6].copy_from_slice(&800_i16.to_be_bytes());
        // A 32-byte `vhea` whose record count is the first advance of the
        // `vmtx` after it, then that `vmtx`.
        file.extend([0, 3, 0, 9, 0, 7]);
        let vhea = Table { offset: 0, len: 32 };
        let vmtx = Table { offset: 34, len: 4 };
        let (header, records) = header_and_records(&file, [Some(vhea), None], [Some(vmtx), None])
            .unwrap()
            .unwrap();
        assert_eq!((header.ascender, header.metrics, records), (800, 3, vmtx));
        // The extended header's count is 32-bit, and its records are the
        // extended table's; a missing records table leaves both out.
        let extended = header_and_records(&file, [None, Some(vhea)], [Some(vmtx), Some(vmtx)]);
        assert_eq!(extended.unwrap().unwrap().0.metrics, 0x3_0009);
        assert_eq!(
            header_and_records(&file, [None, Some(vhea)], [Some(vmtx), None]),
            Ok(None)
        );
        assert_eq!(
            header_and_records(&file, [None, None], [Some(vmtx), None]),
            Ok(None)
        );
        // A header that runs past the end of the file fails the face.
        let late = Table { offset: 4, len: 32 };
        assert!(header_and_records(&file[..39], [Some(late), None], [Some(vmtx), None]).is_err());
        assert!(header_and_records(&file, [Some(late), None], [Some(vmtx), None]).is_ok());
    }

    #[test]
    fn maxp_versions_are_not_checked_and_os2_versions_are() {
        assert_eq!(glyph_count(&[0, 0, 0x50, 0, 0, 7]), Ok(7));
        assert!(glyph_count(&[0, 0, 0x50, 0, 0, 0]).is_err());
        assert_eq!(glyph_count(&[0, 2, 0, 0, 0, 7]), Ok(7));
        let mut table = [0_u8; 78];
        table[4..6].copy_from_slice(&700_u16.to_be_bytes());
        table[74..76].copy_from_slice(&900_u16.to_be_bytes());
        let parsed = os2(&table).unwrap();
        assert_eq!((parsed.weight_class, parsed.win_ascent), (700, 900));
        assert!(os2(&table[..77]).is_none());
    }
}
