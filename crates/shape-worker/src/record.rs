//! The values the shaping operations produce, and the two ways of consuming them:
//! the ordered work digest of the harness protocol and the readable line format.

use std::io::{self, Write};
use triergon_harness_protocol::digest::{fold, signed};

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

/// Consumes the records of one operation.
pub trait Sink {
    fn record(&mut self, record: Record) -> io::Result<()>;
    /// Counts `packets` (glyphs, lines or code points) and `bytes` (code points
    /// shaped or resolved).
    fn count(&mut self, packets: usize, bytes: usize);
}

/// The work of one or more operations.
#[derive(Default)]
pub struct Work {
    pub packets: u64,
    pub bytes: u64,
    pub digest: u64,
}

impl Work {
    /// Adds the work of one operation, its digest folded as one value.
    pub fn accumulate(&mut self, operation: &Work) {
        self.packets += operation.packets;
        self.bytes += operation.bytes;
        fold(&mut self.digest, operation.digest);
    }
}

impl Sink for Work {
    #[inline]
    fn record(&mut self, record: Record) -> io::Result<()> {
        let (tag, _, values) = record.parts();
        fold(&mut self.digest, tag);
        for &value in values.as_slice() {
            fold(&mut self.digest, signed(value));
        }
        Ok(())
    }

    fn count(&mut self, packets: usize, bytes: usize) {
        self.packets += packets as u64;
        self.bytes += bytes as u64;
    }
}

/// The readable form: one line per record, `key value value ...`.
pub struct Dump<W: Write>(pub W);

impl<W: Write> Sink for Dump<W> {
    fn record(&mut self, record: Record) -> io::Result<()> {
        let (_, key, values) = record.parts();
        write!(self.0, "{key}")?;
        for value in values.as_slice() {
            write!(self.0, " {value}")?;
        }
        writeln!(self.0)
    }

    fn count(&mut self, _packets: usize, _bytes: usize) {}
}
