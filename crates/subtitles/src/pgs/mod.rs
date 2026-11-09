//! Bounded PGS wire, object raster and display-set decoding.
//!
//! Owned source facts and cache/update/reset state stay distinct from the
//! receiver's ARGB and placement projections ([`crate::compat::pgs`]).
mod display;
pub(crate) mod display_model;
mod display_palette;
mod display_render;
mod display_work;
pub(crate) mod indexed_argb;
mod object;
#[cfg(feature = "reference-trace")]
pub(crate) mod projector;
mod raster;
pub(crate) mod receiver_decoder;
pub(crate) mod recovery;
mod recovery_role;
mod segment;

pub use display::{
    DisplayConfig, DisplayDecoder, DisplayLimits, DisplayPushError, DisplayStatus,
    IndexedDisplayDecoder,
};
pub(crate) use display_model::ColourMatrix;
pub use display_model::{
    CompositionState, DisplayObject, DisplaySet, IndexedDisplayObject, IndexedDisplaySet,
    PacketClock, PaletteSnapshot, PresentationFacts, SegmentFact, SegmentStamp, SourceColour,
    VisibleGeometry,
};
pub use indexed_argb::{ArgbStorage, ArgbWrite, ArgbWriter, IndexedArgb};
pub use object::{AssemblyLimits, AssemblyStatus, EncodedObject, ObjectAssembly, ObjectPushError};
pub use raster::{IndexedObject, RasterJob, RasterLimits, RasterStatus};
pub(crate) use recovery::RecoveryEof;
pub use recovery::{
    IndexedRecoveredPacket, ReceiverObservations, RecoveredEvent, RecoveredPacket, RecoveredRect,
    RecoveryStatus,
};
pub use segment::{
    Composition, CompositionObject, CompositionObjects, Crop, Envelope, ObjectFragment,
    ObjectStart, Palette, PaletteEntry, Segment, SegmentIter, SupClock, Window, Windows,
};
