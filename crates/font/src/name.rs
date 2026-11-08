//! The naming table (OpenType `name`), decoded lazily.

use crate::read::{Reader, slice, u16_at};
use triergon_core::Result;

/// A naming-table record's name identifier (OpenType `name`, "Name IDs").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NameId {
    Copyright,
    Family,
    Subfamily,
    UniqueId,
    FullName,
    Version,
    PostScript,
    Trademark,
    Manufacturer,
    Designer,
    Description,
    VendorUrl,
    DesignerUrl,
    License,
    LicenseUrl,
    TypographicFamily,
    TypographicSubfamily,
    CompatibleFull,
    SampleText,
    PostScriptCid,
    WwsFamily,
    WwsSubfamily,
    /// Any other identifier, including font-specific ones from 256.
    Other(u16),
}

impl NameId {
    pub const fn from_id(id: u16) -> Self {
        match id {
            0 => Self::Copyright,
            1 => Self::Family,
            2 => Self::Subfamily,
            3 => Self::UniqueId,
            4 => Self::FullName,
            5 => Self::Version,
            6 => Self::PostScript,
            7 => Self::Trademark,
            8 => Self::Manufacturer,
            9 => Self::Designer,
            10 => Self::Description,
            11 => Self::VendorUrl,
            12 => Self::DesignerUrl,
            13 => Self::License,
            14 => Self::LicenseUrl,
            16 => Self::TypographicFamily,
            17 => Self::TypographicSubfamily,
            18 => Self::CompatibleFull,
            19 => Self::SampleText,
            20 => Self::PostScriptCid,
            21 => Self::WwsFamily,
            22 => Self::WwsSubfamily,
            other => Self::Other(other),
        }
    }

    /// The identifier the record stores.
    pub const fn id(self) -> u16 {
        match self {
            Self::Copyright => 0,
            Self::Family => 1,
            Self::Subfamily => 2,
            Self::UniqueId => 3,
            Self::FullName => 4,
            Self::Version => 5,
            Self::PostScript => 6,
            Self::Trademark => 7,
            Self::Manufacturer => 8,
            Self::Designer => 9,
            Self::Description => 10,
            Self::VendorUrl => 11,
            Self::DesignerUrl => 12,
            Self::License => 13,
            Self::LicenseUrl => 14,
            Self::TypographicFamily => 16,
            Self::TypographicSubfamily => 17,
            Self::CompatibleFull => 18,
            Self::SampleText => 19,
            Self::PostScriptCid => 20,
            Self::WwsFamily => 21,
            Self::WwsSubfamily => 22,
            Self::Other(id) => id,
        }
    }
}

/// The platform of a naming-table or `cmap` encoding record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    Unicode,
    Macintosh,
    /// ISO (deprecated); its records are UTF-16BE like Unicode-platform ones.
    Iso,
    Windows,
    Custom,
    Other(u16),
}

impl Platform {
    pub const fn from_id(id: u16) -> Self {
        match id {
            0 => Self::Unicode,
            1 => Self::Macintosh,
            2 => Self::Iso,
            3 => Self::Windows,
            4 => Self::Custom,
            other => Self::Other(other),
        }
    }

    pub const fn id(self) -> u16 {
        match self {
            Self::Unicode => 0,
            Self::Macintosh => 1,
            Self::Iso => 2,
            Self::Windows => 3,
            Self::Custom => 4,
            Self::Other(id) => id,
        }
    }
}

const RECORD: usize = 12;

/// A validated naming table: records and string storage stay in the font bytes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Names {
    records: usize,
    count: u16,
    storage: usize,
    end: usize,
}

impl Names {
    /// Reads the table header at `offset..offset + len` of the file.
    pub(crate) fn new(data: &[u8], offset: usize, len: usize) -> Result<Self> {
        let table = slice(data, offset, len)?;
        let mut r = Reader::new(table);
        // FreeType reads any version as format 0, records first.
        r.u16()?;
        let count = r.u16()?;
        let storage = usize::from(r.u16()?);
        r.skip(usize::from(count) * RECORD)?;
        Ok(Self {
            records: offset + 6,
            count,
            storage: offset + storage,
            end: offset + len,
        })
    }

    #[cfg(test)]
    pub(crate) const fn len(self) -> u16 {
        self.count
    }

    /// Record `index`, or `None` past the end or when its string is empty or lies
    /// outside the string storage: FreeType skips such records, and so do
    /// [`crate::Font::names`] and the face's name selection.
    pub(crate) fn record(self, data: &[u8], index: u16) -> Option<NameRecord<'_>> {
        if index >= self.count {
            return None;
        }
        let at = self.records + RECORD * usize::from(index);
        let field = |n: usize| u16_at(data, at + 2 * n);
        let start = self.storage + usize::from(field(5)?);
        let end = start + usize::from(field(4)?);
        let records_end = self.records + RECORD * usize::from(self.count);
        if start == end || start < records_end || end > self.end {
            return None;
        }
        Some(NameRecord {
            platform: Platform::from_id(field(0)?),
            encoding_id: field(1)?,
            language_id: field(2)?,
            name_id: NameId::from_id(field(3)?),
            bytes: data.get(start..end)?,
        })
    }

    /// The usable records, in table order.
    pub(crate) fn records(self, data: &[u8]) -> impl Iterator<Item = NameRecord<'_>> + '_ {
        (0..self.count).filter_map(move |i| self.record(data, i))
    }

    /// The record FreeType reports for `name_id`: the last English Windows record,
    /// else the first Windows one, unless a Macintosh record exists and the Windows
    /// one is not English; then the last English (or Roman) Macintosh record; then
    /// the last Unicode-platform record.
    pub(crate) fn preferred(self, data: &[u8], name_id: NameId) -> Option<NameRecord<'_>> {
        let (mut windows, mut english) = (None, false);
        let (mut mac_english, mut mac_roman, mut unicode) = (None, None, None);
        for record in self.records(data).filter(|r| r.name_id == name_id) {
            match record.platform {
                Platform::Unicode | Platform::Iso => unicode = Some(record),
                Platform::Macintosh if record.language_id == 0 => mac_english = Some(record),
                Platform::Macintosh if record.encoding_id == 0 => mac_roman = Some(record),
                Platform::Windows
                    if (windows.is_none() || record.language_id & 0x3ff == 9)
                        && matches!(record.encoding_id, 0 | 1 | 10) =>
                {
                    english = record.language_id & 0x3ff == 9;
                    windows = Some(record);
                }
                _ => {}
            }
        }
        let mac = mac_english.or(mac_roman);
        match windows {
            Some(windows) if mac.is_none() || english => Some(windows),
            _ => mac.or(unicode),
        }
    }
}

/// One naming-table record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameRecord<'a> {
    pub platform: Platform,
    /// The platform-specific encoding number.
    pub encoding_id: u16,
    /// The platform-specific language number.
    pub language_id: u16,
    pub name_id: NameId,
    /// The encoded string.
    pub bytes: &'a [u8],
}

impl<'a> NameRecord<'a> {
    /// Whether the record is UTF-16BE: the Unicode, ISO and Windows platforms.
    const fn is_utf16(&self) -> bool {
        matches!(
            self.platform,
            Platform::Unicode | Platform::Iso | Platform::Windows
        )
    }

    /// Whether this crate decodes the record: UTF-16BE platforms, and Macintosh
    /// Roman (encoding 0) as Mac OS Roman.
    pub fn is_decodable(&self) -> bool {
        self.is_utf16() || (self.platform == Platform::Macintosh && self.encoding_id == 0)
    }

    /// The decoded characters. Unpaired UTF-16 surrogates become U+FFFD; an odd
    /// final byte is dropped. Records [`Self::is_decodable`] rejects yield nothing.
    pub fn chars(&self) -> NameChars<'a> {
        NameChars {
            bytes: if self.is_decodable() { self.bytes } else { &[] },
            utf16: self.is_utf16(),
        }
    }

    /// The raw code units: 16-bit units for UTF-16BE platforms, bytes otherwise, as
    /// FreeType reads them when it forms ASCII names.
    pub fn code_units(&self) -> impl Iterator<Item = u16> + 'a {
        let width = if self.is_utf16() { 2 } else { 1 };
        self.bytes.chunks_exact(width).map(move |unit| match unit {
            [high, low] => u16::from_be_bytes([*high, *low]),
            [byte] => u16::from(*byte),
            _ => 0,
        })
    }

    /// Whether the decoded string equals `text` ignoring ASCII case.
    pub fn eq_ignore_ascii_case(&self, text: &str) -> bool {
        let mut chars = self.chars();
        text.chars()
            .all(|c| chars.next().is_some_and(|n| n.eq_ignore_ascii_case(&c)))
            && chars.next().is_none()
    }
}

/// Characters of a [`NameRecord`].
#[derive(Clone, Debug)]
pub struct NameChars<'a> {
    bytes: &'a [u8],
    utf16: bool,
}

impl Iterator for NameChars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        if !self.utf16 {
            let (&byte, rest) = self.bytes.split_first()?;
            self.bytes = rest;
            return Some(mac_roman(byte));
        }
        let unit = |bytes: &[u8]| Some(u16::from_be_bytes(*bytes.first_chunk()?));
        let first = unit(self.bytes)?;
        self.bytes = &self.bytes[2..];
        if !(0xd800..0xdc00).contains(&first) {
            return Some(char::from_u32(u32::from(first)).unwrap_or('\u{fffd}'));
        }
        match unit(self.bytes) {
            Some(second @ 0xdc00..0xe000) => {
                self.bytes = &self.bytes[2..];
                let scalar =
                    0x1_0000 + ((u32::from(first) - 0xd800) << 10) + u32::from(second) - 0xdc00;
                Some(char::from_u32(scalar).unwrap_or('\u{fffd}'))
            }
            _ => Some('\u{fffd}'),
        }
    }
}

/// Mac OS Roman, bytes 0x80 to 0xFF (Unicode's ROMAN.TXT mapping).
const MAC_ROMAN_HIGH: [char; 128] = [
    'Ä', 'Å', 'Ç', 'É', 'Ñ', 'Ö', 'Ü', 'á', 'à', 'â', 'ä', 'ã', 'å', 'ç', 'é', 'è', //
    'ê', 'ë', 'í', 'ì', 'î', 'ï', 'ñ', 'ó', 'ò', 'ô', 'ö', 'õ', 'ú', 'ù', 'û', 'ü', //
    '†', '°', '¢', '£', '§', '•', '¶', 'ß', '®', '©', '™', '´', '¨', '≠', 'Æ', 'Ø', //
    '∞', '±', '≤', '≥', '¥', 'µ', '∂', '∑', '∏', 'π', '∫', 'ª', 'º', 'Ω', 'æ', 'ø', //
    '¿', '¡', '¬', '√', 'ƒ', '≈', '∆', '«', '»', '…', '\u{a0}', 'À', 'Ã', 'Õ', 'Œ', 'œ', //
    '–', '—', '“', '”', '‘', '’', '÷', '◊', 'ÿ', 'Ÿ', '⁄', '€', '‹', '›', 'ﬁ', 'ﬂ', //
    '‡', '·', '‚', '„', '‰', 'Â', 'Ê', 'Á', 'Ë', 'È', 'Í', 'Î', 'Ï', 'Ì', 'Ó', 'Ô', //
    '\u{f8ff}', 'Ò', 'Ú', 'Û', 'Ù', 'ı', 'ˆ', '˜', '¯', '˘', '˙', '˚', '¸', '˝', '˛', 'ˇ', //
];

fn mac_roman(byte: u8) -> char {
    match byte.checked_sub(0x80) {
        Some(high) => MAC_ROMAN_HIGH[usize::from(high)],
        None => char::from(byte),
    }
}

#[cfg(test)]
mod tests {
    use super::{NameId, NameRecord, Names, Platform};

    fn record(platform: u16, encoding: u16, bytes: &[u8]) -> NameRecord<'_> {
        NameRecord {
            platform: Platform::from_id(platform),
            encoding_id: encoding,
            language_id: 0,
            name_id: NameId::Family,
            bytes,
        }
    }

    #[test]
    fn utf16_surrogates_and_mac_roman_decode() {
        let windows = record(
            3,
            1,
            &[0, b'A', 0xd8, 0x3d, 0xde, 0x00, 0xd8, 0x00, 0, b'z', 1],
        );
        assert_eq!(windows.chars().collect::<String>(), "A😀\u{fffd}z");
        let mac = record(1, 0, b"Caf\x8e");
        assert_eq!(mac.chars().collect::<String>(), "Café");
        assert!(mac.eq_ignore_ascii_case("CAFé"));
        assert!(!mac.eq_ignore_ascii_case("Caf"));
        assert!(!mac.eq_ignore_ascii_case("Caféx"));
        assert_eq!(record(1, 1, b"x").chars().count(), 0);
    }

    #[test]
    fn records_outside_storage_are_empty() {
        // Two records; the second points past the table end.
        let mut table = vec![0, 0, 0, 2, 0, 30];
        for (offset, len) in [(0_u16, 2_u16), (2, 4)] {
            for field in [3_u16, 1, 0x409, 1, len, offset] {
                table.extend(field.to_be_bytes());
            }
        }
        table.extend([0, b'X', 0]);
        let names = Names::new(&table, 0, table.len()).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names.record(&table, 0).unwrap().bytes, [0, b'X']);
        assert_eq!(names.record(&table, 1), None);
        assert_eq!(names.records(&table).count(), 1);
        assert_eq!(
            names.preferred(&table, NameId::Family).unwrap().bytes,
            [0, b'X']
        );
        assert_eq!(
            (
                NameId::from_id(6),
                NameId::PostScript.id(),
                NameId::from_id(300).id()
            ),
            (NameId::PostScript, 6, 300)
        );
        assert!(Names::new(&table, 0, 20).is_err());
    }
}
