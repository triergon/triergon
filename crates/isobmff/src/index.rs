//! Segment and fragment indexes: `sidx` ([`SegmentIndex`]), `tfra`
//! ([`TrackFragmentRandomAccess`]) and `mfro` ([`MovieFragmentRandomAccessTail`]).
//!
//! Their entries are hints the file states about where segments and random access
//! samples are. Nothing here checks them against the media they point at.

mod sidx;
mod tfra;

pub use sidx::{SegmentIndex, SegmentIndexCursor, SegmentReference};
pub use tfra::{
    FragmentRandomAccessCursor, FragmentRandomAccessEntry, MovieFragmentRandomAccessTail,
    TrackFragmentRandomAccess,
};
