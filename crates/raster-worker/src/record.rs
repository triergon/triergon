//! The values the operations produce, and the two ways of consuming them: the
//! ordered work digest of the harness protocol and the readable line format.
//! `harness/reference_raster.h` produces the same records in the same order.

use std::io::{self, Write};
use triergon_harness_protocol::digest::{fold, signed};
use triergon_raster::{Bitmap, Outline, PlacedBitmap, SegmentKind, Vector};

/// The digest value of an absent outline or bitmap.
const ABSENT: u64 = u64::MAX;
const FNV_INIT: u64 = 0xcbf2_9ce4_8422_2325;

/// What a record reports, with the tag it folds under.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Convert = 1,
    Rotate,
    Stroke,
    Transform,
    Raster,
    Run,
    Blur,
    Add,
    Multiply,
    MultiplyInverse,
    Fix,
    Shift,
    Clip,
    Unparsed,
    /// The glyphs of a font case: how many were selected.
    Glyphs = 20,
    /// One selected glyph: its id and points, or absent when it does not load.
    Glyph,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Convert => "convert",
            Self::Rotate => "rotate",
            Self::Stroke => "stroke",
            Self::Transform => "transform",
            Self::Raster => "raster",
            Self::Run => "run",
            Self::Blur => "blur",
            Self::Add => "add",
            Self::Multiply => "mul",
            Self::MultiplyInverse => "imul",
            Self::Fix => "fix",
            Self::Shift => "shift",
            Self::Clip => "clip",
            Self::Unparsed => "unparsed",
            Self::Glyphs => "glyphs",
            Self::Glyph => "glyph",
        }
    }
}

/// Consumes the records of one operation: each a kind and values, an absent
/// value standing for a missing outline or bitmap.
pub trait Sink {
    fn begin(&mut self, kind: Kind) -> io::Result<()>;
    fn value(&mut self, value: i64) -> io::Result<()>;
    fn absent(&mut self) -> io::Result<()>;
    fn end(&mut self) -> io::Result<()>;
    /// Counts produced bytes: outline points and bitmap pixels.
    fn count_bytes(&mut self, bytes: usize);

    fn values(&mut self, values: &[i64]) -> io::Result<()> {
        values.iter().try_for_each(|&v| self.value(v))
    }

    /// Points and segments and their FNV-1a hash: each point's x and y as
    /// little-endian 32-bit values, then each segment as libass stores it (its
    /// order, plus 4 when it closes a contour).
    fn outline(&mut self, outline: &Outline) -> io::Result<()> {
        let mut hash = FNV_INIT;
        for p in outline.points() {
            hash = fnv(hash, &p.x.to_le_bytes());
            hash = fnv(hash, &p.y.to_le_bytes());
        }
        for s in outline.segments() {
            let order = match s.kind {
                SegmentKind::Line => 1,
                SegmentKind::Quadratic => 2,
                SegmentKind::Cubic => 3,
            };
            hash = fnv(hash, &[order | (u8::from(s.closes_contour) << 2)]);
        }
        self.count_bytes(outline.points().len());
        self.values(&[
            outline.points().len() as i64,
            outline.segments().len() as i64,
            hash as i64,
        ])
    }

    /// Position, size, stride and the FNV-1a hash of every row over the whole
    /// stride, the padding included, or absent.
    fn bitmap(&mut self, bitmap: Option<&Bitmap>) -> io::Result<()> {
        self.bitmap_at(bitmap, Vector::default())
    }

    /// A glyph's bitmap with its position counted from `origin`, the glyph's.
    fn placed_bitmap(&mut self, placed: Option<&PlacedBitmap>, origin: Vector) -> io::Result<()> {
        let Some(placed) = placed else {
            return self.absent();
        };
        let shift = Vector::new(
            placed.origin.x.wrapping_sub(origin.x),
            placed.origin.y.wrapping_sub(origin.y),
        );
        self.bitmap_at(Some(&placed.bitmap), shift)
    }

    /// A bitmap with its position moved by `shift`.
    fn bitmap_at(&mut self, bitmap: Option<&Bitmap>, shift: Vector) -> io::Result<()> {
        let Some(b) = bitmap else {
            return self.absent();
        };
        let hash = fnv(FNV_INIT, b.data());
        self.count_bytes(b.width() * b.height());
        self.values(&[
            b.position.x.wrapping_add(shift.x).into(),
            b.position.y.wrapping_add(shift.y).into(),
            b.width() as i64,
            b.height() as i64,
            b.stride() as i64,
            hash as i64,
        ])
    }
}

fn fnv(mut hash: u64, data: &[u8]) -> u64 {
    for &b in data {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// The work of one or more operations.
#[derive(Debug, Default)]
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
    fn begin(&mut self, kind: Kind) -> io::Result<()> {
        if (kind as u64) < Kind::Glyphs as u64 {
            self.packets += 1;
        }
        fold(&mut self.digest, kind as u64);
        Ok(())
    }

    #[inline]
    fn value(&mut self, value: i64) -> io::Result<()> {
        fold(&mut self.digest, signed(value));
        Ok(())
    }

    fn absent(&mut self) -> io::Result<()> {
        fold(&mut self.digest, ABSENT);
        Ok(())
    }

    fn end(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn count_bytes(&mut self, bytes: usize) {
        self.bytes += bytes as u64;
    }
}

/// The readable form: one line per record, `kind value value ...`, `-` for an
/// absent value.
#[derive(Debug)]
pub struct Dump<W: Write>(pub W);

impl<W: Write> Sink for Dump<W> {
    fn begin(&mut self, kind: Kind) -> io::Result<()> {
        write!(self.0, "{}", kind.name())
    }

    fn value(&mut self, value: i64) -> io::Result<()> {
        write!(self.0, " {value}")
    }

    fn absent(&mut self) -> io::Result<()> {
        write!(self.0, " -")
    }

    fn end(&mut self) -> io::Result<()> {
        writeln!(self.0)
    }

    fn count_bytes(&mut self, _bytes: usize) {}
}
