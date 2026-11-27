//! The records the shaping operations produce, in the order
//! `harness/reference_shape.h` produces them.

use std::io;
use triergon_harness_runner::records::{self, Sink};

/// One group of values, in the order `harness/reference_shape.h` produces them.
#[derive(Clone, Copy)]
pub enum Record {
    /// A face of the font file.
    Face(u32),
    /// A face the reader rejects.
    FaceError(u32),
    /// A text argument that does not parse.
    TextError(usize),
    /// A shaped text: its glyph count, then its glyphs.
    Text(usize),
    Glyph {
        id: u16,
        cluster: u32,
        x_advance: i32,
        y_advance: i32,
        x_offset: i32,
        y_offset: i32,
    },
    /// A face or text the candidate cannot shape; the reference always can.
    ShapeError(usize),
    /// A face's glyph count, then each glyph's extents.
    Glyphs(u16),
    /// Whether a glyph has extents, and its x and y bearing, width and height
    /// (zero without).
    Extents {
        id: u16,
        extents: Option<[i32; 4]>,
    },
    /// A glyph whose extents the candidate cannot read; the reference always
    /// can.
    ExtentsError(u16),
    /// A resolved line: its length, then directions, levels and the map.
    BidiLine(usize),
    Paragraph(bool),
    Level(u8),
    Map(usize),
    BidiLineError(usize),
    /// A simply shaped line: its length, then its characters.
    SimpleLine(usize),
    Char(u32),
    SimpleLineError(usize),
    Range(u32, u32),
    /// A code point's properties, in the reference's numbering.
    Point([i64; 11]),
    RangeError(usize),
}

impl Record {
    /// The tag and values, as the reference folds and prints them.
    fn parts(self) -> (u64, &'static str, Values) {
        let one = |v: i64| Values::one(v);
        match self {
            Self::Face(i) => (1, "face", one(i.into())),
            Self::FaceError(i) => (2, "face-error", one(i.into())),
            Self::Text(n) => (3, "text", one(n as i64)),
            Self::Glyph {
                id,
                cluster,
                x_advance,
                y_advance,
                x_offset,
                y_offset,
            } => (
                4,
                "glyph",
                Values::many(&[
                    id.into(),
                    cluster.into(),
                    x_advance.into(),
                    y_advance.into(),
                    x_offset.into(),
                    y_offset.into(),
                ]),
            ),
            Self::TextError(i) => (5, "text-error", one(i as i64)),
            Self::ShapeError(i) => (6, "shape-error", one(i as i64)),
            Self::Glyphs(n) => (7, "glyphs", one(n.into())),
            Self::Extents { id, extents } => {
                let [x, y, w, h] = extents.unwrap_or_default();
                let found = extents.is_some();
                let values = [
                    id.into(),
                    found.into(),
                    x.into(),
                    y.into(),
                    w.into(),
                    h.into(),
                ];
                (8, "extents", Values::many(&values))
            }
            Self::ExtentsError(id) => (9, "extents-error", one(id.into())),
            Self::BidiLine(n) => (10, "line", one(n as i64)),
            Self::Paragraph(rtl) => (11, "paragraph", one(rtl.into())),
            Self::Level(l) => (12, "level", one(l.into())),
            Self::Map(i) => (13, "map", one(i as i64)),
            Self::BidiLineError(i) => (14, "line-error", one(i as i64)),
            Self::SimpleLine(n) => (20, "line", one(n as i64)),
            Self::Char(c) => (21, "char", one(c.into())),
            Self::SimpleLineError(i) => (22, "line-error", one(i as i64)),
            Self::Range(first, last) => (30, "range", Values::many(&[first.into(), last.into()])),
            Self::Point(values) => (31, "point", Values::many(&values)),
            Self::RangeError(i) => (32, "range-error", one(i as i64)),
        }
    }
}

/// Up to eleven values of one record.
struct Values {
    values: [i64; 11],
    len: usize,
}

impl Values {
    fn one(value: i64) -> Self {
        Self::many(&[value])
    }

    fn many(values: &[i64]) -> Self {
        let mut all = [0; 11];
        all[..values.len()].copy_from_slice(values);
        Self {
            values: all,
            len: values.len(),
        }
    }

    fn as_slice(&self) -> &[i64] {
        &self.values[..self.len]
    }
}

/// A record's tag and the name the dump prints; the operations count their
/// packets and bytes themselves.
#[derive(Clone, Copy)]
pub struct Tag(u64, &'static str);

impl records::Kind for Tag {
    fn tag(self) -> u64 {
        self.0
    }

    fn name(self) -> &'static str {
        self.1
    }

    fn counted(self) -> bool {
        false
    }
}

/// Consumes the records of one operation.
pub trait Records: Sink<Tag> {
    /// One record: its tag, then its values.
    #[inline]
    fn record(&mut self, record: Record) -> io::Result<()> {
        let (tag, name, values) = record.parts();
        self.begin(Tag(tag, name))?;
        self.ints(values.as_slice())?;
        self.end()
    }
}

impl<S: Sink<Tag>> Records for S {}
