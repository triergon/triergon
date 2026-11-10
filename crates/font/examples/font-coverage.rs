//! Prints which charstring operators and cmap formats the given font files
//! exercise, over every glyph of every face, and which they do not.
//!
//! Usage: font-coverage FILE...

use std::collections::BTreeSet;
use triergon_core::{Budget, Bytes};
use triergon_font::{CharstringOperator, Font, Limits, OperatorSet, Outline};

fn main() {
    let budget = Budget::new(1 << 31).expect("budget");
    let (mut operators, mut formats) = (OperatorSet::default(), BTreeSet::new());
    for path in std::env::args().skip(1) {
        let data = std::fs::read(&path).expect("font file");
        let bytes = Bytes::copy_from_slice(&budget, &data).expect("font bytes");
        let mut scratch = Outline::new(&budget);
        for index in 0..Font::face_count(&data).unwrap_or(0) {
            let Ok(font) = Font::new(bytes.clone(), index, &budget, Limits::default()) else {
                continue;
            };
            formats.extend(font.charmaps().filter_map(|c| c.kind().format_number()));
            for glyph in font.glyphs() {
                if let Ok(used) = font.charstring_operators(glyph, &mut scratch) {
                    operators = operators.union(used);
                }
            }
        }
    }
    let names = |covered: bool| -> Vec<&str> {
        CharstringOperator::ALL
            .into_iter()
            .filter(|&op| operators.contains(op) == covered)
            .map(CharstringOperator::name)
            .collect()
    };
    println!("operators exercised: {}", names(true).join(" "));
    println!("operators not exercised: {}", names(false).join(" "));
    println!("cmap formats: {formats:?}");
}
