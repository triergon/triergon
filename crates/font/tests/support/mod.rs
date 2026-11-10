//! Small fonts built in memory, and the operations the robustness test runs on them.

#![allow(dead_code)]

use triergon_core::{Budget, Bytes};
use triergon_font::{FaceInfo, Font, GlyphId, Limits, Outline, Request, select};

pub fn u16s(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

pub fn i16s(values: &[i16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// The tables of one face.
pub type Tables = Vec<([u8; 4], Vec<u8>)>;

/// An sfnt directory at `base` followed by its tables, which start after the
/// directories of all faces in the file (`directories` bytes in total).
pub fn face(signature: &[u8; 4], tables: &Tables, table_start: usize) -> (Vec<u8>, Vec<u8>) {
    let mut sorted = tables.clone();
    sorted.sort_by_key(|(tag, _)| *tag);
    let mut directory = signature.to_vec();
    directory.extend(u16s(&[sorted.len() as u16, 0, 0, 0]));
    let mut data = Vec::new();
    for (tag, bytes) in &sorted {
        directory.extend(tag);
        directory.extend([0; 4]);
        directory.extend(((table_start + data.len()) as u32).to_be_bytes());
        directory.extend((bytes.len() as u32).to_be_bytes());
        data.extend(bytes);
        while data.len() % 4 != 0 {
            data.push(0);
        }
    }
    (directory, data)
}

pub fn font(signature: &[u8; 4], tables: &Tables) -> Vec<u8> {
    let header = 12 + 16 * tables.len();
    let (mut directory, data) = face(signature, tables, header);
    directory.extend(data);
    directory
}

/// A collection of faces, each given as (signature, tables).
pub fn collection(faces: &[(&[u8; 4], Tables)]) -> Vec<u8> {
    let header = 12 + 4 * faces.len();
    let directories: Vec<usize> = faces.iter().map(|(_, t)| 12 + 16 * t.len()).collect();
    let mut offsets = Vec::new();
    let mut at = header;
    for len in &directories {
        offsets.push(at);
        at += len;
    }
    let mut data_start = at;
    let mut out = b"ttcf\0\x01\0\0".to_vec();
    out.extend((faces.len() as u32).to_be_bytes());
    for offset in &offsets {
        out.extend((*offset as u32).to_be_bytes());
    }
    let mut tables = Vec::new();
    for (signature, face_tables) in faces {
        let (directory, data) = face(signature, face_tables, data_start);
        out.extend(directory);
        data_start += data.len();
        tables.extend(data);
    }
    out.extend(tables);
    out
}

pub fn head(units: u16, long: bool, mac_style: u16) -> Vec<u8> {
    let mut t = 0x0001_0000_u32.to_be_bytes().to_vec();
    t.extend([0; 8]);
    t.extend(0x5f0f_3cf5_u32.to_be_bytes());
    t.extend(u16s(&[0, units]));
    t.extend([0; 16]);
    t.extend(i16s(&[0, -200, 1000, 800]));
    t.extend(u16s(&[mac_style, 8, 2, u16::from(long), 0]));
    t
}

pub fn hhea(metrics: u16) -> Vec<u8> {
    let mut t = 0x0001_0000_u32.to_be_bytes().to_vec();
    t.extend(i16s(&[800, -200, 90]));
    t.extend(u16s(&[1000]));
    t.extend([0; 22]);
    t.extend(u16s(&[metrics]));
    t
}

pub fn maxp(glyphs: u16) -> Vec<u8> {
    let mut t = 0x0000_5000_u32.to_be_bytes().to_vec();
    t.extend(u16s(&[glyphs]));
    t
}

pub fn os2(weight: u16, fs_selection: u16) -> Vec<u8> {
    let mut t = vec![0; 96];
    t[0..2].copy_from_slice(&4_u16.to_be_bytes());
    t[4..6].copy_from_slice(&weight.to_be_bytes());
    t[6..8].copy_from_slice(&5_u16.to_be_bytes());
    t[62..64].copy_from_slice(&fs_selection.to_be_bytes());
    t[68..78].copy_from_slice(&[0x03, 0x52, 0xff, 0x38, 0, 0x5a, 0x04, 0x00, 0x01, 0x00]);
    t
}

/// (advance, left side bearing) per glyph.
pub fn hmtx(metrics: &[(u16, i16)]) -> Vec<u8> {
    metrics
        .iter()
        .flat_map(|&(a, l)| [a.to_be_bytes(), l.to_be_bytes()].concat())
        .collect()
}

/// A Windows Unicode format 4 cmap mapping each (code, glyph).
pub fn cmap(pairs: &[(u16, u16)]) -> Vec<u8> {
    let segments = pairs.len() + 1;
    let mut ends: Vec<u16> = pairs.iter().map(|p| p.0).collect();
    ends.push(0xffff);
    let starts = ends.clone();
    let mut deltas: Vec<u16> = pairs.iter().map(|&(c, g)| g.wrapping_sub(c)).collect();
    deltas.push(1);
    let mut sub = u16s(&[4, 0, 0, 2 * segments as u16, 0, 0, 0]);
    sub.extend(u16s(&ends));
    sub.extend(u16s(&[0]));
    sub.extend(u16s(&starts));
    sub.extend(u16s(&deltas));
    sub.extend(u16s(&vec![0; segments]));
    let mut t = u16s(&[0, 1, 3, 1]);
    t.extend(12_u32.to_be_bytes());
    t.extend(sub);
    t
}

/// Windows (3, 1, 0x409) and Macintosh Roman records for each (id, text).
pub fn name(records: &[(u16, &str)]) -> Vec<u8> {
    let mut storage = Vec::new();
    let mut entries = Vec::new();
    for &(id, text) in records {
        let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        entries.push([3, 1, 0x409, id, utf16.len() as u16, storage.len() as u16]);
        storage.extend(utf16);
    }
    for &(id, text) in records {
        entries.push([1, 0, 0, id, text.len() as u16, storage.len() as u16]);
        storage.extend(text.as_bytes());
    }
    let mut t = u16s(&[0, entries.len() as u16, 6 + 12 * entries.len() as u16]);
    for entry in entries {
        t.extend(u16s(&entry));
    }
    t.extend(storage);
    t
}

pub fn kern(pairs: &[(u16, u16, i16)]) -> Vec<u8> {
    let mut t = u16s(&[
        0,
        1,
        0,
        14 + 6 * pairs.len() as u16,
        1,
        pairs.len() as u16,
        0,
        0,
        0,
    ]);
    for &(l, r, v) in pairs {
        t.extend(u16s(&[l, r]));
        t.extend(v.to_be_bytes());
    }
    t
}

/// glyf and short loca for the given glyph records.
pub fn glyf(glyphs: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
    let mut glyf = Vec::new();
    let mut loca = u16s(&[0]);
    for glyph in glyphs {
        glyf.extend(glyph);
        if glyf.len() % 2 == 1 {
            glyf.push(0);
        }
        loca.extend(u16s(&[(glyf.len() / 2) as u16]));
    }
    (glyf, loca)
}

/// A simple glyph from contours of (x, y, on-curve) points, all coordinates as
/// 16-bit deltas.
pub fn simple(contours: &[&[(i16, i16, bool)]]) -> Vec<u8> {
    let points: Vec<_> = contours.iter().flat_map(|c| c.iter().copied()).collect();
    let xs: Vec<i16> = points.iter().map(|p| p.0).collect();
    let ys: Vec<i16> = points.iter().map(|p| p.1).collect();
    let bounds = [
        *xs.iter().min().unwrap_or(&0),
        *ys.iter().min().unwrap_or(&0),
        *xs.iter().max().unwrap_or(&0),
        *ys.iter().max().unwrap_or(&0),
    ];
    let mut g = i16s(&[contours.len() as i16]);
    g.extend(i16s(&bounds));
    let mut end = 0;
    for contour in contours {
        end += contour.len();
        g.extend(u16s(&[(end - 1) as u16]));
    }
    g.extend(u16s(&[0]));
    for p in &points {
        g.push(u8::from(p.2));
    }
    let mut previous = (0, 0);
    for p in &points {
        g.extend(i16s(&[p.0 - previous.0]));
        previous.0 = p.0;
    }
    for p in &points {
        g.extend(i16s(&[p.1 - previous.1]));
        previous.1 = p.1;
    }
    g
}

/// A composite of (flags, glyph, dx, dy, scale) components with word arguments.
pub fn composite(components: &[(u16, u16, i16, i16, Option<f64>)]) -> Vec<u8> {
    let mut g = i16s(&[-1, 0, 0, 0, 0]);
    for (i, &(flags, glyph, dx, dy, scale)) in components.iter().enumerate() {
        let more = if i + 1 < components.len() { 0x20 } else { 0 };
        let scaled = if scale.is_some() { 0x08 } else { 0 };
        g.extend(u16s(&[flags | more | scaled | 0x03, glyph]));
        g.extend(i16s(&[dx, dy]));
        if let Some(s) = scale {
            g.extend(i16s(&[(s * 16384.0) as i16]));
        }
    }
    g
}

/// A TrueType face: glyph 1 a square at x 100..300 with left side bearing 50,
/// glyph 2 a quadratic bump, glyph 3 glyph 1 at half size offset by (10, 20) plus
/// glyph 2 with its metrics. 'A' maps to 1, 'B' to 2, 'C' to 3.
pub fn truetype_tables(weight: u16, fs_selection: u16, family: &str, subfamily: &str) -> Tables {
    let square: &[(i16, i16, bool)] = &[
        (100, 0, true),
        (100, 200, true),
        (300, 200, true),
        (300, 0, true),
    ];
    let bump: &[(i16, i16, bool)] = &[(0, 0, true), (50, 100, false), (100, 0, true)];
    let (glyf, loca) = glyf(&[
        Vec::new(),
        simple(&[square]),
        simple(&[bump]),
        composite(&[(0, 1, 10, 20, Some(0.5)), (0x0200, 2, 0, 0, None)]),
    ]);
    let full = format!("{family} {subfamily}");
    let postscript = format!("{family}-{subfamily}").replace(' ', "");
    vec![
        (*b"head", head(1000, false, 0)),
        (*b"hhea", hhea(4)),
        (*b"maxp", maxp(4)),
        (*b"OS/2", os2(weight, fs_selection)),
        (*b"hmtx", hmtx(&[(500, 0), (400, 50), (300, 0), (600, 0)])),
        (*b"cmap", cmap(&[(0x41, 1), (0x42, 2), (0x43, 3)])),
        (
            *b"name",
            name(&[(1, family), (2, subfamily), (4, &full), (6, &postscript)]),
        ),
        (*b"kern", kern(&[(1, 2, -40)])),
        (*b"glyf", glyf),
        (*b"loca", loca),
    ]
}

/// A CFF number with a fixed five-byte encoding, so offsets do not move.
fn long(value: i32) -> Vec<u8> {
    let mut v = vec![29];
    v.extend(value.to_be_bytes());
    v
}

fn index(objects: &[Vec<u8>]) -> Vec<u8> {
    if objects.is_empty() {
        return vec![0, 0];
    }
    let mut out = u16s(&[objects.len() as u16]);
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
fn n(value: i16) -> u8 {
    (value + 139) as u8
}

/// A CFF number written as a real (TN 5176 Table 5), from its decimal text.
fn real(text: &str) -> Vec<u8> {
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
pub struct CffProgram {
    pub glyphs: Vec<Vec<u8>>,
    pub global: Vec<Vec<u8>>,
    pub local: Vec<Vec<u8>>,
    /// The SID of each glyph after .notdef, as a format 0 charset; the ISOAdobe
    /// charset when absent.
    pub charset: Option<Vec<u16>>,
    /// The string INDEX: the names of SIDs from 391.
    pub strings: Vec<Vec<u8>>,
    /// An encoding table; the Standard encoding when absent.
    pub encoding: Option<Vec<u8>>,
    /// FontMatrix entries as decimal text.
    pub matrix: Option<[&'static str; 6]>,
    pub nominal_width: i32,
}

impl CffProgram {
    /// The CFF table. Offsets use the five-byte integer form, so the layout is
    /// computed once with placeholders and once for real.
    pub fn build(&self) -> Vec<u8> {
        let private = |subrs: i32| {
            let mut p = long(self.nominal_width);
            p.push(21); // nominalWidthX
            if !self.local.is_empty() {
                p.extend(long(subrs));
                p.push(19); // Subrs, relative to the Private DICT
            }
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
            c.extend(u16s(sids));
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

/// A CFF table: glyph 1 a square drawn with a width, hints, a hintmask and a local
/// subroutine; glyph 2 a curve through a global subroutine and a flex.
pub fn cff_table() -> Vec<u8> {
    CffProgram {
        // Local subr 0 (biased -107): vlineto 100; return.
        local: vec![vec![n(100), 7, 11]],
        // Global subr 0: rrcurveto 10 20 30 0 20 -20; return.
        global: vec![vec![n(10), n(20), n(30), n(0), n(20), n(-20), 8, 11]],
        glyphs: vec![
            vec![14],
            // width 10 (nominal 400), hstem 0 20, vstem 0 20, hintmask, rmoveto 50 0,
            // hlineto 100, callsubr -107, hlineto -100, endchar.
            vec![
                n(10),
                n(0),
                n(20),
                1,
                n(0),
                n(20),
                3,
                19,
                0xc0,
                n(50),
                n(0),
                21,
                n(100),
                6,
                n(-107),
                10,
                n(-100),
                6,
                14,
            ],
            // rmoveto 0 0, callgsubr -107, rlineto 0 0 (dropped), hflex, endchar.
            vec![
                n(0),
                n(0),
                21,
                n(-107),
                29,
                n(0),
                n(0),
                5,
                n(10),
                n(5),
                n(5),
                n(10),
                n(10),
                n(5),
                n(10),
                12,
                34,
                14,
            ],
        ],
        charset: None,
        strings: Vec::new(),
        encoding: None,
        matrix: None,
        nominal_width: 400,
    }
    .build()
}

/// A two-byte charstring number (108..=1131 or -1131..=-108), or one byte inside
/// -107..=107.
pub fn number(value: i16) -> Vec<u8> {
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
fn charstring(parts: &[Part]) -> Vec<u8> {
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
enum Part {
    N(i16),
    Op(u8),
    Escape(u8),
    /// A raw byte, such as a hint mask.
    Byte(u8),
}

/// A CFF face exercising `seac`, arithmetic (`div`, `mul`, `add`, `sub`, `neg`,
/// `abs`, `exch`, `dup`, `index`, `roll`, `put`, `get`, `ifelse`), subroutines
/// nested twelve deep, past TN 5177's ten, and the flex, counter-mask, logic and
/// `sqrt` operators. Glyphs: .notdef, A, acute, D (A with an acute through
/// `seac`), B (arithmetic), C (nested calls), E (flex and logic).
fn cff_features_tables() -> Tables {
    use Part::{Byte, Escape as E, N, Op};
    let glyphs = vec![
        charstring(&[Op(14)]),
        // A: width 600; a triangle.
        charstring(&[
            N(100),
            N(100),
            N(0),
            Op(21),
            N(200),
            N(600),
            N(200),
            N(-600),
            Op(5),
            Op(14),
        ]),
        // acute: a quadrilateral above.
        charstring(&[
            N(250),
            N(650),
            Op(21),
            N(100),
            N(100),
            N(-150),
            N(0),
            Op(5),
            Op(14),
        ]),
        // D: seac with the acute offset by (50, 20): adx ady bchar achar.
        charstring(&[N(50), N(20), N(65), N(194), Op(14)]),
        // B: coordinates from arithmetic.
        charstring(&[
            N(100),
            N(100),
            Op(21),
            N(600),
            N(4),
            E(12),
            N(0),
            Op(5), // 600/4 = 150
            N(100),
            N(3),
            E(12),
            N(50),
            Op(5), // 33.33.., 50
            N(7),
            N(-9),
            E(24),
            E(9),
            N(5),
            E(14),
            E(10),
            N(0),
            E(28),
            Op(5), // |7*-9| + -5, exch
            N(10),
            E(27),
            E(10),
            N(-20),
            Op(5), // dup add: 20, -20
            N(3),
            N(0),
            E(20),
            N(0),
            E(21),
            N(2),
            N(1),
            N(4),
            N(3),
            E(22),
            N(9),
            E(11),
            Op(5), // put/get/ifelse/sub
            N(11),
            N(22),
            N(33),
            N(3),
            N(1),
            E(30),
            N(1),
            E(29),
            E(18),
            E(18),
            Op(5), // roll, index, drop
            Op(14),
        ]),
        // C: twelve nested local subroutines, each drawing a line after its call.
        charstring(&[N(100), N(100), Op(21), N(-107), Op(10), Op(14)]),
        // E: two stems and a counter mask over both, dotsection, the four flex
        // forms, then a line whose dx is ((1 and 1) + (0 or 1) + not 0 + (2 eq 2))
        // * 25 = 100 and whose dy is sqrt 81 = 9.
        charstring(&[
            N(0),
            N(50),
            N(100),
            N(50),
            Op(1),
            Op(20),
            Byte(0xc0),
            E(0),
            N(100),
            N(100),
            Op(21),
            N(50),
            N(50),
            N(20),
            N(50),
            N(50),
            N(50),
            N(50),
            E(34),
            N(30),
            N(10),
            N(30),
            N(10),
            N(30),
            N(0),
            N(30),
            N(0),
            N(30),
            N(-10),
            N(30),
            N(-10),
            N(50),
            E(35),
            N(20),
            N(10),
            N(20),
            N(10),
            N(20),
            N(20),
            N(20),
            N(-10),
            N(20),
            E(36),
            N(10),
            N(20),
            N(10),
            N(20),
            N(10),
            N(20),
            N(10),
            N(-20),
            N(10),
            N(-20),
            N(10),
            E(37),
            N(1),
            N(1),
            E(3),
            N(0),
            N(1),
            E(4),
            E(10),
            N(0),
            E(5),
            E(10),
            N(2),
            N(2),
            E(15),
            E(10),
            N(25),
            E(24),
            N(81),
            E(26),
            Op(5),
            N(-150),
            Op(7),
            Op(14),
        ]),
        // F: curve operators with operand counts the format does not allow,
        // read as FreeType's CFF engine reads them: vhcurveto with 11, vvcurveto
        // with 7, hhcurveto with 6, rlinecurve with 6, rcurveline with 9 and
        // hvcurveto with 7.
        charstring(&[
            N(100),
            N(100),
            Op(21),
            N(-11),
            N(-11),
            N(-11),
            N(-11),
            N(-11),
            N(-11),
            N(19),
            N(-1),
            N(18),
            N(-1),
            N(18),
            Op(30),
            N(1),
            N(2),
            N(3),
            N(4),
            N(5),
            N(6),
            N(7),
            Op(26),
            N(10),
            N(20),
            N(30),
            N(40),
            N(50),
            N(60),
            Op(27),
            N(10),
            N(0),
            N(10),
            N(10),
            N(0),
            N(10),
            Op(25),
            N(10),
            N(10),
            N(10),
            N(10),
            N(10),
            N(10),
            N(5),
            N(5),
            N(99),
            Op(24),
            N(5),
            N(5),
            N(5),
            N(5),
            N(5),
            N(5),
            N(5),
            Op(31),
            Op(14),
        ]),
    ];
    // Local subroutines 0..12: subr k calls subr k + 1, then draws; the last draws.
    let mut local: Vec<Vec<u8>> = (0..12)
        .map(|k| charstring(&[N(k + 1 - 107), Op(10), N(10), N(20), Op(5), Op(11)]))
        .collect();
    local.push(charstring(&[N(50), N(0), Op(5), Op(11)]));
    let program = CffProgram {
        glyphs,
        global: Vec::new(),
        local,
        // A, acute, D, B, C, E, F as standard strings.
        charset: Some(vec![34, 125, 37, 35, 36, 38, 39]),
        strings: Vec::new(),
        encoding: None,
        matrix: None,
        nominal_width: 500,
    };
    vec![
        (*b"head", head(1000, false, 0)),
        (*b"hhea", hhea(8)),
        (*b"maxp", maxp(8)),
        (*b"OS/2", os2(400, 0x40)),
        (
            *b"hmtx",
            hmtx(&[
                (500, 0),
                (600, 100),
                (500, 250),
                (600, 100),
                (700, 100),
                (700, 100),
                (900, 100),
                (600, 100),
            ]),
        ),
        (
            *b"cmap",
            cmap(&[
                (0x41, 1),
                (0x42, 4),
                (0x43, 5),
                (0x44, 3),
                (0x45, 6),
                (0x46, 7),
                (0xb4, 2),
            ]),
        ),
        (
            *b"name",
            name(&[
                (1, "Authored CFF"),
                (2, "Regular"),
                (4, "Authored CFF Regular"),
                (6, "AuthoredCFF-Regular"),
            ]),
        ),
        (*b"CFF ", program.build()),
    ]
}

/// A CFF face whose FontMatrix slants and halves its glyphs (2000 units per em).
fn cff_matrix_tables() -> Tables {
    use Part::{N, Op};
    let square = charstring(&[
        N(100),
        N(100),
        Op(21),
        N(500),
        Op(6),
        N(500),
        Op(7),
        N(-500),
        Op(6),
        Op(14),
    ]);
    let program = CffProgram {
        glyphs: vec![charstring(&[Op(14)]), square],
        global: Vec::new(),
        local: Vec::new(),
        charset: None,
        strings: Vec::new(),
        encoding: None,
        matrix: Some(["0.0005", "0", "0.0001", "0.0005", "0.025", "-0.0125"]),
        nominal_width: 0,
    };
    vec![
        (*b"head", head(2000, false, 0)),
        (*b"hhea", hhea(2)),
        (*b"maxp", maxp(2)),
        (*b"hmtx", hmtx(&[(1000, 0), (1200, 100)])),
        (*b"cmap", cmap(&[(0x41, 1)])),
        (*b"name", name(&[(1, "Authored Matrix"), (2, "Regular")])),
        (*b"CFF ", program.build()),
    ]
}

/// A cmap subtable of format 0: 256 glyph bytes.
pub fn cmap_format0(map: &[(u8, u8)]) -> Vec<u8> {
    let mut glyphs = [0_u8; 256];
    for &(code, glyph) in map {
        glyphs[usize::from(code)] = glyph;
    }
    let mut t = u16s(&[0, 262, 0]);
    t.extend(glyphs);
    t
}

/// Format 6 (16-bit) or 10 (32-bit): consecutive codes from `first`.
pub fn cmap_trimmed(format: u16, first: u32, glyphs: &[u16]) -> Vec<u8> {
    if format == 6 {
        let mut t = u16s(&[
            6,
            (10 + 2 * glyphs.len()) as u16,
            0,
            first as u16,
            glyphs.len() as u16,
        ]);
        t.extend(u16s(glyphs));
        t
    } else {
        let mut t = u16s(&[10, 0]);
        for v in [20 + 2 * glyphs.len() as u32, 0, first, glyphs.len() as u32] {
            t.extend(v.to_be_bytes());
        }
        t.extend(u16s(glyphs));
        t
    }
}

/// Format 12 or 13 from (first, last, glyph) groups.
pub fn cmap_groups(format: u16, groups: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut t = u16s(&[format, 0]);
    for v in [16 + 12 * groups.len() as u32, 0, groups.len() as u32] {
        t.extend(v.to_be_bytes());
    }
    for &(a, b, g) in groups {
        for v in [a, b, g] {
            t.extend(v.to_be_bytes());
        }
    }
    t
}

/// A cmap table from (platform, encoding, subtable) records.
pub fn cmap_records(records: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
    let mut t = u16s(&[0, records.len() as u16]);
    let mut offset = 4 + 8 * records.len();
    for (platform, encoding, subtable) in records {
        t.extend(u16s(&[*platform, *encoding]));
        t.extend((offset as u32).to_be_bytes());
        offset += subtable.len();
    }
    for (_, _, subtable) in records {
        t.extend(subtable);
    }
    t
}

/// A TrueType face with one subtable of each supported cmap format.
fn cmap_formats_tables() -> Tables {
    let mut tables = truetype_tables(400, 0x40, "Authored Cmap", "Regular");
    let format4 = {
        let full = cmap(&[(0x41, 1), (0x42, 2), (0x43, 3)]);
        full[12..].to_vec()
    };
    let records = vec![
        (0, 3, cmap_trimmed(6, 0x61, &[1, 2, 3])),
        (0, 4, cmap_trimmed(10, 0x1_0400, &[3, 2])),
        (
            0,
            6,
            cmap_groups(13, &[(0x2000, 0x200f, 1), (0x1_f600, 0x1_f64f, 2)]),
        ),
        (1, 0, cmap_format0(&[(0x41, 1), (0x8e, 2)])),
        (3, 1, format4),
        (
            3,
            10,
            cmap_groups(12, &[(0x41, 0x43, 1), (0x1_d400, 0x1_d402, 1)]),
        ),
    ];
    for table in &mut tables {
        if &table.0 == b"cmap" {
            table.1 = cmap_records(&records);
        }
    }
    tables
}

/// TrueType faces that differ only in their cmap, one for each step of the
/// charmap libass selects for text: (3, 10) after (3, 1), the first (3, 1)
/// after Symbol, Symbol over the Unicode and Macintosh subtables, and without
/// Windows subtables FreeType's choice (ISO after Unicode BMP, (0, 4) over a
/// later format 12 (0, 6)) or the first subtable. The subtables map 'A' (and
/// 'B' and 'C') to different glyphs. An `fpgm` keeps the auto-hinter off them.
fn cmap_order_faces() -> Vec<(&'static [u8; 4], Tables)> {
    let bmp = |pairs: &[(u16, u16)]| cmap(pairs)[12..].to_vec();
    let faces = [
        (
            "Full",
            vec![
                (0, 3, bmp(&[(0x41, 1), (0x42, 2), (0x43, 3)])),
                (3, 1, bmp(&[(0x41, 1), (0x42, 2), (0x43, 3)])),
                (
                    3,
                    10,
                    cmap_groups(
                        12,
                        &[
                            (0x41, 0x41, 3),
                            (0x42, 0x42, 2),
                            (0x43, 0x43, 1),
                            (0x1_d400, 0x1_d400, 1),
                        ],
                    ),
                ),
            ],
        ),
        (
            "BMP",
            vec![
                (3, 0, bmp(&[(0xf041, 3), (0xf042, 2)])),
                (3, 1, bmp(&[(0x41, 1), (0x42, 2)])),
                (3, 1, cmap_trimmed(6, 0x41, &[2, 1])),
            ],
        ),
        (
            "Symbol",
            vec![
                (0, 3, bmp(&[(0x41, 1)])),
                (1, 0, cmap_format0(&[(0x41, 2)])),
                (3, 0, bmp(&[(0xf041, 3)])),
            ],
        ),
        (
            "ISO",
            vec![
                (0, 3, bmp(&[(0x41, 1)])),
                (2, 1, bmp(&[(0x41, 2)])),
                (1, 0, cmap_format0(&[(0x41, 3)])),
            ],
        ),
        (
            "UCS4",
            vec![
                (0, 4, cmap_groups(12, &[(0x41, 0x41, 1)])),
                (0, 6, cmap_groups(12, &[(0x41, 0x41, 2)])),
                (0, 3, bmp(&[(0x41, 3)])),
            ],
        ),
        (
            "Macintosh",
            vec![
                (1, 0, cmap_format0(&[(0x41, 1)])),
                (1, 1, cmap_trimmed(6, 0x41, &[2])),
            ],
        ),
    ];
    faces
        .into_iter()
        .map(|(style, records)| {
            let mut tables = truetype_tables(400, 0x40, "Authored Cmap Order", style);
            for table in &mut tables {
                if &table.0 == b"cmap" {
                    table.1 = cmap_records(&records);
                }
            }
            // PUSHB_1 0; POP
            tables.push((*b"fpgm", vec![0xb0, 0, 0x21]));
            (b"\0\x01\0\0", tables)
        })
        .collect()
}

/// A vertical metrics header (`vhea`, or the extended `VHEA` with a 32-bit
/// record count) listing `metrics` records.
pub fn vhea(metrics: u32, extended: bool) -> Vec<u8> {
    let mut t = 0x0001_1000_u32.to_be_bytes().to_vec();
    t.extend(i16s(&[500, -500, 0]));
    t.extend(u16s(&[1500]));
    t.extend([0; 22]);
    if extended {
        t.extend(metrics.to_be_bytes());
    } else {
        t.extend(u16s(&[metrics as u16]));
    }
    t
}

/// A format 2 subtable: single bytes 0x41 and 0x42 to glyphs 7 and 1 through
/// the first subheader, and 0x8140 to glyph 2 through the subheader of lead
/// byte 0x81.
pub fn cmap_high_byte() -> Vec<u8> {
    let mut keys = [0_u16; 256];
    keys[0x81] = 8;
    let mut t = u16s(&[2, 540, 0]);
    t.extend(u16s(&keys));
    // Subheaders at 518 and 526; each glyph array offset counts from its
    // idRangeOffset word, at 524 and 532, to the arrays at 534 and 538.
    t.extend(u16s(&[0x41, 2, 0, 10]));
    t.extend(u16s(&[0x40, 1, 0, 6]));
    t.extend(u16s(&[7, 1, 2]));
    t
}

/// A format 8 subtable from (first, last, glyph) groups, all of 16-bit codes
/// but those above 0xFFFF.
pub fn cmap_mixed(groups: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut t = u16s(&[8, 0]);
    t.extend((8208 + 12 * groups.len() as u32).to_be_bytes());
    t.extend(0_u32.to_be_bytes());
    t.extend([0_u8; 8192]);
    t.extend((groups.len() as u32).to_be_bytes());
    for &(a, b, g) in groups {
        for v in [a, b, g] {
            t.extend(v.to_be_bytes());
        }
    }
    t
}

/// TrueType faces with each way FreeType reads vertical advances. Their
/// glyphs: 1 a square, 2 a bump, 3 glyph 1 at half size plus glyph 2 with its
/// metrics, 4 glyph 1 moved up without them; 'A' to 'D' map 1 to 4.
/// - `Records`: four long `vmtx` records (an advance height of 65535, one of
///   zero that leaves the phantom points level, and the composite's own,
///   which `USE_MY_METRICS` replaces) and a top side bearing after them.
/// - `Made Up`: a `vhea` without records, so FreeType makes the advances up.
/// - `Short`: a 32-byte `vhea`, whose record count FreeType reads from the
///   `vmtx` after it, the first record's top side bearing: two records.
/// - `Extended`: only `VHEA` and `VMTX`, which FreeType reads for them.
///
/// An `fpgm` keeps the auto-hinter off them.
fn vertical_faces() -> Vec<(&'static [u8; 4], Tables)> {
    let square: &[(i16, i16, bool)] = &[
        (100, 0, true),
        (100, 200, true),
        (300, 200, true),
        (300, 0, true),
    ];
    let bump: &[(i16, i16, bool)] = &[(0, 0, true), (50, 100, false), (100, 0, true)];
    let (glyf, loca) = glyf(&[
        Vec::new(),
        simple(&[square]),
        simple(&[bump]),
        composite(&[(0, 1, 10, 20, Some(0.5)), (0x0200, 2, 0, 0, None)]),
        composite(&[(0, 1, 0, 300, None)]),
    ]);
    let records = hmtx(&[(65535, 10), (0, 100), (1500, -30), (800, 70)]);
    let faces: [(&str, Tables); 4] = [
        (
            "Records",
            vec![
                (*b"vhea", vhea(4, false)),
                (*b"vmtx", [records.clone(), i16s(&[-20])].concat()),
            ],
        ),
        (
            "Made Up",
            vec![(*b"vhea", vhea(0, false)), (*b"vmtx", records.clone())],
        ),
        (
            "Short",
            vec![
                (*b"vhea", vhea(2, false)[..32].to_vec()),
                (
                    *b"vmtx",
                    [hmtx(&[(1000, 2), (900, 40)]), i16s(&[-10, 5, 15])].concat(),
                ),
            ],
        ),
        (
            "Extended",
            vec![
                (*b"VHEA", vhea(3, true)),
                (*b"VMTX", [records[..12].to_vec(), i16s(&[25, -5])].concat()),
            ],
        ),
    ];
    faces
        .into_iter()
        .map(|(style, vertical)| {
            let full = format!("Authored Vertical {style}");
            let postscript = format!("AuthoredVertical-{style}").replace(' ', "");
            let mut tables = vec![
                (*b"head", head(1000, false, 0)),
                (*b"hhea", hhea(5)),
                (*b"maxp", maxp(5)),
                (*b"OS/2", os2(400, 0x40)),
                (
                    *b"hmtx",
                    hmtx(&[(500, 0), (400, 50), (300, 0), (600, 0), (450, 100)]),
                ),
                (
                    *b"cmap",
                    cmap(&[(0x41, 1), (0x42, 2), (0x43, 3), (0x44, 4)]),
                ),
                (
                    *b"name",
                    name(&[
                        (1, "Authored Vertical"),
                        (2, style),
                        (4, &full),
                        (6, &postscript),
                    ]),
                ),
                (*b"glyf", glyf.clone()),
                (*b"loca", loca.clone()),
                // PUSHB_1 0; POP
                (*b"fpgm", vec![0xb0, 0, 0x21]),
            ];
            tables.extend(vertical);
            (b"\0\x01\0\0", tables)
        })
        .collect()
}

/// A CFF face whose charmaps are all ones FreeType reads beyond the common
/// formats: a format 2 subtable on (1, 1), a format 8 one on (3, 10), the
/// Unicode charmap FreeType builds from glyph names, since no subtable is
/// (3, 1) or Unicode-platform, and the charmap of a custom encoding with a
/// supplement. The names are standard strings and strings of the face: an
/// Adobe Glyph List name, a variant, `uniXXXX` and `uXXXXX` forms, `space` and
/// `Delta`, which FreeType maps twice, a name used by two glyphs and one that
/// maps to no code point.
fn charmaps_tables() -> Tables {
    use Part::{N, Op};
    let square = |side: i16| {
        charstring(&[
            N(50),
            N(0),
            Op(21),
            N(side),
            Op(6),
            N(side),
            Op(7),
            N(-side),
            Op(6),
            Op(14),
        ])
    };
    let mut glyphs = vec![charstring(&[Op(14)])];
    glyphs.extend((1..10).map(|g| square(100 + 40 * g)));
    let program = CffProgram {
        glyphs,
        global: Vec::new(),
        local: Vec::new(),
        // A, A.swash, uni00E9, u1F600, space, Delta, B, B, glyph9.
        charset: Some(vec![34, 391, 392, 393, 1, 394, 35, 35, 395]),
        strings: [&b"A.swash"[..], b"uni00E9", b"u1F600", b"Delta", b"glyph9"]
            .map(<[u8]>::to_vec)
            .to_vec(),
        // Format 1 with supplements: codes 0x80..=0x83 for glyphs 1 to 4, and
        // code 0xC0 for SID 395 (glyph 9).
        encoding: Some(vec![0x81, 1, 0x80, 3, 1, 0xc0, 0x01, 0x8b]),
        matrix: None,
        nominal_width: 0,
    };
    let metrics: Vec<(u16, i16)> = (0..10).map(|g| (300 + 50 * g, 50)).collect();
    vec![
        (*b"head", head(1000, false, 0)),
        (*b"hhea", hhea(10)),
        (*b"maxp", maxp(10)),
        (*b"OS/2", os2(400, 0x40)),
        (*b"hmtx", hmtx(&metrics)),
        (
            *b"cmap",
            cmap_records(&[
                (1, 1, cmap_high_byte()),
                (
                    3,
                    10,
                    cmap_mixed(&[(0x42, 0x42, 8), (0x1_f600, 0x1_f600, 4)]),
                ),
            ]),
        ),
        (
            *b"name",
            name(&[
                (1, "Authored Charmaps"),
                (2, "Regular"),
                (4, "Authored Charmaps Regular"),
                (6, "AuthoredCharmaps-Regular"),
            ]),
        ),
        (*b"CFF ", program.build()),
    ]
}

/// Authored fonts for the FreeType comparison: features the freely licensed
/// corpus does not exercise. Named by their file in the corpus.
pub fn authored_fonts() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "authored-cff-features.otf",
            font(b"OTTO", &cff_features_tables()),
        ),
        (
            "authored-cff-matrix.otf",
            font(b"OTTO", &cff_matrix_tables()),
        ),
        (
            "authored-cmap-formats.ttf",
            font(b"\0\x01\0\0", &cmap_formats_tables()),
        ),
        ("authored-cmap-order.ttc", collection(&cmap_order_faces())),
        ("authored-vertical.ttc", collection(&vertical_faces())),
        ("authored-charmaps.otf", font(b"OTTO", &charmaps_tables())),
    ]
}

pub fn cff_tables() -> Tables {
    vec![
        (*b"head", head(1000, false, 0)),
        (*b"hhea", hhea(3)),
        (*b"maxp", maxp(3)),
        (*b"hmtx", hmtx(&[(500, 0), (410, 50), (300, 0)])),
        (*b"cmap", cmap(&[(0x41, 1), (0x42, 2)])),
        (*b"name", name(&[(1, "Test Sans"), (2, "Regular")])),
        (*b"CFF ", cff_table()),
    ]
}

pub fn bytes(data: &[u8]) -> (Budget, Bytes) {
    let budget = Budget::new(data.len() + (1 << 24)).unwrap();
    let bytes = Bytes::copy_from_slice(&budget, data).unwrap();
    (budget, bytes)
}

/// A deterministic xorshift generator for mutations.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One of four edits, chosen by `round`: bit flips, a truncation, an overwritten
/// burst, or a removed range, for the mutation test.
pub fn mutate(data: &mut Vec<u8>, rng: &mut Rng, round: usize) {
    if data.is_empty() {
        return;
    }
    match round % 4 {
        0 => {
            for _ in 0..1 + rng.below(16) {
                let at = rng.below(data.len());
                data[at] ^= 1 << rng.below(8);
            }
        }
        1 => {
            let len = rng.below(data.len());
            data.truncate(len);
        }
        2 => {
            let at = rng.below(data.len());
            for k in 0..rng.below(64) {
                if let Some(byte) = data.get_mut(at + k) {
                    *byte = rng.next() as u8;
                }
            }
        }
        _ => {
            let a = rng.below(data.len());
            let b = (a + rng.below(4096)).min(data.len());
            data.drain(a..b);
        }
    }
}

/// Every public read on every face of `data`; errors are fine, panics are not.
pub fn exercise(data: &[u8], limits: Limits) {
    let (budget, bytes) = bytes(data);
    let Ok(count) = Font::face_count(data) else {
        return;
    };
    let mut fonts = Vec::new();
    for index in 0..count.min(4) {
        if let Ok(font) = Font::new(bytes.clone(), index, &budget, limits) {
            fonts.push(font);
        }
    }
    let mut outline = Outline::new(&budget);
    let mut scaled = Outline::new(&budget);
    for font in &fonts {
        let _ = (
            font.vertical_metrics(),
            font.style(),
            font.family_name(),
            font.style_name(),
        );
        let _ = font.postscript_name().map(|r| r.chars().count());
        for record in font.names() {
            let _ = record.chars().count();
        }
        for charmap in font.charmaps() {
            let _ = charmap.mappings().take(512).count();
            for code in [0, 0x41, 0xffff, 0x1_0000, u32::MAX] {
                let _ = charmap.glyph(code);
            }
        }
        // The first glyphs and an even spread of the rest, about 300 in all.
        let count = font.glyph_count();
        let step = (count / 256).max(1);
        let spread = (0..count.min(48)).chain((48..count).step_by(step as usize));
        // Sizes: libass's 256 pixels, a small one and the extremes.
        let sized: Vec<_> = [16384, 538, 1, i32::MAX]
            .into_iter()
            .filter_map(|height| font.at_size(height).ok())
            .collect();
        let synthesis = font.synthesis(900, 110);
        for size in &sized {
            let _ = (
                size.ascent_descent(),
                size.harfbuzz_scale(),
                size.underline(),
            );
            let _ = (size.strikeout(), size.embolden_strength());
        }
        for glyph in spread.map(GlyphId) {
            let _ = font.advance(glyph);
            let _ = font.outline_into(glyph, &mut outline);
            let _ = outline.segments().count();
            let _ = font.kerning(glyph, GlyphId(1));
            for size in &sized {
                let _ = size.hinted_advance(glyph);
                let _ = size.kerning(glyph, GlyphId(1));
                if size.outline_into(glyph, &mut scaled).is_ok() {
                    let _ = size.synthesize(&mut scaled, synthesis);
                    let _ = scaled.segments().count();
                }
            }
        }
        let _ = font.glyph_index('A');
    }
    let faces: Vec<FaceInfo<'_>> = fonts.iter().map(FaceInfo::new).collect();
    for family in ["Test Sans", "x"] {
        let request = Request {
            family,
            weight: 700,
            slant: 100,
            character: Some(u32::from('A')),
        };
        let _ = select(&faces, &request);
    }
}
