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
mod native_palette;
mod native_palette_buffer;
mod native_view;
mod pes_parser;
mod pixel;
mod raster;

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

pub use native_view::{
    D32_PIN, D32_VIEW, NativeFailure, NativeGeometryPolicy, NativeOmittedRegion, NativePalette,
    NativePublication, PublicationReason, ReceiverView,
};

pub use pes_parser::{NativePesFraming, NativePesPacket, NativePesParser, NativePesProgress};
