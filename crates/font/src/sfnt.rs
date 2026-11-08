//! The sfnt container: collection header and table directory (OpenType "Font file"
//! and "Font collections").

use crate::read::{Meter, Reader, TRUNCATED};
use triergon_core::{Error, Result};

const TRUETYPE: u32 = 0x0001_0000;
const APPLE_TRUE: u32 = u32::from_be_bytes(*b"true");
const OTTO: u32 = u32::from_be_bytes(*b"OTTO");
const COLLECTION: u32 = u32::from_be_bytes(*b"ttcf");
const TABLE_RECORD: usize = 16;

/// The outline technology a face declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flavor {
    /// `glyf` and `loca`.
    TrueType,
    /// `CFF ` or `CFF2`.
    Cff,
}

/// A table's byte range in the file. Collection faces locate tables relative to
/// the start of the file, as standalone fonts do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Table {
    pub(crate) offset: usize,
    pub(crate) len: usize,
}

impl Table {
    pub(crate) fn bytes(self, data: &[u8]) -> &[u8] {
        // Directory parsing keeps every range inside the data.
        data.get(self.offset..self.offset + self.len)
            .unwrap_or_default()
    }
}

/// The tables this crate reads. Others are ignored.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tables {
    pub(crate) head: Option<Table>,
    pub(crate) hhea: Option<Table>,
    pub(crate) hmtx: Option<Table>,
    pub(crate) maxp: Option<Table>,
    pub(crate) os2: Option<Table>,
    pub(crate) name: Option<Table>,
    pub(crate) cmap: Option<Table>,
    pub(crate) glyf: Option<Table>,
    pub(crate) loca: Option<Table>,
    pub(crate) cff: Option<Table>,
    pub(crate) cff2: Option<Table>,
    pub(crate) kern: Option<Table>,
    pub(crate) post: Option<Table>,
    pub(crate) fpgm: Option<Table>,
    pub(crate) prep: Option<Table>,
}

impl Tables {
    fn slot(&mut self, tag: [u8; 4]) -> Option<&mut Option<Table>> {
        Some(match &tag {
            b"head" => &mut self.head,
            b"hhea" => &mut self.hhea,
            b"hmtx" => &mut self.hmtx,
            b"maxp" => &mut self.maxp,
            b"OS/2" => &mut self.os2,
            b"name" => &mut self.name,
            b"cmap" => &mut self.cmap,
            b"glyf" => &mut self.glyf,
            b"loca" => &mut self.loca,
            b"CFF " => &mut self.cff,
            b"CFF2" => &mut self.cff2,
            b"kern" => &mut self.kern,
            b"post" => &mut self.post,
            b"fpgm" => &mut self.fpgm,
            b"prep" => &mut self.prep,
            _ => return None,
        })
    }
}

/// The number of faces in `data`: the collection's count, or one for a single font.
pub(crate) fn face_count(data: &[u8]) -> Result<u32> {
    let mut r = Reader::new(data);
    match r.u32()? {
        COLLECTION => {
            r.skip(4)?; // majorVersion, minorVersion
            let count = r.u32()?;
            if count == 0 {
                return Err(Error::Invalid("empty font collection"));
            }
            Ok(count)
        }
        TRUETYPE | APPLE_TRUE | OTTO => Ok(1),
        _ => Err(Error::Unsupported("font file signature")),
    }
}

/// The directory offset of face `index`.
fn face_offset(data: &[u8], index: u32) -> Result<usize> {
    let count = face_count(data)?;
    if index >= count {
        return Err(Error::Invalid("face index outside the font file"));
    }
    if u32_be(data, 0) != Some(COLLECTION) {
        return Ok(0);
    }
    let at = usize::try_from(index)
        .ok()
        .and_then(|i| i.checked_mul(4)?.checked_add(12))
        .ok_or(Error::Overflow)?;
    let offset = u32_be(data, at).ok_or(TRUNCATED)?;
    usize::try_from(offset).map_err(|_| Error::Overflow)
}

fn u32_be(data: &[u8], at: usize) -> Option<u32> {
    crate::read::u32_at(data, at)
}

/// Reads the table directory of face `index`.
///
/// A table whose range starts beyond the file is absent. One that ends beyond it is
/// absent too, except `hmtx`, which keeps the bytes that exist: its trailing side
/// bearings are optional for outlines and advances.
pub(crate) fn directory(data: &[u8], index: u32, meter: &mut Meter) -> Result<(Flavor, Tables)> {
    let mut r = Reader::at(data, face_offset(data, index)?)?;
    let signature = r.u32()?;
    let count = usize::from(r.u16()?);
    r.skip(6)?; // searchRange, entrySelector, rangeShift
    meter.charge(count)?;
    let records = r.take(count.checked_mul(TABLE_RECORD).ok_or(Error::Overflow)?)?;
    let mut tables = Tables::default();
    for record in records.chunks_exact(TABLE_RECORD) {
        let mut r = Reader::new(record);
        let tag = r.tag()?;
        r.skip(4)?; // checksum
        let offset = usize::try_from(r.u32()?).map_err(|_| Error::Overflow)?;
        let len = usize::try_from(r.u32()?).map_err(|_| Error::Overflow)?;
        let Some(slot) = tables.slot(tag) else {
            continue;
        };
        if slot.is_some() || offset > data.len() {
            continue;
        }
        let available = data.len() - offset;
        let len = if len <= available {
            len
        } else if &tag == b"hmtx" {
            available
        } else {
            continue;
        };
        *slot = Some(Table { offset, len });
    }
    let flavor = match signature {
        TRUETYPE | APPLE_TRUE if tables.glyf.is_some() => Flavor::TrueType,
        OTTO | TRUETYPE if tables.cff.is_some() || tables.cff2.is_some() => Flavor::Cff,
        TRUETYPE | APPLE_TRUE | OTTO => return Err(Error::Invalid("face has no outlines")),
        _ => return Err(Error::Unsupported("sfnt version")),
    };
    Ok((flavor, tables))
}

#[cfg(test)]
mod tests {
    use super::{Flavor, directory, face_count};
    use crate::read::Meter;
    use triergon_core::Error;

    fn font(signature: &[u8; 4], tables: &[(&[u8; 4], u32, u32)]) -> Vec<u8> {
        let mut data = signature.to_vec();
        data.extend((tables.len() as u16).to_be_bytes());
        data.extend([0; 6]);
        for (tag, offset, len) in tables {
            data.extend(*tag);
            data.extend([0; 4]);
            data.extend(offset.to_be_bytes());
            data.extend(len.to_be_bytes());
        }
        data
    }

    #[test]
    fn tables_outside_the_file_are_absent_except_a_short_hmtx() {
        let mut data = font(
            b"\0\x01\0\0",
            &[
                (b"glyf", 76, 4),
                (b"hmtx", 78, 8),
                (b"name", 78, 8),
                (b"zzzz", 0, 1),
            ],
        );
        data.extend([0; 4]);
        let (flavor, tables) = directory(&data, 0, &mut Meter::new(100)).unwrap();
        assert_eq!(flavor, Flavor::TrueType);
        assert_eq!(tables.glyf.unwrap().len, 4);
        assert_eq!(tables.hmtx.unwrap().len, 2);
        assert!(tables.name.is_none());
    }

    #[test]
    fn collections_count_and_locate_faces() {
        let face = font(b"OTTO", &[(b"CFF ", 0, 4)]);
        let mut data = b"ttcf\0\x01\0\0\0\0\0\x02".to_vec();
        data.extend(20_u32.to_be_bytes());
        data.extend(20_u32.to_be_bytes());
        data.extend(&face);
        assert_eq!(face_count(&data), Ok(2));
        assert_eq!(
            directory(&data, 1, &mut Meter::new(100)).unwrap().0,
            Flavor::Cff
        );
        assert_eq!(
            directory(&data, 2, &mut Meter::new(100)).unwrap_err(),
            Error::Invalid("face index outside the font file")
        );
        assert_eq!(
            face_count(b"wOFF"),
            Err(Error::Unsupported("font file signature"))
        );
        assert!(directory(&data, 0, &mut Meter::new(0)).is_err());
    }
}
