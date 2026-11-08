//! Small fonts built in memory, and the operations the robustness test runs on them.

#![allow(dead_code)]

use triergon_core::{Budget, Bytes};
use triergon_font::{FaceInfo, Font, GlyphId, Limits, Outline, Request, ScaledOutline, select};

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

/// A CFF table: glyph 1 a square drawn with a width, hints, a hintmask and a local
/// subroutine; glyph 2 a curve through a global subroutine and a flex.
pub fn cff_table() -> Vec<u8> {
    // Local subr 0 (biased -107): vlineto 100; return.
    let local = vec![vec![n(100), 7, 11]];
    // Global subr 0: rrcurveto 10 20 30 0 20 -20; return.
    let global = vec![vec![n(10), n(20), n(30), n(0), n(20), n(-20), 8, 11]];
    let glyphs = vec![
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
    ];
    let private = |subrs: i32| {
        let mut p = long(400);
        p.push(21); // nominalWidthX
        p.extend(long(subrs));
        p.push(19);
        p
    };
    let private_len = private(0).len();
    let top = |charstrings: i32, private_offset: i32| {
        let mut t = long(charstrings);
        t.push(17);
        t.extend(long(private_len as i32));
        t.extend(long(private_offset));
        t.push(18);
        t
    };
    let header = vec![1, 0, 4, 4];
    let names = index(&[b"Test".to_vec()]);
    let top_len = index(&[top(0, 0)]).len();
    let strings = index(&[]);
    let global = index(&global);
    let charstrings_at = header.len() + names.len() + top_len + strings.len() + global.len();
    let charstrings = index(&glyphs);
    let private_at = charstrings_at + charstrings.len();
    let mut out = header;
    out.extend(names);
    out.extend(index(&[top(charstrings_at as i32, private_at as i32)]));
    out.extend(strings);
    out.extend(global);
    out.extend(charstrings);
    out.extend(private(private_len as i32));
    out.extend(index(&local));
    out
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
    let mut scaled = ScaledOutline::new(&budget);
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
            bold: 1,
            italic: 1,
            character: Some('A'),
        };
        let _ = select(&faces, &request);
    }
}
