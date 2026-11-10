mod support;

use support::{bytes, cff_tables, collection, font, truetype_tables};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_font::{
    Bounds, Built, CharEncoding, CharmapKind, CharstringBounds, CmapFormat, ControlBox, FaceInfo,
    Font, GlyphId, Limits, Orientation, Outline, OutlineFormat, Platform, Point, PointKind,
    Request, Segment, Store, StoreKey, Style, select,
};

fn parse(data: &[u8]) -> Font {
    let (budget, bytes) = bytes(data);
    Font::new(bytes, 0, &budget, Limits::default()).unwrap()
}

fn points(font: &Font, glyph: u16) -> (Vec<(i32, i32, PointKind)>, Vec<u32>, i32) {
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
        (charmap.platform(), charmap.encoding(), charmap.kind()),
        (
            Platform::Windows,
            CharEncoding::Unicode,
            CharmapKind::Subtable(CmapFormat::SegmentDelta)
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
        [(50, 0, on), (50, 200, on), (250, 200, on), (250, 0, on)]
    );
    assert_eq!((ends, advance), (vec![4], 400));
    let (composite, ends, advance) = points(&font, 3);
    // Glyph 1 unshifted at half size plus (10, 20), then glyph 2, whose metrics apply.
    assert_eq!(&composite[..2], [(60, 20, on), (60, 120, on)]);
    assert_eq!(composite[5], (50, 100, PointKind::Quadratic));
    assert_eq!((ends, advance), (vec![4, 7], 300));
    let budget = Budget::new(1 << 20).unwrap();
    let (outline, _) = font.outline(GlyphId(2), &budget).unwrap();
    let segments: Vec<_> = outline.segments().collect();
    let p = Point::new;
    assert_eq!(
        segments,
        [
            Segment::MoveTo(p(0, 0)),
            Segment::QuadTo(p(50, 100), p(100, 0)),
            Segment::Close
        ]
    );
    assert!(font.outline(GlyphId(0), &budget).unwrap().0.is_empty());
}

#[test]
fn sizes_scale_as_freetype_does_for_libass() {
    let face = parse(&font(
        b"\0\x01\0\0",
        &truetype_tables(400, 0, "Test Sans", "Regular"),
    ));
    // Windows metrics 1024 and 256: libass's span is 1280 font units.
    let gdi = face.gdi_metrics();
    assert_eq!(
        (gdi.ascender, gdi.descender, gdi.height),
        (1024, -256, 1280)
    );
    let sized = face.at_size(16_384).unwrap();
    let m = sized.metrics();
    assert_eq!((m.x_scale, m.y_scale, m.x_ppem), (838_861, 838_861, 200));
    assert_eq!((m.ascender, m.descender, m.height), (13120, -3328, 16384));
    assert_eq!(sized.ascent_descent(), (13107, 3277));
    assert_eq!(sized.harfbuzz_scale(), (12800, 12800));
    assert_eq!(sized.embolden_strength(), 200);
    assert_eq!(sized.kerning(GlyphId(1), GlyphId(2)), -512);
    // No bytecode: FreeType would auto-hint, which this crate does not.
    assert!(matches!(
        sized.default_advance(GlyphId(1)),
        Err(Error::Unsupported(_))
    ));
    let budget = Budget::new(1 << 20).unwrap();
    let mut outline = Outline::new(&budget);
    let metrics = sized.outline_into(GlyphId(1), &mut outline).unwrap();
    let corners: Vec<_> = outline.points().iter().map(|p| p.point).collect();
    // xMin 100 scales to 1280; the origin (xMin - lsb) to 640.
    assert_eq!(
        corners,
        [
            Point::new(640, 0),
            Point::new(640, 2560),
            Point::new(3200, 2560),
            Point::new(3200, 0)
        ]
    );
    assert_eq!(metrics.advance, 5120);
    assert_eq!(outline.orientation(), Orientation::TrueType);
    let synthesis = face.synthesis(700, 110);
    assert_eq!((synthesis.oblique, synthesis.embolden), (true, true));
    sized.synthesize(&mut outline, synthesis).unwrap();
    assert_eq!(outline.points()[1].point, Point::new(1538, 2760));
    let tricky = parse(&font(
        b"\0\x01\0\0",
        &truetype_tables(400, 0, "ABCDEF+PMingLiU", "Regular"),
    ));
    assert_eq!(
        tricky.at_size(16384).unwrap().default_advance(GlyphId(1)),
        Err(Error::Unsupported(
            "hinted advances of a tricky TrueType font"
        ))
    );
    assert!(face.at_size(-1).is_err());
    // No pixels per em: FreeType loads glyphs unscaled and unhinted.
    let unscaled = face.at_size(0).unwrap();
    assert_eq!(unscaled.metrics().x_ppem, 0);
    assert_eq!(unscaled.ascent_descent(), (0, 0));
    assert_eq!(unscaled.harfbuzz_scale(), (0, 0));
    let metrics = unscaled.outline_into(GlyphId(1), &mut outline).unwrap();
    assert_eq!(outline.points()[2].point, Point::new(250, 200));
    assert_eq!(metrics.advance, 400);
    assert_eq!(unscaled.default_advance(GlyphId(1)), Ok(400));
    assert_eq!(face.at_size(1).unwrap().metrics().x_ppem, 0);

    let cff = parse(&font(b"OTTO", &cff_tables()));
    let sized = cff.at_size(16384).unwrap();
    // hhea 800 and -200 without OS/2: 410 units are 6717.4 26.6 units, rounded to 105 pixels.
    assert_eq!(sized.default_advance(GlyphId(1)), Ok(6720));
    assert!(cff.is_postscript());
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
        [(50, 0, on), (150, 0, on), (150, 100, on), (50, 100, on)]
    );
    assert_eq!((ends, advance), (vec![4], 410));
    let (curve, ends, _) = points(&font, 2);
    // The zero-length line is dropped; the flex adds two curves.
    assert_eq!(ends, [10]);
    assert_eq!(curve[3], (60, 0, on));
    assert_eq!(curve[9], (110, 0, on));
    assert_eq!(curve[4].2, PointKind::Cubic);
}

#[test]
fn control_boxes_come_from_glyf_headers_and_unrounded_charstrings() {
    let work = || WorkBudget::new(1 << 20);
    let cff = parse(&font(b"OTTO", &cff_tables()));
    let unit = |v: i32| v << 16;
    assert_eq!(
        cff.control_box(GlyphId(1), &mut work()),
        Ok(ControlBox::Charstring(CharstringBounds {
            x_min: unit(50),
            y_min: 0,
            x_max: unit(150),
            y_max: unit(100),
        }))
    );
    let Ok(ControlBox::Charstring(curve)) = cff.control_box(GlyphId(2), &mut work()) else {
        panic!("glyph 2 draws a curve");
    };
    let (points, _, _) = points(&cff, 2);
    for (x, y, _) in points {
        assert!((curve.x_min..=curve.x_max).contains(&unit(x)));
        assert!((curve.y_min..=curve.y_max).contains(&unit(y)));
    }
    // No outline is built, so the point limit does not apply.
    let (budget, data) = bytes(&font(b"OTTO", &cff_tables()));
    let few = Limits {
        points: 1,
        ..Limits::default()
    };
    let cff = Font::new(data, 0, &budget, few).unwrap();
    assert!(matches!(
        cff.outline(GlyphId(2), &budget),
        Err(Error::Policy(_))
    ));
    assert_eq!(
        cff.control_box(GlyphId(2), &mut work()),
        Ok(ControlBox::Charstring(curve))
    );

    let tables = truetype_tables(400, 0, "Test Sans", "Regular");
    let truetype = parse(&font(b"\0\x01\0\0", &tables));
    assert_eq!(
        truetype.control_box(GlyphId(0), &mut work()),
        Ok(ControlBox::Empty)
    );
    assert_eq!(
        truetype.control_box(GlyphId(1), &mut work()),
        Ok(ControlBox::Glyf(Bounds {
            x_min: 100,
            y_min: 0,
            x_max: 300,
            y_max: 200,
        }))
    );
    assert_eq!(truetype.side_bearing(GlyphId(1)), Ok(50));
    assert!(matches!(
        truetype.control_box(GlyphId(4), &mut work()),
        Err(Error::Invalid(_))
    ));
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
    // Weights and slants as libass's font description gives them for the
    // ASS bold and italic values 0 and 1.
    let request = |family, bold: u32, italic: u32| Request {
        family,
        weight: 400 + 300 * bold,
        slant: 100 * italic,
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
    // Any slant asks for italic, as `\i-1` (slant 2^32 - 1) does in libass.
    let any_slant = Request {
        slant: u32::MAX,
        ..request("Test Sans", 0, 0)
    };
    assert_eq!(select(&faces, &any_slant), Some(2));
    let needs_c = Request {
        character: Some(u32::from('C')),
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

#[test]
fn vertical_advances_follow_each_way_freetype_reads_them() {
    let fonts = support::authored_fonts();
    let (_, data) = fonts
        .iter()
        .find(|(name, _)| *name == "authored-vertical.ttc")
        .unwrap();
    let (budget, bytes) = bytes(data);
    let mut outline = Outline::new(&budget);
    // Per face: whether it has vertical metrics, and each glyph's advance
    // height in font units.
    let expected: [(bool, [i32; 5]); 4] = [
        // 65535, level phantom points, the bump's own, the bump's through
        // USE_MY_METRICS instead of the composite's 800, and the last record's.
        (true, [65535, 0, 1500, 1500, 800]),
        // Made up from the typographic ascender and descender.
        (false, [1050; 5]),
        // Two records, counted by the bytes after a short `vhea`.
        (true, [1000, 900, 900, 900, 900]),
        // `VHEA` and `VMTX`.
        (true, [65535, 0, 1500, 1500, 1500]),
    ];
    for (index, (vertical, advances)) in expected.into_iter().enumerate() {
        let font = Font::new(bytes.clone(), index as u32, &budget, Limits::default()).unwrap();
        assert_eq!(font.has_vertical_metrics(), vertical, "face {index}");
        for (glyph, advance) in advances.into_iter().enumerate() {
            let metrics = font
                .outline_into(GlyphId(glyph as u16), &mut outline)
                .unwrap();
            assert_eq!(
                (metrics.vertical_advance, metrics.linear_vertical_advance),
                (advance, i64::from(advance)),
                "face {index}, glyph {glyph}"
            );
        }
    }
    // A `vhea` cut short at the end of the file fails the face.
    let mut tables = truetype_tables(400, 0, "Test Sans", "Regular");
    tables.push((*b"vhea", support::vhea(4, false)[..20].to_vec()));
    let data = font(b"\0\x01\0\0", &tables);
    let (budget, bytes) = support::bytes(&data);
    assert!(matches!(
        Font::new(bytes, 0, &budget, Limits::default()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn cff_faces_list_freetype_charmaps_in_its_order() {
    let data = support::authored_fonts()
        .into_iter()
        .find(|(name, _)| *name == "authored-charmaps.otf")
        .unwrap()
        .1;
    let font = parse(&data);
    let kinds: Vec<_> = font
        .charmaps()
        .map(|c| (c.platform(), c.encoding_id(), c.kind()))
        .collect();
    assert_eq!(
        kinds,
        [
            (
                Platform::Macintosh,
                1,
                CharmapKind::Subtable(CmapFormat::HighByte)
            ),
            (
                Platform::Windows,
                10,
                CharmapKind::Subtable(CmapFormat::Mixed)
            ),
            (Platform::Windows, 1, CharmapKind::GlyphNames),
            (Platform::Other(7), 2, CharmapKind::CffEncoding),
        ]
    );
    // libass reads text through the Windows UCS-4 subtable.
    assert_eq!(font.text_charmap_index(), Some(1));
    let maps: Vec<Vec<(u32, u16)>> = font
        .charmaps()
        .map(|c| {
            c.mappings()
                .map(|m| m.map(|(c, g)| (c, g.0)).unwrap())
                .collect()
        })
        .collect();
    assert_eq!(maps[0], [(0x41, 7), (0x42, 1), (0x8140, 2)]);
    assert_eq!(maps[1], [(0x42, 8), (0x1_f600, 4)]);
    // Names: A, B (twice), é and the emoji, space with U+00A0 and Delta
    // (U+2206) with U+0394 besides; A.swash is a variant of A.
    let names: Vec<u32> = maps[2].iter().map(|&(c, _)| c).collect();
    assert_eq!(
        names,
        [0x20, 0x41, 0x42, 0xa0, 0xe9, 0x394, 0x2206, 0x1_f600]
    );
    let names = font.charmap(2).unwrap();
    assert_eq!(names.glyph(0x41), Some(GlyphId(1)));
    assert_eq!(names.glyph(0xe9), Some(GlyphId(3)));
    assert_eq!(names.glyph(0x394), Some(GlyphId(6)));
    assert_eq!(
        maps[3],
        [(0x80, 1), (0x81, 2), (0x82, 3), (0x83, 4), (0xc0, 9)]
    );
    assert_eq!(names.encoding(), CharEncoding::Unicode);
}
