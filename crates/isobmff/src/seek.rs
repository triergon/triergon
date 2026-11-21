//! Seek plans over a track's sample tables and fragments, without reading media.
//!
//! [`SourceSeekPlanner`] lands on the sync sample at or before a target sample with the
//! preroll the caller asks for. [`MovieEditLocator`] maps a movie time through the edit
//! list, [`SourceCompositionLookup`] finds the sample presented at a media time, and
//! [`MovieSourceSeekPlanner`] combines both with the recovery groups into a plan for a
//! movie time. A sync sample is what the container states, not proof of decoder recovery.

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
