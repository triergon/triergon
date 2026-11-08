//! The values the operations produce, and the two ways of consuming them: the
//! ordered work digest of the harness protocol and the readable line format.
//! `harness/reference_ass.h` produces the same records in the same order.
//!
//! A record is a kind and values: integers, doubles (folded as their bits,
//! printed as 16 hex digits), byte strings (folded as their length and every
//! byte, printed quoted) and absent values (a missing string, outline or
//! scroll).

use std::io::{self, Write};
use triergon_harness_protocol::digest::{fold, fold_framed, signed};
use triergon_raster::{Outline, Rect, SegmentKind};

/// The digest value of an absent value.
const ABSENT: u64 = u64::MAX;
const FNV_INIT: u64 = 0xcbf2_9ce4_8422_2325;

/// What a record reports, with the tag it folds under.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// The track: header values and counts.
    Track = 1,
    /// The style and event format lines.
    Formats,
    Style,
    Event,
    /// An embedded font: name, size and the FNV-1a hash of its data.
    Font,
    /// No track: a whole script without a script type.
    NoTrack,
    /// The play resolution after libass's lazy initialisation.
    PlayRes,
    /// The start of an event's walk: the event and the time.
    Walk,
    Glyph,
    /// A drawing glyph's outline.
    Outline,
    /// The event state after the walk.
    State,
    /// The vector clip's outline.
    Clip,
    /// A glyph's karaoke result.
    Karaoke,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Formats => "formats",
            Self::Style => "style",
            Self::Event => "event",
            Self::Font => "font",
            Self::NoTrack => "no-track",
            Self::PlayRes => "playres",
            Self::Walk => "walk",
            Self::Glyph => "glyph",
            Self::Outline => "outline",
            Self::State => "state",
            Self::Clip => "clip",
            Self::Karaoke => "kara",
        }
    }
}

/// Consumes the records of one operation.
pub trait Sink {
    fn begin(&mut self, kind: Kind) -> io::Result<()>;
    fn int(&mut self, value: i64) -> io::Result<()>;
    fn real(&mut self, value: f64) -> io::Result<()>;
    fn text(&mut self, value: &[u8]) -> io::Result<()>;
    fn absent(&mut self) -> io::Result<()>;
    fn end(&mut self) -> io::Result<()>;

    fn ints(&mut self, values: &[i64]) -> io::Result<()> {
        values.iter().try_for_each(|&v| self.int(v))
    }

    fn reals(&mut self, values: &[f64]) -> io::Result<()> {
        values.iter().try_for_each(|&v| self.real(v))
    }

    fn flag(&mut self, value: bool) -> io::Result<()> {
        self.int(value.into())
    }

    fn optional(&mut self, value: Option<&[u8]>) -> io::Result<()> {
        match value {
            Some(value) => self.text(value),
            None => self.absent(),
        }
    }

    /// A drawing's control box, then its point and segment counts and the
    /// FNV-1a hash of each point's x and y as little-endian 32-bit values and
    /// each segment as libass stores it (its order, plus 4 when it closes a
    /// contour); absent for a drawing libass rejects.
    fn outline(&mut self, drawing: Option<(&Outline, Rect)>) -> io::Result<()> {
        let Some((outline, cbox)) = drawing else {
            return self.absent();
        };
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
        self.ints(&[
            cbox.x_min.into(),
            cbox.y_min.into(),
            cbox.x_max.into(),
            cbox.y_max.into(),
            outline.points().len() as i64,
            outline.segments().len() as i64,
            hash as i64,
        ])
    }
}

pub fn fnv(mut hash: u64, data: &[u8]) -> u64 {
    for &b in data {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// The work of one or more operations: records, the bytes of the strings
/// recorded, and the digest.
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
        self.packets += 1;
        fold(&mut self.digest, kind as u64);
        Ok(())
    }

    #[inline]
    fn int(&mut self, value: i64) -> io::Result<()> {
        fold(&mut self.digest, signed(value));
        Ok(())
    }

    #[inline]
    fn real(&mut self, value: f64) -> io::Result<()> {
        fold(&mut self.digest, value.to_bits());
        Ok(())
    }

    fn text(&mut self, value: &[u8]) -> io::Result<()> {
        self.bytes += value.len() as u64;
        fold_framed(&mut self.digest, value);
        Ok(())
    }

    fn absent(&mut self) -> io::Result<()> {
        fold(&mut self.digest, ABSENT);
        Ok(())
    }

    fn end(&mut self) -> io::Result<()> {
        Ok(())
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

    fn int(&mut self, value: i64) -> io::Result<()> {
        write!(self.0, " {value}")
    }

    fn real(&mut self, value: f64) -> io::Result<()> {
        write!(self.0, " {:016x}", value.to_bits())
    }

    fn text(&mut self, value: &[u8]) -> io::Result<()> {
        self.0.write_all(b" \"")?;
        for &c in value {
            if (0x20..0x7f).contains(&c) && c != b'"' && c != b'\\' {
                self.0.write_all(&[c])?;
            } else {
                write!(self.0, "\\x{c:02x}")?;
            }
        }
        self.0.write_all(b"\"")
    }

    fn absent(&mut self) -> io::Result<()> {
        write!(self.0, " -")
    }

    fn end(&mut self) -> io::Result<()> {
        writeln!(self.0)
    }
}
