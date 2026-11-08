//! Mutated layout tables and fonts must give errors or glyphs, never panics or
//! unbounded work; bidi resolution must accept any text. `cargo xtask fuzz
//! --contract` mutates the shaping features' corpus inputs the same way.

mod support;

use support::{Rng, exercise, exercise_bidi, font, mutate, table_range};

#[test]
fn the_test_font_ligates_kerns_and_attaches() {
    let glyphs = support::shape_latin("fiav\u{301}");
    let ids: Vec<u16> = glyphs.iter().map(|g| g.0).collect();
    assert_eq!(ids, [27, 1, 22, 28]);
    assert_eq!(glyphs[1].1.x_advance, 501 - 80);
    assert_eq!((glyphs[3].1.x_advance, glyphs[3].1.y_offset), (0, 700));
}

#[test]
fn work_beyond_the_limits_is_a_policy_error() {
    let text = "fiav\u{301}".repeat(64);
    let within = |work_per_char| {
        let limits = triergon_shape::Limits {
            work_per_char,
            minimum_work: 0,
            ..triergon_shape::Limits::default()
        };
        support::shape_latin_within(&text, limits)
    };
    assert!(within(4096).is_ok());
    assert_eq!(
        within(1).unwrap_err(),
        triergon_core::Error::Policy("shaping work")
    );
}

#[test]
fn the_test_font_shapes_every_text() {
    assert_eq!(exercise(&font()), support::TEXTS.len());
}

#[test]
fn mutated_layout_tables_never_panic() {
    let original = font();
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for tag in [b"GSUB", b"GPOS", b"GDEF", b"cmap", b"hmtx"] {
        let range = table_range(&original, tag).expect("table present");
        for round in 0..400 {
            let mut data = original.clone();
            mutate(&mut data, range.clone(), &mut rng, round);
            exercise(&data);
        }
    }
}

#[test]
fn mutated_files_never_panic() {
    let original = font();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for round in 0..400 {
        let mut data = original.clone();
        mutate(&mut data, 0..original.len(), &mut rng, round);
        exercise(&data);
    }
}

#[test]
fn random_bidi_text_never_panics() {
    // Characters of every bidi class, brackets and separators.
    const ALPHABET: &[char] = &[
        'a', '\u{5d0}', '\u{627}', '1', '\u{661}', '+', '$', ',', '\u{300}', '\u{200b}', '\n',
        '\t', ' ', '!', '(', ')', '[', ']', '\u{2329}', '\u{3009}', '\u{202a}', '\u{202b}',
        '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        '\u{200d}',
    ];
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    for round in 0..2000 {
        let len = rng.below(if round % 10 == 0 { 400 } else { 40 });
        let text: Vec<char> = (0..len)
            .map(|_| ALPHABET[rng.below(ALPHABET.len())])
            .collect();
        exercise_bidi(&text);
    }
}
