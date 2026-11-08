//! A small TrueType font with GDEF, GSUB and GPOS built in memory, and the
//! shaping run the robustness tests repeat over mutated copies of fonts.

#![allow(dead_code, reason = "each test binary uses a part")]

use triergon_core::{Budget, Bytes};
use triergon_font::{Font, Limits};
use triergon_shape::{
    Buffer, Direction, Face, Feature, Language, OtMetrics, Plan, Scale, Script, Tag, bidi, shape,
};

fn be16(out: &mut Vec<u8>, v: u16) {
    out.extend(v.to_be_bytes());
}

/// Glyphs: 0 .notdef, 1..=26 'a'..'z', 27 "fi" ligature, 28 U+0301 (a mark),
/// 29 alternate 'a'.
const GLYPHS: u16 = 30;

fn coverage(glyphs: &[u16]) -> Vec<u8> {
    let mut t = Vec::new();
    be16(&mut t, 1);
    be16(&mut t, glyphs.len() as u16);
    for &g in glyphs {
        be16(&mut t, g);
    }
    t
}

/// A layout table with script `latn` and `DFLT` listing every feature, one
/// lookup per feature.
fn layout(features: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let n = features.len() as u16;
    // Script list: two scripts sharing one default language system.
    let mut langsys = Vec::new();
    be16(&mut langsys, 0);
    be16(&mut langsys, 0xffff);
    be16(&mut langsys, n);
    for i in 0..n {
        be16(&mut langsys, i);
    }
    let mut script = Vec::new();
    be16(&mut script, 4);
    be16(&mut script, 0);
    script.extend(&langsys);
    let mut scripts = Vec::new();
    be16(&mut scripts, 2);
    for (k, tag) in [b"DFLT", b"latn"].iter().enumerate() {
        scripts.extend(*tag);
        be16(&mut scripts, (2 + 12 + k * script.len()) as u16);
    }
    scripts.extend(&script);
    scripts.extend(&script);
    let mut feature_list = Vec::new();
    be16(&mut feature_list, n);
    for (i, (tag, _)) in features.iter().enumerate() {
        feature_list.extend(*tag);
        be16(&mut feature_list, (2 + 6 * features.len() + 6 * i) as u16);
    }
    for i in 0..n {
        be16(&mut feature_list, 0);
        be16(&mut feature_list, 1);
        be16(&mut feature_list, i);
    }
    let mut lookups = Vec::new();
    be16(&mut lookups, n);
    let mut at = 2 + 2 * features.len();
    for (_, lookup) in features {
        be16(&mut lookups, at as u16);
        at += lookup.len();
    }
    for (_, l) in features {
        lookups.extend(l);
    }
    let mut t = Vec::new();
    be16(&mut t, 1);
    be16(&mut t, 0);
    be16(&mut t, 10);
    be16(&mut t, (10 + scripts.len()) as u16);
    be16(&mut t, (10 + scripts.len() + feature_list.len()) as u16);
    t.extend(scripts);
    t.extend(feature_list);
    t.extend(lookups);
    t
}

/// A lookup of `kind` with `flags` and one subtable.
fn lookup(kind: u16, flags: u16, subtable: Vec<u8>) -> Vec<u8> {
    let mut t = Vec::new();
    be16(&mut t, kind);
    be16(&mut t, flags);
    be16(&mut t, 1);
    be16(&mut t, 8);
    t.extend(subtable);
    t
}

fn gsub() -> Vec<u8> {
    // liga: f i -> fi.
    let mut liga = Vec::new();
    be16(&mut liga, 1);
    be16(&mut liga, 0); // coverage, patched below
    be16(&mut liga, 1);
    be16(&mut liga, 8);
    let set_at = liga.len();
    be16(&mut liga, 1);
    be16(&mut liga, 4);
    be16(&mut liga, 27);
    be16(&mut liga, 2);
    be16(&mut liga, 9);
    let cov = liga.len() as u16;
    liga[2..4].copy_from_slice(&cov.to_be_bytes());
    let _ = set_at;
    liga.extend(coverage(&[6]));
    // salt: a -> alternate a.
    let mut single = Vec::new();
    be16(&mut single, 1);
    be16(&mut single, 6);
    be16(&mut single, 29);
    single.extend(coverage(&[1]));
    // calt: chain context, backtrack o, input f, lookahead f; applies lookup 1.
    let mut chain = Vec::new();
    be16(&mut chain, 3);
    be16(&mut chain, 1);
    be16(&mut chain, 0);
    be16(&mut chain, 1);
    be16(&mut chain, 0);
    be16(&mut chain, 1);
    be16(&mut chain, 0);
    be16(&mut chain, 1);
    be16(&mut chain, 0);
    be16(&mut chain, 1);
    let base = chain.len();
    for (patch, glyph) in [(4, 15_u16), (8, 6), (12, 6)] {
        let at = chain.len() as u16;
        chain[patch..patch + 2].copy_from_slice(&at.to_be_bytes());
        chain.extend(coverage(&[glyph]));
    }
    let _ = base;
    layout(&[
        (b"liga", lookup(4, 0, liga)),
        (b"salt", lookup(1, 0, single)),
        (b"calt", lookup(6, 0, chain)),
    ])
}

fn gpos() -> Vec<u8> {
    // kern: A V pairs (glyph 1, 22) -80.
    let mut pair = Vec::new();
    be16(&mut pair, 1);
    be16(&mut pair, 0);
    be16(&mut pair, 4);
    be16(&mut pair, 0);
    be16(&mut pair, 1);
    be16(&mut pair, 12);
    be16(&mut pair, 1);
    be16(&mut pair, 22);
    pair.extend((-80_i16).to_be_bytes());
    let cov = pair.len() as u16;
    pair[2..4].copy_from_slice(&cov.to_be_bytes());
    pair.extend(coverage(&[1]));
    // mark: U+0301 on any base glyph 1..=26.
    let mut mark = Vec::new();
    be16(&mut mark, 1);
    for _ in 0..5 {
        be16(&mut mark, 0);
    }
    let anchor = |out: &mut Vec<u8>, x: i16, y: i16| {
        be16(out, 1);
        out.extend(x.to_be_bytes());
        out.extend(y.to_be_bytes());
    };
    mark[6..8].copy_from_slice(&1_u16.to_be_bytes());
    let mark_cov = mark.len() as u16;
    mark[2..4].copy_from_slice(&mark_cov.to_be_bytes());
    mark.extend(coverage(&[28]));
    let bases: Vec<u16> = (1..=26).collect();
    let base_cov = mark.len() as u16;
    mark[4..6].copy_from_slice(&base_cov.to_be_bytes());
    mark.extend(coverage(&bases));
    let mark_array = mark.len() as u16;
    mark[8..10].copy_from_slice(&mark_array.to_be_bytes());
    be16(&mut mark, 1);
    be16(&mut mark, 0);
    be16(&mut mark, 6);
    anchor(&mut mark, 250, 0);
    let base_array = mark.len() as u16;
    mark[10..12].copy_from_slice(&base_array.to_be_bytes());
    be16(&mut mark, 26);
    for i in 0..26 {
        be16(&mut mark, (2 + 2 * 26 + 6 * i) as u16);
    }
    for _ in 0..26 {
        anchor(&mut mark, 300, 700);
    }
    layout(&[(b"kern", lookup(2, 0, pair)), (b"mark", lookup(4, 0, mark))])
}

fn gdef() -> Vec<u8> {
    let mut t = Vec::new();
    be16(&mut t, 1);
    be16(&mut t, 0);
    be16(&mut t, 12);
    be16(&mut t, 0);
    be16(&mut t, 0);
    be16(&mut t, 0);
    // Class definition format 2: bases 1..=27, mark 28.
    be16(&mut t, 2);
    be16(&mut t, 2);
    for (first, last, class) in [(1_u16, 27_u16, 1_u16), (28, 28, 3)] {
        be16(&mut t, first);
        be16(&mut t, last);
        be16(&mut t, class);
    }
    t
}

fn cmap() -> Vec<u8> {
    // Format 4: a..z -> 1..26, U+0301 -> 28.
    let segments: [(u16, u16, i16); 3] = [
        (0x61, 0x7a, 1 - 0x61),
        (0x301, 0x301, 28 - 0x301),
        (0xffff, 0xffff, 1),
    ];
    let mut sub = Vec::new();
    be16(&mut sub, 4);
    be16(&mut sub, (16 + 8 * segments.len()) as u16);
    be16(&mut sub, 0);
    be16(&mut sub, 2 * segments.len() as u16);
    be16(&mut sub, 4);
    be16(&mut sub, 1);
    be16(&mut sub, 2);
    for s in &segments {
        be16(&mut sub, s.1);
    }
    be16(&mut sub, 0);
    for s in &segments {
        be16(&mut sub, s.0);
    }
    for s in &segments {
        sub.extend(s.2.to_be_bytes());
    }
    for _ in &segments {
        be16(&mut sub, 0);
    }
    let mut t = Vec::new();
    be16(&mut t, 0);
    be16(&mut t, 1);
    be16(&mut t, 3);
    be16(&mut t, 1);
    t.extend(12_u32.to_be_bytes());
    t.extend(sub);
    t
}

/// The test font file.
pub fn font() -> Vec<u8> {
    let mut head = vec![0; 54];
    head[0..4].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    head[12..16].copy_from_slice(&0x5F0F_3CF5_u32.to_be_bytes());
    head[18..20].copy_from_slice(&1000_u16.to_be_bytes());
    let mut hhea = vec![0; 36];
    hhea[0..4].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
    hhea[4..6].copy_from_slice(&800_i16.to_be_bytes());
    hhea[34..36].copy_from_slice(&GLYPHS.to_be_bytes());
    let mut maxp = Vec::new();
    maxp.extend(0x0000_5000_u32.to_be_bytes());
    be16(&mut maxp, GLYPHS);
    let mut hmtx = Vec::new();
    for g in 0..GLYPHS {
        be16(&mut hmtx, if g == 28 { 0 } else { 500 + g });
        be16(&mut hmtx, 0);
    }
    let loca = vec![0; 2 * (usize::from(GLYPHS) + 1)];
    let tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"GDEF", gdef()),
        (b"GPOS", gpos()),
        (b"GSUB", gsub()),
        (b"cmap", cmap()),
        // Every glyph is empty; the table only needs to exist.
        (b"glyf", vec![0; 4]),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
    ];
    let mut out = Vec::new();
    out.extend(0x0001_0000_u32.to_be_bytes());
    be16(&mut out, tables.len() as u16);
    out.extend([0; 6]);
    let mut offset = 12 + 16 * tables.len();
    for (tag, data) in &tables {
        out.extend(*tag);
        out.extend([0; 4]);
        out.extend((offset as u32).to_be_bytes());
        out.extend((data.len() as u32).to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in &tables {
        out.extend(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    out
}

/// Texts shaped against every font, with their script and direction.
pub const TEXTS: [(&str, &[u8; 4], Direction); 6] = [
    (
        "office affluent fi\u{301} a\u{301}\u{301}",
        b"Latn",
        Direction::LeftToRight,
    ),
    (
        "AVAVA To\u{200d}a\u{00ad}b",
        b"Latn",
        Direction::LeftToRight,
    ),
    (
        "\u{627}\u{644}\u{633}\u{644}\u{627}\u{645} \u{644}\u{627}",
        b"Arab",
        Direction::RightToLeft,
    ),
    (
        "\u{928}\u{92e}\u{938}\u{94d}\u{924}\u{947} \u{930}\u{94d}\u{915}",
        b"Deva",
        Direction::LeftToRight,
    ),
    (
        "\u{e01}\u{e33}\u{e48} \u{1100}\u{1161}\u{11a8} \u{d55c}",
        b"Thai",
        Direction::LeftToRight,
    ),
    (
        "\u{5e9}\u{5c1}\u{5b8}\u{5dc}\u{5d5}\u{5b9}\u{5dd} \u{1780}\u{17d2}\u{179a}",
        b"Hebr",
        Direction::RightToLeft,
    ),
];

/// Shapes `text` as Latin with the test font and no extra features: glyph ids
/// and positions.
pub fn shape_latin(text: &str) -> Vec<(u16, triergon_shape::Position)> {
    shape_latin_within(text, triergon_shape::Limits::default()).expect("shape")
}

/// [`shape_latin`] within `limits`, with the buffer's work as well.
pub fn shape_latin_within(
    text: &str,
    limits: triergon_shape::Limits,
) -> triergon_core::Result<Vec<(u16, triergon_shape::Position)>> {
    let budget = Budget::new(16 << 20).expect("budget");
    let bytes = Bytes::copy_from_slice(&budget, &font()).expect("bytes");
    let font = Font::new(bytes, 0, &budget, Limits::default()).expect("font");
    let face = Face::new(&font);
    let scale = Scale::units(font.units_per_em());
    let metrics = OtMetrics::new(&font, scale);
    let language = Language::new("en").expect("language");
    let plan = Plan::new(
        &face,
        Direction::LeftToRight,
        Script::from_iso15924(*b"Latn"),
        &language,
        &[],
        &budget,
    )
    .expect("plan");
    let text: Vec<char> = text.chars().collect();
    let mut buffer = Buffer::with_limits(&budget, limits);
    shape(&face, &metrics, &plan, &text, 0..text.len(), &mut buffer)?;
    Ok(buffer.glyphs().map(|(g, p)| (g.id.0, p)).collect())
}

/// Parses `data` and shapes every text with it, returning how many runs shaped
/// without error. Malformed fonts must give errors, never panics.
pub fn exercise(data: &[u8]) -> usize {
    let Ok(budget) = Budget::new(64 << 20) else {
        return 0;
    };
    let Ok(bytes) = Bytes::copy_from_slice(&budget, data) else {
        return 0;
    };
    let limits = Limits {
        work: 1 << 20,
        ..Limits::default()
    };
    let Ok(font) = Font::new(bytes, 0, &budget, limits) else {
        return 0;
    };
    let face = Face::new(&font);
    let scale = Scale::units(font.units_per_em());
    let metrics = OtMetrics::new(&font, scale);
    let features = [Feature {
        tag: Tag::new(b"salt"),
        value: 1,
    }];
    let mut buffer = Buffer::new(&budget);
    let mut shaped = 0;
    for (text, script, direction) in TEXTS {
        let text: Vec<char> = text.chars().collect();
        let Ok(language) = Language::new("en") else {
            continue;
        };
        let Ok(plan) = Plan::new(
            &face,
            direction,
            Script::from_iso15924(*script),
            &language,
            &features,
            &budget,
        ) else {
            continue;
        };
        if shape(&face, &metrics, &plan, &text, 0..text.len(), &mut buffer).is_ok() {
            shaped += 1;
        }
    }
    shaped
}

/// Resolves and reorders `text` as one paragraph with each base direction.
pub fn exercise_bidi(text: &[char]) {
    let Ok(budget) = Budget::new(16 << 20) else {
        return;
    };
    let classes: Vec<bidi::BidiClass> = text.iter().map(|&c| bidi::BidiClass::of(c)).collect();
    let brackets: Vec<Option<bidi::Bracket>> = text.iter().map(|&c| bidi::Bracket::of(c)).collect();
    let mut resolver = bidi::Resolver::new(&budget);
    for base in [
        bidi::BaseDirection::LeftToRight,
        bidi::BaseDirection::RightToLeft,
        bidi::BaseDirection::Auto,
    ] {
        for with_brackets in [false, true] {
            let mut levels = vec![bidi::Level::of(Direction::LeftToRight); text.len()];
            let brackets = with_brackets.then_some(brackets.as_slice());
            let Ok(direction) = resolver.resolve(&classes, brackets, base, &mut levels) else {
                continue;
            };
            let mut order: Vec<usize> = (0..text.len()).collect();
            let _ = bidi::reorder_line(&classes, direction, &mut levels, &mut order);
        }
    }
}

/// A xorshift generator, so runs repeat exactly.
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

/// One of four edits of `data[range]`: bit flips, a truncation of the file, an
/// overwritten burst, a removed range.
pub fn mutate(data: &mut Vec<u8>, range: std::ops::Range<usize>, rng: &mut Rng, round: usize) {
    if range.is_empty() {
        return;
    }
    let pick = |rng: &mut Rng| range.start + rng.below(range.len());
    match round % 4 {
        0 => {
            for _ in 0..1 + rng.below(8) {
                let at = pick(rng);
                data[at] ^= 1 << rng.below(8);
            }
        }
        1 => {
            let len = pick(rng);
            data.truncate(len);
        }
        2 => {
            let at = pick(rng);
            for k in 0..rng.below(16) {
                if let Some(byte) = data.get_mut(at + k) {
                    *byte = rng.next() as u8;
                }
            }
        }
        _ => {
            let at = pick(rng);
            let end = (at + 1 + rng.below(32)).min(data.len());
            data.drain(at..end);
        }
    }
}

/// The byte range of table `tag` in a font file.
pub fn table_range(data: &[u8], tag: &[u8; 4]) -> Option<std::ops::Range<usize>> {
    let count = usize::from(u16::from_be_bytes([*data.get(4)?, *data.get(5)?]));
    (0..count).find_map(|i| {
        let record = data.get(12 + 16 * i..28 + 16 * i)?;
        (&record[..4] == tag).then(|| {
            let offset = u32::from_be_bytes(record[8..12].try_into().ok()?) as usize;
            let len = u32::from_be_bytes(record[12..16].try_into().ok()?) as usize;
            Some(offset..offset + len)
        })?
    })
}
