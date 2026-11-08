//! A font and buffer for testing shaper steps in isolation.

use crate::buffer::Buffer;
use crate::metrics::{Extents, Metrics, Scale, Scaled};
use triergon_core::Budget;
use triergon_font::GlyphId;

/// Maps the listed characters, each to a glyph of its own with the given advance.
#[derive(Debug)]
pub(super) struct TestFont(pub(super) &'static [(u32, i32)]);

impl TestFont {
    fn index(&self, c: u32) -> Option<usize> {
        self.0.iter().position(|&(u, _)| u == c)
    }
}

impl Metrics for TestFont {
    fn scale(&self) -> Scale {
        Scale::units(1000)
    }

    fn nominal_glyph(&self, c: char) -> Option<GlyphId> {
        self.index(u32::from(c))
            .and_then(|i| u16::try_from(i + 1).ok())
            .map(GlyphId)
    }

    fn variation_glyph(&self, _c: char, _selector: char) -> Option<GlyphId> {
        None
    }

    fn h_advance(&self, glyph: GlyphId) -> i32 {
        usize::from(glyph.0)
            .checked_sub(1)
            .and_then(|g| self.0.get(g))
            .map_or(0, |&(_, advance)| advance)
    }

    fn extents(&self, _glyph: GlyphId) -> Option<Extents> {
        None
    }
}

pub(super) fn scaled(font: &TestFont) -> Scaled<'_> {
    Scaled::new(font, 1000)
}

/// A buffer holding `text` as one item.
pub(super) fn text_buffer(budget: &Budget, text: &[u32]) -> Buffer {
    let chars: Vec<char> = text.iter().filter_map(|&c| char::from_u32(c)).collect();
    let mut buffer = Buffer::new(budget);
    buffer
        .load(&chars, 0..chars.len())
        .expect("test text loads");
    buffer
}

/// The characters (or glyphs) and clusters of the buffer.
pub(super) fn contents(buffer: &Buffer) -> Vec<(u32, u32)> {
    buffer
        .infos()
        .iter()
        .map(|i| (i.glyph, i.cluster))
        .collect()
}
