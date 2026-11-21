//! MP4 and QuickTime playback. [`Mp4Open`] reads a movie's metadata, [`Mp4IndexJob`]
//! indexes it into a [`PreparedMp4`] plan, and [`Engine::mp4`](crate::Engine::mp4) or
//! [`Mp4RangeOpen`] plays it; the seek jobs, catalogue replacement and the events the
//! engine delivers are here too.

pub(crate) mod catalogue;
pub(crate) mod open;
pub(crate) mod plan;
pub(crate) mod sample_sort;

pub use crate::engine::mp4::progressive::ProgressiveLimits;
pub use crate::event::{Mp4EmptyPacket, Mp4Event, Mp4MissingSample, Mp4Packet, Mp4ShortPacket};
pub use crate::player::{
    Mp4DemuxSeekCommand, Mp4DemuxSeekJob, Mp4DemuxSeekRequest, Mp4RangeOpen, Mp4RangePlayer,
    Mp4RangeSeek, Mp4RangeSeekJob, Mp4SyncSeekCommand, Mp4SyncSeekJob, Mp4SyncSeekRequest,
};
pub use catalogue::{
    CatalogueBoundary, CatalogueCommit, Mp4Catalogue, Mp4CatalogueMode, RetiredCatalogue,
};
pub use open::{
    MetadataReadScope, Movie, MovieTrack, Mp4Open, OpenLimits, OpenProgress, OpenedMp4,
};
pub use plan::{
    MovieSeekCommand, MovieSeekJob, MovieSeekOutcome, Mp4Configuration, Mp4IndexJob, PlanLimits,
    PlanTrack, PreparedMp4, SourceRow, TrackMetadata, TrackSnapshot,
};
