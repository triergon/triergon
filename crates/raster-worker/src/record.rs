//! The records the operations produce, in the order `harness/reference_raster.h`
//! produces them, and the outline and bitmap values they carry.

use std::io;
use triergon_harness_runner::records::{self, FNV_OFFSET, Sink, fnv};
use triergon_raster::compat::libass::PlacedBitmap;
use triergon_raster::{Bitmap, Outline, SegmentKind, Vector};

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

impl records::Kind for Kind {
    fn tag(self) -> u64 {
        self as u64
    }

    fn name(self) -> &'static str {
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

    /// The glyph records of a font case describe the input, not work done.
    fn counted(self) -> bool {
        (self as u64) < Self::Glyphs as u64
    }
}

/// The values of an outline or a bitmap, an absent one standing for a missing
/// outline or bitmap.
pub trait Values: Sink<Kind> {
    /// Points and segments and their FNV-1a hash: each point's x and y as
    /// little-endian 32-bit values, then each segment as libass stores it (its
    /// order, plus 4 when it closes a contour).
    fn outline(&mut self, outline: &Outline) -> io::Result<()> {
        let mut hash = FNV_OFFSET;
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
        self.count(0, outline.points().len());
        self.ints(&[
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
        self.bitmap_at(Some(&placed.bitmap), placed.origin.offset_from(origin))
    }

    /// A bitmap with its position moved by `shift`.
    fn bitmap_at(&mut self, bitmap: Option<&Bitmap>, shift: Vector) -> io::Result<()> {
        let Some(b) = bitmap else {
            return self.absent();
        };
        let hash = fnv(FNV_OFFSET, b.data());
        self.count(0, b.width() * b.height());
        self.ints(&[
            b.position.x.wrapping_add(shift.x).into(),
            b.position.y.wrapping_add(shift.y).into(),
            b.width() as i64,
            b.height() as i64,
            b.stride() as i64,
            hash as i64,
        ])
    }
}

impl<S: Sink<Kind>> Values for S {}
