//! CFF tables built in memory: INDEXes, DICT numbers, charstrings and a
//! name-keyed font program. The crate's own tests include this file too, so it
//! uses nothing from outside it.

#![allow(dead_code)]

/// A CFF number with a fixed five-byte encoding, so offsets do not move.
pub(crate) fn long(value: i32) -> Vec<u8> {
    let mut v = vec![29];
    v.extend(value.to_be_bytes());
    v
}

pub(crate) fn index(objects: &[Vec<u8>]) -> Vec<u8> {
    if objects.is_empty() {
        return vec![0, 0];
    }
    let mut out = (objects.len() as u16).to_be_bytes().to_vec();
    out.push(4);
    let mut offset = 1_u32;
    out.extend(offset.to_be_bytes());
    for object in objects {
        offset += object.len() as u32;
        out.extend(offset.to_be_bytes());
    }
    for object in objects {
        out.extend(object);
    }
    out
}

/// A one-byte charstring number (-107..=107).
pub(crate) fn n(value: i16) -> u8 {
    (value + 139) as u8
}

/// A CFF number written as a real (TN 5176 Table 5), from its decimal text.
pub(crate) fn real(text: &str) -> Vec<u8> {
    let mut nibbles: Vec<u8> = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        nibbles.push(match c {
            '0'..='9' => c as u8 - b'0',
            '.' => 0xa,
            'E' if chars.peek() == Some(&'-') => {
                chars.next();
                0xc
            }
            'E' => 0xb,
            '-' => 0xe,
            _ => panic!("not a real: {text}"),
        });
    }
    nibbles.push(0xf);
    if nibbles.len() % 2 == 1 {
        nibbles.push(0xf);
    }
    let mut out = vec![30];
    out.extend(nibbles.chunks(2).map(|pair| pair[0] << 4 | pair[1]));
    out
}

/// The parts of a name-keyed CFF table.
pub(crate) struct CffProgram {
    pub(crate) glyphs: Vec<Vec<u8>>,
    pub(crate) global: Vec<Vec<u8>>,
    pub(crate) local: Vec<Vec<u8>>,
    /// The SID of each glyph after .notdef, as a format 0 charset; the ISOAdobe
    /// charset when absent.
    pub(crate) charset: Option<Vec<u16>>,
    /// The string INDEX: the names of SIDs from 391.
    pub(crate) strings: Vec<Vec<u8>>,
    /// An encoding table; the Standard encoding when absent.
    pub(crate) encoding: Option<Vec<u8>>,
    /// FontMatrix entries as decimal text.
    pub(crate) matrix: Option<[&'static str; 6]>,
    pub(crate) nominal_width: i32,
    /// More Private DICT entries, encoded, such as blue zones.
    pub(crate) private: Vec<u8>,
}

impl CffProgram {
    /// The CFF table. Offsets use the five-byte integer form, so the layout is
    /// computed once with placeholders and once for real.
    pub(crate) fn build(&self) -> Vec<u8> {
        let private = |subrs: i32| {
            let mut p = long(self.nominal_width);
            p.push(21); // nominalWidthX
            if !self.local.is_empty() {
                p.extend(long(subrs));
                p.push(19); // Subrs, relative to the Private DICT
            }
            p.extend(&self.private);
            p
        };
        let private_len = private(0).len();
        let top = |charstrings: i32, charset: i32, encoding: i32, private_offset: i32| {
            let mut t = long(charstrings);
            t.push(17);
            if self.charset.is_some() {
                t.extend(long(charset));
                t.push(15);
            }
            if self.encoding.is_some() {
                t.extend(long(encoding));
                t.push(16);
            }
            if let Some(matrix) = self.matrix {
                for value in matrix {
                    t.extend(real(value));
                }
                t.extend([12, 7]);
            }
            t.extend(long(private_len as i32));
            t.extend(long(private_offset));
            t.push(18);
            t
        };
        let charset = self.charset.as_ref().map(|sids| {
            let mut c = vec![0];
            c.extend(sids.iter().flat_map(|sid| sid.to_be_bytes()));
            c
        });
        let header = vec![1, 0, 4, 4];
        let names = index(&[b"Test".to_vec()]);
        let top_len = index(&[top(0, 0, 0, 0)]).len();
        let strings = index(&self.strings);
        let global = index(&self.global);
        let charstrings_at = header.len() + names.len() + top_len + strings.len() + global.len();
        let charstrings = index(&self.glyphs);
        let charset_at = charstrings_at + charstrings.len();
        let encoding_at = charset_at + charset.as_ref().map_or(0, Vec::len);
        let private_at = encoding_at + self.encoding.as_ref().map_or(0, Vec::len);
        let mut out = header;
        out.extend(names);
        out.extend(index(&[top(
            charstrings_at as i32,
            charset_at as i32,
            encoding_at as i32,
            private_at as i32,
        )]));
        out.extend(strings);
        out.extend(global);
        out.extend(charstrings);
        out.extend(charset.unwrap_or_default());
        out.extend(self.encoding.clone().unwrap_or_default());
        out.extend(private(private_len as i32));
        if !self.local.is_empty() {
            out.extend(index(&self.local));
        }
        out
    }
}

/// A two-byte charstring number (108..=1131 or -1131..=-108), or one byte inside
/// -107..=107.
pub(crate) fn number(value: i16) -> Vec<u8> {
    match value {
        -107..=107 => vec![n(value)],
        108..=1131 => {
            let v = value - 108;
            vec![(v / 256 + 247) as u8, (v % 256) as u8]
        }
        -1131..=-108 => {
            let v = -value - 108;
            vec![(v / 256 + 251) as u8, (v % 256) as u8]
        }
        _ => {
            let mut b = vec![28];
            b.extend(value.to_be_bytes());
            b
        }
    }
}

/// A charstring from numbers and operator bytes.
pub(crate) fn charstring(parts: &[Part]) -> Vec<u8> {
    parts
        .iter()
        .flat_map(|part| match *part {
            Part::N(v) => number(v),
            Part::Op(op) => vec![op],
            Part::Escape(op) => vec![12, op],
            Part::Byte(byte) => vec![byte],
        })
        .collect()
}

/// One element of an authored charstring.
#[derive(Clone, Copy)]
pub(crate) enum Part {
    N(i16),
    Op(u8),
    Escape(u8),
    /// A raw byte, such as a hint mask.
    Byte(u8),
}
