//! Independently authored EN300743 method-0 bitmap service operation and raster.
//! Experimental; not a full DVB subtitle decoder.
//! Complete packets come from the caller's demux/core input adapter. Pixel/RLE,
//! service state, CLUT source facts and immutable display events remain local.

mod bits;
mod boundary;
mod colour;
mod coverage;
mod display;
mod display_model;
mod display_render;
mod display_state;
mod model;
pub(crate) mod pes_parser;
mod pixel;
mod raster;
pub(crate) mod receiver_decoder;

pub use boundary::{
    ClockOrigin, ClockValue, ConfigurationSource, PacketIdentity, PacketSourceFacts, PacketTiming,
    SourceClock, SourceConfiguration,
};
pub use colour::{
    ColourProjection, ColourSource, PaletteEntry, SOURCE_PROJECTION, STUDIO_UNREPAIRED_PROJECTION,
};
pub use display::Decoder;
pub use display_model::{
    DecodeStatus, DisplayAction, DisplayDefinition, DisplayEvent, DisplayLimits, DisplayProgress,
    DisplayRegion, DisplayWindow, DvbPacket, ObjectRetention, OpaqueSegment, PacketEnvelope,
    PaletteSnapshot, Service,
};
pub use model::{
    ClipRect, Depth, Failure, FieldParity, FieldSpec, IndexedPlane, InitialPlane, MapTables,
    PaletteFacts, Point, Progress, RasterLimits, RasterSpec, Size, SourceFacts, Status,
    StoredGeometry,
};
pub use raster::Raster;
