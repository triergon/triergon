//! The strict Big5 profile's tables, from the Encoding Standard's
//! `index-big5.txt` (<https://encoding.spec.whatwg.org/index-big5.txt>).
//!
//! The index lists pointers and code points; the Big5 decoder of the
//! standard turns a lead byte 0x81 to 0xFE and a trail byte 0x40 to 0x7E or
//! 0xA1 to 0xFE into the pointer (lead − 0x81) × 157 + (trail − offset), where
//! the offset is 0x40 below 0x7F and 0x62 above, and decodes pointers 1133,
//! 1135, 1164 and 1166 as two code points each, whatever the index holds for
//! them. The tables are sorted big-endian records of a two-byte code and the
//! low 16 bits of its code point: `big5.bin` for the code points below
//! U+10000 and `big5_plane2.bin` for those in plane 2, which holds every other
//! code point of the index.

use std::path::Path;

use crate::files;

const TABLES: &str = "crates/subtitles/src/text/charset/mapping-data";

/// Lead bytes 0x81 to 0xFE, 157 trail bytes each.
const POINTERS: u32 = 126 * 157;

/// The pointers the decoder resolves to two code points before the index.
const TWO_CODE_POINTS: [u32; 4] = [1133, 1135, 1164, 1166];

/// `subtitle-views charset-tables big5 INDEX [--write]`: compares the committed
/// tables with those `INDEX` gives, or writes them.
pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    let (index, write) = match args {
        [encoding, index] if encoding == "big5" => (index, false),
        [encoding, index, flag] if encoding == "big5" && flag == "--write" => (index, true),
        _ => return Err("usage: subtitle-views charset-tables big5 INDEX [--write]".into()),
    };
    let text = String::from_utf8(files::read(Path::new(index))?)
        .map_err(|_| format!("{index}: not UTF-8"))?;
    let tables = tables(&text)?;
    let mut differ = Vec::new();
    for (name, records) in [
        ("big5.bin", &tables.bmp),
        ("big5_plane2.bin", &tables.plane2),
    ] {
        let path = root.join(TABLES).join(name);
        let bytes = encode(records);
        if write {
            files::write(&path, &bytes)?;
        } else if files::read(&path)? != bytes {
            differ.push(name);
        }
    }
    println!(
        "index-big5: {} pointers, {} below U+10000, {} in plane 2, {} skipped as two code points",
        tables.bmp.len() + tables.plane2.len() + tables.skipped,
        tables.bmp.len(),
        tables.plane2.len(),
        tables.skipped
    );
    if differ.is_empty() {
        println!(
            "{}",
            if write {
                "written"
            } else {
                "the committed tables equal the index"
            }
        );
        Ok(())
    } else {
        Err(format!("differs from the index: {}", differ.join(", ")))
    }
}

/// The Big5 code of `pointer`: its lead and trail bytes.
fn code(pointer: u32) -> Option<u16> {
    if pointer >= POINTERS {
        return None;
    }
    let (lead, trail) = (pointer / 157 + 0x81, pointer % 157);
    let trail = trail + if trail < 0x3f { 0x40 } else { 0x62 };
    u16::try_from(lead << 8 | trail).ok()
}

struct Tables {
    bmp: Vec<(u16, u16)>,
    plane2: Vec<(u16, u16)>,
    skipped: usize,
}

/// The records of every line of the index: `pointer<TAB>0xCODEPOINT`, then
/// anything; `#` lines are comments.
fn tables(text: &str) -> Result<Tables, String> {
    let mut tables = Tables {
        bmp: Vec::new(),
        plane2: Vec::new(),
        skipped: 0,
    };
    let mut seen = vec![false; POINTERS as usize];
    for (number, line) in (1..).zip(text.lines()) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(pointer), Some(value)) = (fields.next(), fields.next()) else {
            return Err(format!(
                "line {number}: expected a pointer and a code point"
            ));
        };
        let pointer: u32 = pointer
            .trim()
            .parse()
            .map_err(|_| format!("line {number}: pointer {pointer:?}"))?;
        let value = value
            .trim()
            .strip_prefix("0x")
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .and_then(char::from_u32)
            .ok_or(format!("line {number}: code point {value:?}"))?;
        let code = code(pointer).ok_or(format!("line {number}: pointer {pointer} out of range"))?;
        if std::mem::replace(&mut seen[pointer as usize], true) {
            return Err(format!("line {number}: pointer {pointer} again"));
        }
        if TWO_CODE_POINTS.contains(&pointer) {
            tables.skipped += 1;
            continue;
        }
        let value = u32::from(value);
        let low = (value & 0xffff) as u16;
        match value >> 16 {
            0 => tables.bmp.push((code, low)),
            2 => tables.plane2.push((code, low)),
            plane => return Err(format!("line {number}: code point in plane {plane}")),
        }
    }
    tables.bmp.sort_unstable();
    tables.plane2.sort_unstable();
    Ok(tables)
}

fn encode(records: &[(u16, u16)]) -> Vec<u8> {
    records
        .iter()
        .flat_map(|&(code, value)| [code.to_be_bytes(), value.to_be_bytes()])
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{POINTERS, TWO_CODE_POINTS, code, encode, tables};

    /// The pointer of a Big5 code, as the decoder computes it; `None` for bytes it
    /// does not take as a lead and a trail.
    fn pointer(code: u16) -> Option<u32> {
        let [lead, trail] = code.to_be_bytes();
        let offset = match trail {
            0x40..=0x7e => 0x40,
            0xa1..=0xfe => 0x62,
            _ => return None,
        };
        (0x81..=0xfe)
            .contains(&lead)
            .then(|| u32::from(lead - 0x81) * 157 + u32::from(trail - offset))
    }

    #[test]
    fn every_pointer_is_the_code_the_decoder_reads_back() {
        for p in 0..POINTERS {
            let c = code(p).unwrap();
            assert_eq!(pointer(c), Some(p), "{c:04X}");
        }
        assert_eq!(code(POINTERS), None);
        // The standard's own examples of the formula: the two-code-point rows.
        assert_eq!(
            TWO_CODE_POINTS.map(|p| code(p).unwrap()),
            [0x8862, 0x8864, 0x88a3, 0x88a5]
        );
        assert_eq!(pointer(0x8140), Some(0));
        assert_eq!(pointer(0xfefe), Some(POINTERS - 1));
        assert_eq!(
            [pointer(0x807e), pointer(0x8180), pointer(0x81a0)],
            [None; 3]
        );
    }

    #[test]
    fn lines_become_sorted_records_by_plane() {
        let index = "# comment\n\n10896\t0x2460\t① (CIRCLED DIGIT ONE)\n942\t0x43F0\t䏰\n\
                     943\t0x4C32\t䰲\n1133\t0x00CA\tÊ\n947\t0x27267\t𧉧\n";
        let tables = tables(index).unwrap();
        assert_eq!(
            tables.bmp,
            [(0x8740, 0x43f0), (0x8741, 0x4c32), (0xc6a1, 0x2460)]
        );
        assert_eq!(tables.plane2, [(0x8745, 0x7267)]);
        assert_eq!(tables.skipped, 1);
        assert_eq!(encode(&tables.plane2), [0x87, 0x45, 0x72, 0x67]);
    }

    #[test]
    fn malformed_lines_are_refused() {
        for index in [
            "942\n",
            "x\t0x43F0\n",
            "942\t43F0\n",
            "942\t0xD800\n",
            "19782\t0x43F0\n",
            "942\t0x43F0\n942\t0x43F1\n",
            "942\t0x10000\n",
        ] {
            assert!(tables(index).is_err(), "{index:?}");
        }
    }
}
