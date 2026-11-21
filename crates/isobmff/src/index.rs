//! Segment and fragment indexes: `sidx`, `tfra` and `mfro`.
//!
//! Their entries are hints the file states about where segments and fragments start.
//! Nothing here checks them against the media they point at.

mod sidx;
mod tfra;

pub use sidx::{SegmentIndex, SegmentIndexCursor, SegmentReference};
pub use tfra::{
    FragmentRandomAccessCursor, FragmentRandomAccessEntry, MovieFragmentRandomAccessTail,
    TrackFragmentRandomAccess,
};
