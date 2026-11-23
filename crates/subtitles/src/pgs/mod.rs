//! PGS (Blu-ray presentation graphics) segments and FFmpeg's receiver.
//!
//! [`SegmentIter`] splits a packet or SUP stream into typed segments. The
//! receiver that decodes them as the reference does is
//! [`compat::pgs::Decoder`](crate::compat::pgs::Decoder).
pub(crate) mod receiver;
mod segment;

pub use receiver::Clock;
pub use segment::{
    Composition, CompositionObject, CompositionObjects, Crop, Envelope, ObjectFragment,
    ObjectStart, Palette, PaletteEntry, Segment, SegmentIter, SupClock, Window, Windows,
};
