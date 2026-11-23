//! The records the operations produce, in the order `harness/reference_ass.h`
//! produces them: a kind and values, integers, doubles, byte strings and absent
//! values (a missing string, outline or scroll).

use std::io;
use triergon_harness_runner::records::{self, FNV_OFFSET, Sink, fnv};
use triergon_raster::{Outline, Rect, SegmentKind};

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
    /// The play resolution with its defaults applied (`Info::play_res`).
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

impl records::Kind for Kind {
    fn tag(self) -> u64 {
        self as u64
    }

    fn name(self) -> &'static str {
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

/// The value of a drawing's outline.
pub trait Values: Sink<Kind> {
    /// A drawing's control box, then its point and segment counts and the
    /// FNV-1a hash of each point's x and y as little-endian 32-bit values and
    /// each segment as one byte (its order, plus 4 when it closes a contour);
    /// absent for a drawing libass rejects.
    fn outline(&mut self, drawing: Option<(&Outline, Rect)>) -> io::Result<()> {
        let Some((outline, cbox)) = drawing else {
            return self.absent();
        };
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

impl<S: Sink<Kind>> Values for S {}
