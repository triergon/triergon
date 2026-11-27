//! The strict multibyte profiles' tables, from the Encoding Standard's indexes
//! (<https://encoding.spec.whatwg.org/>): `index-big5.txt`,
//! `index-jis0208.txt`, `index-euc-kr.txt`, `index-gb18030.txt` and
//! `index-gb18030-ranges.txt`.
//!
//! An index lists pointers and code points; each decoder of the standard
//! turns the bytes of a code into its pointer. The tables are sorted
//! big-endian records of a two-byte code and the low 16 bits of its code
//! point, codes below 0x100 standing for single bytes:
//!
//! - `big5.bin` and `big5_plane2.bin`: a lead byte 0x81 to 0xFE and a trail
//!   byte 0x40 to 0x7E or 0xA1 to 0xFE give the pointer (lead − 0x81) × 157 +
//!   (trail − offset), the offset 0x40 below 0x7F and 0x62 above; pointers
//!   1133, 1135, 1164 and 1166 are two code points each, whatever the index
//!   holds for them. The code points in plane 2 have their own table.
//! - Shift_JIS: a lead byte 0x81 to 0x9F or 0xE0 to 0xFC and a trail byte
//!   0x40 to 0x7E or 0x80 to 0xFC give (lead − offset) × 188 + (trail −
//!   offset), the lead's offset 0x81 below 0xA0 and 0xC1 above, the trail's
//!   0x40 below 0x7F and 0x41 above; pointers 8836 to 10715 are the
//!   user-defined codes U+E000 to U+E757. The single bytes 0x80 and 0xA1 to
//!   0xDF are U+0080 and U+FF61 to U+FF9F.
//! - EUC-KR: a lead byte 0x81 to 0xFE and a trail byte 0x41 to 0xFE give
//!   (lead − 0x81) × 190 + (trail − 0x41).
//! - `gbk.bin` and `gb18030_ranges.bin`: a lead byte 0x81 to 0xFE and a trail
//!   byte 0x40 to 0x7E or 0x80 to 0xFE give (lead − 0x81) × 190 + (trail −
//!   offset), the offset 0x40 below 0x7F and 0x41 above, and the single byte
//!   0x80 is U+20AC. The ranges of four-byte codes below U+10000 are records
//!   of a pointer and the code point it starts at; pointer 189000 and those
//!   above it, which the standard decodes by formula, are not listed.
//!
//! Shift_JIS and EUC-KR have no table of their own: what their indexes give
//! is code page 932's and 949's tables beyond ASCII, which their profiles
//! decode, Shift_JIS's 0x80 aside, which its decoder gives as itself. For
//! them the command checks that and writes nothing.

use std::path::Path;

use crate::files;

const TABLES: &str = "crates/subtitles/src/text/charset/mapping-data";

/// The pointers the Big5 decoder resolves to two code points before the index.
const TWO_CODE_POINTS: [u32; 4] = [1133, 1135, 1164, 1166];

/// The Shift_JIS pointers of the user-defined codes, U+E000 onwards.
const USER_DEFINED: std::ops::RangeInclusive<u32> = 8836..=10715;

/// The first four-byte GB18030 pointer the standard decodes by formula.
const SUPPLEMENTARY: u32 = 189_000;

/// The encodings the command writes, and how each turns a pointer into a code.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Big5,
    ShiftJis,
    EucKr,
    Gbk,
}

impl Encoding {
    fn of(name: &str) -> Option<Self> {
        Some(match name {
            "big5" => Self::Big5,
            "shift_jis" => Self::ShiftJis,
            "euc-kr" => Self::EucKr,
            "gbk" => Self::Gbk,
            _ => return None,
        })
    }

    /// The code of `pointer`: its lead and trail bytes.
    fn code(self, pointer: u32) -> Option<u16> {
        let (lead, trail) = match self {
            Self::Big5 => {
                let (lead, trail) = (pointer / 157, pointer % 157);
                (lead + 0x81, trail + if trail < 0x3f { 0x40 } else { 0x62 })
            }
            Self::ShiftJis => {
                let (lead, trail) = (pointer / 188, pointer % 188);
                (
                    lead + if lead < 0x1f { 0x81 } else { 0xc1 },
                    trail + if trail < 0x3f { 0x40 } else { 0x41 },
                )
            }
            Self::EucKr => (pointer / 190 + 0x81, pointer % 190 + 0x41),
            Self::Gbk => {
                let trail = pointer % 190;
                (
                    pointer / 190 + 0x81,
                    trail + if trail < 0x3f { 0x40 } else { 0x41 },
                )
            }
        };
        let top = match self {
            Self::ShiftJis => 0xfc,
            Self::Big5 | Self::EucKr | Self::Gbk => 0xfe,
        };
        (lead <= top).then(|| u16::try_from(lead << 8 | trail).ok())?
    }
}

/// Where an encoding's records are committed.
enum Committed {
    /// A table of its own, which `--write` writes.
    Own(&'static str),
    /// A code page's table, whose records beyond ASCII are the encoding's
    /// but for the single bytes its decoder gives as themselves.
    CodePage {
        table: &'static str,
        as_itself: &'static [u16],
    },
}

impl Encoding {
    fn committed(self) -> Committed {
        match self {
            Self::Big5 => Committed::Own("big5.bin"),
            Self::ShiftJis => Committed::CodePage {
                table: "windows932.bin",
                as_itself: &[0x80],
            },
            Self::EucKr => Committed::CodePage {
                table: "windows949.bin",
                as_itself: &[],
            },
            Self::Gbk => Committed::Own("gbk.bin"),
        }
    }
}

/// `subtitle-views charset-tables ENCODING INDEX [RANGES] [--write]`:
/// compares the committed tables with those the index gives, or writes them.
pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: subtitle-views charset-tables big5|shift_jis|euc-kr INDEX [--write], \
                         or gbk INDEX RANGES [--write]";
    let (write, args) = match args {
        [rest @ .., flag] if flag == "--write" => (true, rest),
        rest => (false, rest),
    };
    let (encoding, index, ranges) = match args {
        [name, index] => (
            Encoding::of(name).filter(|&e| e != Encoding::Gbk),
            index,
            None,
        ),
        [name, index, ranges] => (
            Encoding::of(name).filter(|&e| e == Encoding::Gbk),
            index,
            Some(ranges),
        ),
        _ => return Err(USAGE.into()),
    };
    let encoding = encoding.ok_or(USAGE)?;
    let read = |path: &String| {
        String::from_utf8(files::read(Path::new(path))?).map_err(|_| format!("{path}: not UTF-8"))
    };
    let tables = tables(encoding, &read(index)?)?;
    let mut outputs = Vec::new();
    let mut differ = Vec::new();
    match encoding.committed() {
        Committed::Own(name) => outputs.push((name, encode(&tables.records))),
        Committed::CodePage { table, .. } if write => {
            return Err(format!("nothing to write: the profile decodes {table}"));
        }
        Committed::CodePage { table, as_itself } => {
            let committed = files::read(&root.join(TABLES).join(table))?;
            if !beyond_ascii_equal(&tables.records, &committed, as_itself) {
                differ.push(table);
            }
        }
    }
    if let Some(plane2) = &tables.plane2 {
        outputs.push(("big5_plane2.bin", encode(plane2)));
    }
    if let Some(path) = ranges {
        outputs.push(("gb18030_ranges.bin", encode(&ranges_table(&read(path)?)?)));
    }
    for (name, bytes) in &outputs {
        let path = root.join(TABLES).join(name);
        if write {
            files::write(&path, bytes)?;
        } else if files::read(&path)? != *bytes {
            differ.push(*name);
        }
    }
    println!(
        "{index}: {} records, {} skipped{}",
        tables.records.len() + tables.plane2.as_ref().map_or(0, Vec::len),
        tables.skipped,
        tables
            .plane2
            .as_ref()
            .map_or(String::new(), |plane2| format!(
                ", {} in plane 2",
                plane2.len()
            ))
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

struct Tables {
    records: Vec<(u16, u16)>,
    /// Big5's code points in plane 2.
    plane2: Option<Vec<(u16, u16)>>,
    /// Pointers the decoder resolves before the index.
    skipped: usize,
}

/// The lines of an index: `pointer<TAB>0xCODEPOINT`, then anything; `#`
/// lines are comments. A pointer listed twice is refused.
fn lines(text: &str) -> Result<Vec<(u32, u32)>, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
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
        if !seen.insert(pointer) {
            return Err(format!("line {number}: pointer {pointer} again"));
        }
        entries.push((pointer, u32::from(value)));
    }
    Ok(entries)
}

/// The tables of `encoding` from its index.
fn tables(encoding: Encoding, text: &str) -> Result<Tables, String> {
    let mut tables = Tables {
        records: Vec::new(),
        plane2: (encoding == Encoding::Big5).then(Vec::new),
        skipped: 0,
    };
    for (pointer, value) in lines(text)? {
        let code = encoding
            .code(pointer)
            .ok_or(format!("pointer {pointer} out of range"))?;
        if encoding == Encoding::Big5 && TWO_CODE_POINTS.contains(&pointer)
            || encoding == Encoding::ShiftJis && USER_DEFINED.contains(&pointer)
        {
            tables.skipped += 1;
            continue;
        }
        let low = (value & 0xffff) as u16;
        match (value >> 16, &mut tables.plane2) {
            (0, _) => tables.records.push((code, low)),
            (2, Some(plane2)) => plane2.push((code, low)),
            (plane, _) => return Err(format!("pointer {pointer}: code point in plane {plane}")),
        }
    }
    // What the decoders give without the index.
    match encoding {
        Encoding::ShiftJis => {
            for pointer in USER_DEFINED {
                let value = 0xe000 + (pointer - USER_DEFINED.start()) as u16;
                tables
                    .records
                    .push((encoding.code(pointer).ok_or("user-defined code")?, value));
            }
            tables.records.push((0x80, 0x80));
            tables
                .records
                .extend((0xa1..=0xdf).map(|byte| (byte, 0xff61 + (byte - 0xa1))));
        }
        Encoding::Gbk => tables.records.push((0x80, 0x20ac)),
        Encoding::Big5 | Encoding::EucKr => {}
    }
    tables.records.sort_unstable();
    if let Some(plane2) = &mut tables.plane2 {
        plane2.sort_unstable();
    }
    if tables.records.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err("two code points for one code".into());
    }
    Ok(tables)
}

/// The four-byte ranges below U+10000 of `index-gb18030-ranges.txt`, in
/// pointer order.
fn ranges_table(text: &str) -> Result<Vec<(u16, u16)>, String> {
    let mut ranges = Vec::new();
    for (pointer, value) in lines(text)? {
        if pointer >= SUPPLEMENTARY {
            if (pointer, value) != (SUPPLEMENTARY, 0x1_0000) {
                return Err(format!("range {pointer} beyond the formula's start"));
            }
            continue;
        }
        let pointer = u16::try_from(pointer).map_err(|_| format!("range {pointer}"))?;
        let value = u16::try_from(value).map_err(|_| format!("range {pointer} code point"))?;
        ranges.push((pointer, value));
    }
    ranges.sort_unstable();
    Ok(ranges)
}

/// Whether `records` but those of `as_itself` are the records of `table`
/// beyond ASCII.
fn beyond_ascii_equal(records: &[(u16, u16)], table: &[u8], as_itself: &[u16]) -> bool {
    let (table, rest) = table.as_chunks::<4>();
    let table = table
        .iter()
        .map(|&[high, low, value_high, value_low]| {
            (
                u16::from_be_bytes([high, low]),
                u16::from_be_bytes([value_high, value_low]),
            )
        })
        .filter(|&(code, _)| code >= 0x80);
    rest.is_empty()
        && table.eq(records
            .iter()
            .copied()
            .filter(|(code, _)| !as_itself.contains(code)))
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
    use super::{Encoding, TWO_CODE_POINTS, beyond_ascii_equal, encode, ranges_table, tables};

    /// The pointer of a code, as each decoder of the standard computes it;
    /// `None` for bytes it does not take as a lead and a trail.
    fn pointer(encoding: Encoding, code: u16) -> Option<u32> {
        let [lead, trail] = code.to_be_bytes().map(u32::from);
        match encoding {
            Encoding::Big5 => {
                let offset = match trail {
                    0x40..=0x7e => 0x40,
                    0xa1..=0xfe => 0x62,
                    _ => return None,
                };
                (0x81..=0xfe)
                    .contains(&lead)
                    .then(|| (lead - 0x81) * 157 + trail - offset)
            }
            Encoding::ShiftJis => {
                let lead = match lead {
                    0x81..=0x9f => lead - 0x81,
                    0xe0..=0xfc => lead - 0xc1,
                    _ => return None,
                };
                let offset = match trail {
                    0x40..=0x7e => 0x40,
                    0x80..=0xfc => 0x41,
                    _ => return None,
                };
                Some(lead * 188 + trail - offset)
            }
            Encoding::EucKr => ((0x81..=0xfe).contains(&lead) && (0x41..=0xfe).contains(&trail))
                .then(|| (lead - 0x81) * 190 + trail - 0x41),
            Encoding::Gbk => {
                let offset = match trail {
                    0x40..=0x7e => 0x40,
                    0x80..=0xfe => 0x41,
                    _ => return None,
                };
                (0x81..=0xfe)
                    .contains(&lead)
                    .then(|| (lead - 0x81) * 190 + trail - offset)
            }
        }
    }

    #[test]
    fn every_pointer_is_the_code_the_decoder_reads_back() {
        for (encoding, pointers) in [
            (Encoding::Big5, 126 * 157),
            (Encoding::ShiftJis, 60 * 188),
            (Encoding::EucKr, 126 * 190),
            (Encoding::Gbk, 126 * 190),
        ] {
            for p in 0..pointers {
                let c = encoding.code(p).unwrap();
                assert_eq!(pointer(encoding, c), Some(p), "{c:04X}");
            }
            assert_eq!(encoding.code(pointers), None);
        }
        // The standard's own examples of the formula: the two-code-point rows.
        assert_eq!(
            TWO_CODE_POINTS.map(|p| Encoding::Big5.code(p).unwrap()),
            [0x8862, 0x8864, 0x88a3, 0x88a5]
        );
        assert_eq!(pointer(Encoding::Big5, 0xfefe), Some(126 * 157 - 1));
        assert_eq!(pointer(Encoding::ShiftJis, 0xf040), Some(8836));
        assert_eq!(pointer(Encoding::ShiftJis, 0xf9fc), Some(10715));
        assert_eq!(
            [
                pointer(Encoding::Big5, 0x807e),
                pointer(Encoding::ShiftJis, 0xa040),
                pointer(Encoding::EucKr, 0x8140),
                pointer(Encoding::Gbk, 0x817f),
            ],
            [None; 4]
        );
    }

    #[test]
    fn lines_become_sorted_records_by_plane() {
        let index = "# comment\n\n10896\t0x2460\t① (CIRCLED DIGIT ONE)\n942\t0x43F0\t䏰\n\
                     943\t0x4C32\t䰲\n1133\t0x00CA\tÊ\n947\t0x27267\t𧉧\n";
        let tables = tables(Encoding::Big5, index).unwrap();
        assert_eq!(
            tables.records,
            [(0x8740, 0x43f0), (0x8741, 0x4c32), (0xc6a1, 0x2460)]
        );
        assert_eq!(tables.plane2.as_deref(), Some(&[(0x8745, 0x7267)][..]));
        assert_eq!(tables.skipped, 1);
        assert_eq!(encode(&tables.records[..1]), [0x87, 0x40, 0x43, 0xf0]);
    }

    #[test]
    fn decoders_add_what_they_give_without_the_index() {
        let shift_jis = tables(Encoding::ShiftJis, "0\t0x3000\n").unwrap();
        assert_eq!(shift_jis.records.len(), 1 + 1880 + 1 + 63);
        assert_eq!(shift_jis.records[0], (0x80, 0x80));
        assert_eq!(shift_jis.records[1], (0xa1, 0xff61));
        assert!(shift_jis.records.contains(&(0x8140, 0x3000)));
        assert!(shift_jis.records.contains(&(0xf040, 0xe000)));
        assert!(shift_jis.records.contains(&(0xf9fc, 0xe757)));
        let gbk = tables(Encoding::Gbk, "0\t0x4E02\n").unwrap();
        assert_eq!(gbk.records, [(0x80, 0x20ac), (0x8140, 0x4e02)]);
        let euc_kr = tables(Encoding::EucKr, "0\t0xAC02\n").unwrap();
        assert_eq!(euc_kr.records, [(0x8141, 0xac02)]);
    }

    #[test]
    fn a_code_page_is_compared_beyond_ascii_and_the_bytes_given_as_themselves() {
        let records = [(0x80, 0x80), (0xa1, 0xff61), (0x8140, 0x3000)];
        let table = encode(&[(0x41, 0x41), (0xa1, 0xff61), (0x8140, 0x3000)]);
        assert!(beyond_ascii_equal(&records, &table, &[0x80]));
        assert!(!beyond_ascii_equal(&records, &table, &[]));
        assert!(!beyond_ascii_equal(&records[1..2], &table, &[]));
        assert!(!beyond_ascii_equal(
            &records[1..],
            &table[..table.len() - 1],
            &[]
        ));
    }

    #[test]
    fn ranges_stop_at_the_formula() {
        let ranges = ranges_table("36\t0x00A5\n0\t0x0080\n189000\t0x10000\n").unwrap();
        assert_eq!(ranges, [(0, 0x80), (36, 0xa5)]);
        assert!(ranges_table("189001\t0x10001\n").is_err());
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
            assert!(tables(Encoding::Big5, index).is_err(), "{index:?}");
        }
        assert!(tables(Encoding::ShiftJis, "11280\t0x3000\n").is_err());
        assert!(tables(Encoding::EucKr, "0\t0x20000\n").is_err());
    }
}
