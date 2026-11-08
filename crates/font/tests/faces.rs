mod support;

use support::{bytes, cff_tables, collection, font, truetype_tables};
use triergon_core::{Budget, Bytes, Error};
use triergon_font::{
    Built, CharEncoding, CmapFormat, FaceInfo, Font, GlyphId, Limits, OutlineFormat, Platform,
    Point, PointKind, Request, Segment, Store, StoreKey, Style, select,
};

fn parse(data: &[u8]) -> Font {
    let (budget, bytes) = bytes(data);
    Font::new(bytes, 0, &budget, Limits::default()).unwrap()
}

fn points(font: &Font, glyph: u32) -> (Vec<(f32, f32, PointKind)>, Vec<u32>, u16) {
    let budget = Budget::new(1 << 20).unwrap();
    let (outline, metrics) = font.outline(GlyphId(glyph), &budget).unwrap();
    let points = outline
        .points()
        .iter()
        .map(|p| (p.point.x, p.point.y, p.kind))
        .collect();
    (points, outline.contour_ends().to_vec(), metrics.advance)
}

#[test]
fn truetype_metrics_names_and_charmap() {
    let font = parse(&font(
        b"\0\x01\0\0",
        &truetype_tables(700, 0x20, "Test Sans", "Bold"),
    ));
    assert_eq!(font.outline_format(), OutlineFormat::TrueType);
    assert_eq!((font.glyph_count(), font.units_per_em()), (4, 1000));
    let v = font.vertical_metrics();
    assert_eq!((v.ascender, v.descender, v.height), (800, -200, 1090));
    assert_eq!(
        font.style(),
        Style {
            bold: true,
            italic: false
        }
    );
    assert_eq!(
        font.family_name().unwrap().chars().collect::<String>(),
        "Test Sans"
    );
    assert_eq!(font.style_name().unwrap().platform, Platform::Windows);
    assert_eq!(
        font.postscript_name().unwrap().chars().collect::<String>(),
        "TestSans-Bold"
    );
    assert_eq!(font.names().count(), 8);
    assert_eq!(font.glyph_index('B'), Some(GlyphId(2)));
    assert_eq!(font.glyph_index('D'), None);
    let charmap = font.text_charmap().unwrap();
    assert_eq!(
        (charmap.platform(), charmap.encoding(), charmap.format()),
        (
            Platform::Windows,
            CharEncoding::Unicode,
            CmapFormat::SegmentDelta
        )
    );
    assert_eq!(charmap.mappings().count(), 3);
    assert_eq!(font.kerning_pairs().count(), 1);
    assert_eq!(font.kerning(GlyphId(1), GlyphId(2)), -40);
    assert_eq!(font.kerning(GlyphId(2), GlyphId(1)), 0);
    assert_eq!(font.advance(GlyphId(3)), Ok(600));
    assert!(font.advance(GlyphId(4)).is_err());
}

#[test]
fn truetype_outlines_shift_to_the_side_bearing_and_compose() {
    let font = parse(&font(
        b"\0\x01\0\0",
        &truetype_tables(400, 0, "Test Sans", "Regular"),
    ));
    let (square, ends, advance) = points(&font, 1);
    // xMin 100, lsb 50: shifted left by 50.
    let on = PointKind::On;
    assert_eq!(
        square,
        [
            (50.0, 0.0, on),
            (50.0, 200.0, on),
            (250.0, 200.0, on),
            (250.0, 0.0, on)
        ]
    );
    assert_eq!((ends, advance), (vec![4], 400));
    let (composite, ends, advance) = points(&font, 3);
    // Glyph 1 unshifted at half size plus (10, 20), then glyph 2, whose metrics apply.
    assert_eq!(&composite[..2], [(60.0, 20.0, on), (60.0, 120.0, on)]);
    assert_eq!(composite[5], (50.0, 100.0, PointKind::Quadratic));
    assert_eq!((ends, advance), (vec![4, 7], 300));
    let budget = Budget::new(1 << 20).unwrap();
    let (outline, _) = font.outline(GlyphId(2), &budget).unwrap();
    let segments: Vec<_> = outline.segments().collect();
    let p = Point::new;
    assert_eq!(
        segments,
        [
            Segment::MoveTo(p(0.0, 0.0)),
            Segment::QuadTo(p(50.0, 100.0), p(100.0, 0.0)),
            Segment::Close
        ]
    );
    assert!(font.outline(GlyphId(0), &budget).unwrap().0.is_empty());
}

#[test]
fn cff_charstrings_with_subroutines_hints_and_flex() {
    let font = parse(&font(b"OTTO", &cff_tables()));
    assert_eq!(font.outline_format(), OutlineFormat::Cff);
    assert!(!font.is_cid_keyed());
    let (square, ends, advance) = points(&font, 1);
    let on = PointKind::On;
    assert_eq!(
        square,
        [
            (50.0, 0.0, on),
            (150.0, 0.0, on),
            (150.0, 100.0, on),
            (50.0, 100.0, on)
        ]
    );
    assert_eq!((ends, advance), (vec![4], 410));
    let (curve, ends, _) = points(&font, 2);
    // The zero-length line is dropped; the flex adds two curves.
    assert_eq!(ends, [10]);
    assert_eq!(curve[3], (60.0, 0.0, on));
    assert_eq!(curve[9], (110.0, 0.0, on));
    assert_eq!(curve[4].2, PointKind::Cubic);
}

#[test]
fn collections_select_faces_and_matching_scores_styles() {
    let data = collection(&[
        (
            b"\0\x01\0\0",
            truetype_tables(400, 0x40, "Test Sans", "Regular"),
        ),
        (
            b"\0\x01\0\0",
            truetype_tables(700, 0x20, "Test Sans", "Bold"),
        ),
        (
            b"\0\x01\0\0",
            truetype_tables(400, 0x01, "Test Sans", "Italic"),
        ),
        (b"OTTO", cff_tables()),
    ]);
    assert_eq!(Font::face_count(&data), Ok(4));
    let (budget, bytes) = bytes(&data);
    let fonts: Vec<Font> = (0..4)
        .map(|i| Font::new(bytes.clone(), i, &budget, Limits::default()).unwrap())
        .collect();
    assert_eq!(fonts[3].outline_format(), OutlineFormat::Cff);
    assert_eq!(
        Font::new(bytes.clone(), 4, &budget, Limits::default()).unwrap_err(),
        Error::Invalid("face index outside the font file")
    );
    let faces: Vec<FaceInfo<'_>> = fonts.iter().map(FaceInfo::new).collect();
    assert_eq!(faces[1].weight(), 700);
    assert!(faces[2].is_italic() && !faces[2].is_bold());
    let request = |family, bold, italic| Request {
        family,
        bold,
        italic,
        character: None,
    };
    assert_eq!(select(&faces, &request("test sans", 0, 0)), Some(0));
    assert_eq!(select(&faces, &request("Test Sans", 1, 0)), Some(1));
    assert_eq!(select(&faces, &request("Test Sans", 0, 1)), Some(2));
    // Bold italic: an upright bold face costs 1; the italic regular, emboldened, 51.
    assert_eq!(select(&faces, &request("Test Sans", 1, 1)), Some(1));
    // A full name matches exactly, scoring zero; a TrueType face is not matched by
    // its PostScript name alone.
    assert_eq!(select(&faces, &request("Test Sans Italic", 1, 0)), Some(2));
    assert_eq!(select(&faces, &request("test sans bold", 0, 0)), Some(1));
    assert_eq!(select(&faces, &request("TestSans-Bold", 0, 0)), None);
    assert_eq!(select(&faces, &request("Arimo", 0, 0)), None);
    let needs_c = Request {
        character: Some('C'),
        ..request("Test Sans", 0, 0)
    };
    assert_eq!(select(&faces[3..], &needs_c), None);
}

#[test]
fn limits_and_budgets_bound_outlines() {
    let data = font(
        b"\0\x01\0\0",
        &truetype_tables(400, 0, "Test Sans", "Regular"),
    );
    let (budget, bytes) = bytes(&data);
    let tight = Limits {
        points: 5,
        ..Limits::default()
    };
    let font = Font::new(bytes.clone(), 0, &budget, tight).unwrap();
    assert_eq!(
        font.outline(GlyphId(3), &budget).unwrap_err(),
        Error::Policy("outline points")
    );
    let shallow = Limits {
        depth: 0,
        ..Limits::default()
    };
    let font = Font::new(bytes.clone(), 0, &budget, shallow).unwrap();
    assert_eq!(
        font.outline(GlyphId(3), &budget).unwrap_err(),
        Error::Policy("glyph nesting depth")
    );
    let idle = Limits {
        work: 3,
        ..Limits::default()
    };
    let font = Font::new(bytes.clone(), 0, &budget, Limits::default()).unwrap();
    assert!(Font::new(bytes, 0, &budget, idle).is_err());
    let small = Budget::new(Budget::control_storage_bytes() + 16).unwrap();
    assert!(matches!(
        font.outline(GlyphId(1), &small).unwrap_err(),
        Error::Limit { .. }
    ));
}

#[test]
fn malformed_files_are_errors() {
    let (budget, _) = bytes(&[]);
    let open = |data: &[u8]| {
        Font::new(
            Bytes::copy_from_slice(&budget, data).unwrap(),
            0,
            &budget,
            Limits::default(),
        )
    };
    assert_eq!(
        open(b"wOFF\0\0\0\0").unwrap_err(),
        Error::Unsupported("font file signature")
    );
    let mut tables = truetype_tables(400, 0, "Test Sans", "Regular");
    tables.retain(|(tag, _)| tag != b"loca");
    assert_eq!(
        open(&font(b"\0\x01\0\0", &tables)).unwrap_err(),
        Error::Invalid("missing required table")
    );
    let mut tables = cff_tables();
    tables.retain(|(tag, _)| tag != b"CFF ");
    assert_eq!(
        open(&font(b"OTTO", &tables)).unwrap_err(),
        Error::Invalid("face has no outlines")
    );
}

#[derive(Debug, Hash, PartialEq, Eq)]
struct FaceKey(u32);

impl StoreKey for FaceKey {
    type Value = Font;
}

#[test]
fn the_store_shares_parsed_faces() {
    let data = font(b"OTTO", &cff_tables());
    let (budget, bytes) = bytes(&data);
    let store = Store::new(&budget);
    let build = || {
        Font::new(bytes.clone(), 0, &budget, Limits::default()).map(|font| Built {
            value: font,
            bytes: 0,
        })
    };
    let a = store.get_or_build(FaceKey(0), build).unwrap();
    let b = store
        .get_or_build(FaceKey(0), || panic!("built twice"))
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    assert!(store.retained_bytes() > 0);
}
