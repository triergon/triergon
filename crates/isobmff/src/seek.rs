//! Seek plans over the source indexes.
//!
//! [`SourceSeekPlanner`] lands on a track's preceding sync sample with the preroll the
//! caller asks for. [`MovieEditLocator`] maps a movie time through the edit list to a
//! media point, [`SourceCompositionLookup`] finds the sample presented at a media time,
//! and [`MovieSourceSeekPlanner`] combines both with the `roll` recovery groups into a
//! plan for a movie time. Plans state container facts only: a sync sample is not proof
//! of decoder recovery.

mod movie;
mod opus;
mod source;

pub use movie::{
    CompositionTarget, MovieEditLocator, MovieEmptyEdit, MoviePoint, MovieSeekClocks,
    MovieSeekLimits, MovieSeekResult, MovieSeekStatus, MovieSourcePoint, MovieSourceSeekContext,
    MovieSourceSeekPlanner, SourceCompositionLookup,
};
pub use opus::OpusRecoveryLanding;
pub(crate) use opus::OpusRecoverySeek;
pub use source::{
    SourceSeekLanding, SourceSeekLimits, SourceSeekPlan, SourceSeekPlanner, SourceSeekRequest,
    SourceSeekSamples,
};
