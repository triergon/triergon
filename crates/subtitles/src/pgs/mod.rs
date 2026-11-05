//! Bounded PGS wire, object raster and display-set decoding.
//!
//! Owned source facts and cache/update/reset state stay distinct from explicit
//! native ARGB and placement projections. Complete parity and numerical
//! qualification remain open.
mod display;
mod display_model;
mod display_palette;
mod display_render;
mod display_work;
mod indexed_argb;
mod native_compatibility;
mod native_projection;
mod native_recovery;
mod object;
mod raster;
mod recovery_role;
mod segment;

pub use display::{
    DisplayConfig, DisplayDecoder, DisplayLimits, DisplayPushError, DisplayStatus,
    IndexedDisplayDecoder,
};
pub use display_model::{
    CompositionState, DisplayObject, DisplaySet, IndexedDisplayObject, IndexedDisplaySet,
    NativeClockSource, NativeColourOrigin, NativeMatrix, PacketClock, PaletteSnapshot,
    PresentationFacts, SegmentFact, SegmentStamp, SourceColour, VisibleGeometry,
};
pub use indexed_argb::{ArgbStorage, ArgbWrite, ArgbWriter, IndexedArgb, NativeArgbView};
pub use native_projection::{
    NATIVE_PROJECTION, NativeCanvas, NativeDisplay, NativeEof, NativeEofBehavior, NativePacket,
    NativeProjector, NativeRasterOrigin, NativeRect, ProjectionStatus,
};
pub use native_recovery::{
    IndexedNativeRecoveryDecoder, IndexedRecoveredPacket, NATIVE_RECOVERY, NativeRecoveryDecoder,
    ReceiverObservations, RecoveredEvent, RecoveredPacket, RecoveredRect, RecoveryStatus,
};
pub use object::{AssemblyLimits, AssemblyStatus, EncodedObject, ObjectAssembly, ObjectPushError};
pub use raster::{IndexedObject, RasterJob, RasterLimits, RasterStatus};
pub use segment::{
    Composition, CompositionObject, CompositionObjects, Crop, Envelope, ObjectFragment,
    ObjectStart, Palette, PaletteEntry, Segment, SegmentIter, SupClock, Window, Windows,
};

pub use native_compatibility::{
    NATIVE_COMPATIBILITY, NativeCompatibilityDecoder, NativeCompatibilityEof,
    NativeCompatibilityPacket,
};
