//! The naming table (OpenType `name`), decoded lazily.

use crate::read::{Reader, slice, u16_at};
use triergon_core::{Error, Result};

/// Name identifiers this crate interprets (OpenType `name` "Name IDs").
pub mod id {
    pub const FAMILY: u16 = 1;
    pub const SUBFAMILY: u16 = 2;
    pub const FULL_NAME: u16 = 4;
    pub const POSTSCRIPT: u16 = 6;
    pub const TYPOGRAPHIC_FAMILY: u16 = 16;
    pub const TYPOGRAPHIC_SUBFAMILY: u16 = 17;
    pub const WWS_FAMILY: u16 = 21;
    pub const WWS_SUBFAMILY: u16 = 22;
}

/// Platform identifiers.
pub mod platform {
    pub const UNICODE: u16 = 0;
    pub const MACINTOSH: u16 = 1;
    pub const WINDOWS: u16 = 3;
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
        let version = r.u16()?;
        if version > 1 {
            return Err(Error::Invalid("name table version"));
        }
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
            platform: field(0)?,
            encoding: field(1)?,
            language: field(2)?,
            name_id: field(3)?,
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
    pub(crate) fn preferred(self, data: &[u8], name_id: u16) -> Option<NameRecord<'_>> {
        let (mut windows, mut english) = (None, false);
        let (mut mac_english, mut mac_roman, mut unicode) = (None, None, None);
        for record in self.records(data).filter(|r| r.name_id == name_id) {
            match record.platform {
                platform::UNICODE | 2 => unicode = Some(record),
                platform::MACINTOSH if record.language == 0 => mac_english = Some(record),
                platform::MACINTOSH if record.encoding == 0 => mac_roman = Some(record),
                platform::WINDOWS
                    if (windows.is_none() || record.language & 0x3ff == 9)
                        && matches!(record.encoding, 0 | 1 | 10) =>
                {
                    english = record.language & 0x3ff == 9;
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
    pub platform: u16,
    pub encoding: u16,
    pub language: u16,
    pub name_id: u16,
    /// The encoded string.
    pub bytes: &'a [u8],
}

impl<'a> NameRecord<'a> {
    /// Whether this crate decodes the record: Unicode and Windows platforms are
    /// UTF-16BE, Macintosh Roman (platform 1, encoding 0) is Mac OS Roman.
    pub fn is_decodable(&self) -> bool {
        matches!(self.platform, platform::UNICODE | platform::WINDOWS)
            || (self.platform == platform::MACINTOSH && self.encoding == 0)
    }

    /// The decoded characters. Unpaired UTF-16 surrogates become U+FFFD; an odd
    /// final byte is dropped. Records [`Self::is_decodable`] rejects yield nothing.
    pub fn chars(&self) -> NameChars<'a> {
        let utf16 = matches!(self.platform, platform::UNICODE | platform::WINDOWS);
        NameChars {
            bytes: if self.is_decodable() { self.bytes } else { &[] },
            utf16,
        }
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
    use super::{NameRecord, Names};

    fn record(platform: u16, encoding: u16, bytes: &[u8]) -> NameRecord<'_> {
        NameRecord {
            platform,
            encoding,
            language: 0,
            name_id: 1,
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
        assert_eq!(names.preferred(&table, 1).unwrap().bytes, [0, b'X']);
        assert!(Names::new(&table, 0, 20).is_err());
    }
}
