//! The face-wide tables: `head`, `hhea`, `maxp`, `OS/2` and `hmtx` (OpenType).

use crate::read::{Reader, u16_at};
use crate::sfnt::Table;
use triergon_core::{Error, Result};

/// The `head` fields a face needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    /// Bit 3: scale by integer pixels per em.
    pub(crate) flags: u16,
    pub(crate) units_per_em: u16,
    pub(crate) bounds: [i16; 4],
    pub(crate) mac_style: u16,
    /// `indexToLocFormat` is 1: 32-bit `loca` offsets.
    pub(crate) long_offsets: bool,
}

pub(crate) fn head(data: &[u8]) -> Result<Head> {
    let mut r = Reader::new(data);
    r.skip(16)?; // version .. magicNumber
    let flags = r.u16()?;
    let units_per_em = r.u16()?;
    if !(1..=16384).contains(&units_per_em) {
        return Err(Error::Invalid("unitsPerEm outside 1..16384"));
    }
    r.skip(16)?; // created, modified
    let bounds = [r.i16()?, r.i16()?, r.i16()?, r.i16()?];
    let mac_style = r.u16()?;
    r.skip(4)?; // lowestRecPPEM, fontDirectionHint
    let long_offsets = match r.i16()? {
        0 => false,
        1 => true,
        _ => return Err(Error::Invalid("indexToLocFormat")),
    };
    Ok(Head {
        flags,
        units_per_em,
        bounds,
        mac_style,
        long_offsets,
    })
}

/// The `hhea` fields a face needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hhea {
    pub(crate) ascender: i16,
    pub(crate) descender: i16,
    pub(crate) line_gap: i16,
    pub(crate) advance_max: u16,
    pub(crate) metrics: u16,
}

pub(crate) fn hhea(data: &[u8]) -> Result<Hhea> {
    let mut r = Reader::new(data);
    r.skip(4)?; // version
    let ascender = r.i16()?;
    let descender = r.i16()?;
    let line_gap = r.i16()?;
    let advance_max = r.u16()?;
    r.skip(22)?; // minLeftSideBearing .. metricDataFormat
    let metrics = r.u16()?;
    Ok(Hhea {
        ascender,
        descender,
        line_gap,
        advance_max,
        metrics,
    })
}

/// `maxp.numGlyphs`, in either table version.
pub(crate) fn glyph_count(data: &[u8]) -> Result<u16> {
    let mut r = Reader::new(data);
    match r.u32()? {
        0x0000_5000 | 0x0001_0000 => {}
        _ => return Err(Error::Invalid("maxp version")),
    }
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

/// Horizontal advances: `numberOfHMetrics` long records, then side bearings that
/// reuse the last advance.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Hmtx {
    table: Table,
    metrics: u16,
}

impl Hmtx {
    /// Checks that every stored advance is present. A table cut short after the
    /// last advance (side bearings missing) is still usable.
    pub(crate) fn new(table: Table, metrics: u16, glyphs: u16) -> Result<Self> {
        let metrics = metrics.min(glyphs);
        if metrics == 0 {
            return Err(Error::Invalid("numberOfHMetrics is zero"));
        }
        if table.len < usize::from(metrics) * 4 - 2 {
            return Err(Error::Invalid("hmtx shorter than its advances"));
        }
        Ok(Self { table, metrics })
    }

    /// The advance of `glyph`, which must be below the glyph count.
    pub(crate) fn advance(self, data: &[u8], glyph: u16) -> u16 {
        let record = glyph.min(self.metrics - 1);
        u16_at(self.table.bytes(data), 4 * usize::from(record)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{Hmtx, glyph_count, head, os2};
    use crate::sfnt::Table;
    use triergon_core::Error;

    #[test]
    fn head_checks_units_and_location_format() {
        let mut data = [0_u8; 54];
        data[18..20].copy_from_slice(&1000_u16.to_be_bytes());
        data[50..52].copy_from_slice(&1_i16.to_be_bytes());
        let parsed = head(&data).unwrap();
        assert_eq!((parsed.units_per_em, parsed.long_offsets), (1000, true));
        data[50] = 2;
        assert_eq!(head(&data), Err(Error::Invalid("indexToLocFormat")));
        data[18..20].copy_from_slice(&[0, 0]);
        assert!(head(&data).is_err());
        assert!(head(&data[..53]).is_err());
    }

    #[test]
    fn advances_repeat_the_last_record() {
        // Two records (100, 200) and one side bearing.
        let data = [0, 100, 0, 0, 0, 200, 0, 0, 0, 5];
        let table = Table { offset: 0, len: 10 };
        let hmtx = Hmtx::new(table, 2, 3).unwrap();
        assert_eq!([0, 1, 2].map(|g| hmtx.advance(&data, g)), [100, 200, 200]);
        assert!(Hmtx::new(Table { offset: 0, len: 5 }, 2, 3).is_err());
        assert!(Hmtx::new(table, 0, 3).is_err());
    }

    #[test]
    fn maxp_and_os2_versions() {
        assert_eq!(glyph_count(&[0, 0, 0x50, 0, 0, 7]), Ok(7));
        assert!(glyph_count(&[0, 0, 0x50, 0, 0, 0]).is_err());
        assert!(glyph_count(&[0, 2, 0, 0, 0, 7]).is_err());
        let mut table = [0_u8; 78];
        table[4..6].copy_from_slice(&700_u16.to_be_bytes());
        table[74..76].copy_from_slice(&900_u16.to_be_bytes());
        let parsed = os2(&table).unwrap();
        assert_eq!((parsed.weight_class, parsed.win_ascent), (700, 900));
        assert!(os2(&table[..77]).is_none());
    }
}
