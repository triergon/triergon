//! Original bounded DVD subpicture packet views and indexed-pixel expansion.
//!
//! Assembly accepts arbitrary SPU byte fragments. DvdPacket, raster and receiver
//! jobs accept one assembled SPU. SpuPacketJob retains complete multi-unit packets
//! and typed tails; ConfigJob reads original VobSub CodecPrivate text.
//! ProgramJob preserves ordered control facts and raw unknown operands. Native
//! crop projection borrows source rows using an explicitly supplied ARGB palette;
//! it does not establish palette generation or a complete decoder's policy.
//! Source pixels, control words and configuration remain observable; complete
//! presentation policy and display scheduling are separate.
mod assembly;
mod config;
mod native_palette;
mod packet;
mod program;
mod projection;
mod raster;
mod traditional;
mod wire;

pub use traditional::{
    DvdDisplayBoundary, DvdNativePresentation, ReceiverDisplayTimes, TraditionalLimits,
    TraditionalSpuJob, TraditionalStatus, TraditionalStorage, TraditionalSubpicture,
};

pub use config::{Canvas, ConfigJob, ConfigLimits, ConfigStatus, VobSubConfig};
pub use native_palette::{NativePalette, NativePaletteOrigin, native_d32_palette};
pub use packet::{
    SpuEncoding, SpuPacket, SpuPacketJob, SpuPacketLimits, SpuPacketStatus, SpuSpan, SpuTail,
    SpuTailReason,
};
pub use program::{
    ControlFacts, ControlPolicy, ControlProgram, ControlState, DisplayAction, DisplayControl,
    DisplayControlStep, DisplayControls, OpaqueCommandSpan, ProgramJob, ProgramLimits,
    ProgramStatus, TruncatedControl,
};
pub use projection::{
    NativeCrop, NativeCropMode, NativeDvdView, NativeProjectionJob, NativeSelection,
    ProjectedBitmap, ProjectionLimits, ProjectionStatus,
};
pub use raster::{DvdBitmap, DvdRasterJob, DvdRasterLimits, DvdRasterStatus};
pub use wire::{Command, Commands, Control, Controls, DvdArea, DvdDate, DvdPacket, FieldOffsets};

pub use assembly::{AssembledSpu, SpuAssembly, SpuAssemblyLimits, SpuAssemblyStatus, SpuPushError};

mod hd;
mod hd_receiver;
pub use hd::{
    HdAlphaOrigin, HdCommand, HdCommands, HdControl, HdControls, HdFacts, HdFields, HdFourPalette,
    HdLimits, HdPacket, HdPaletteJob, HdPolicy, HdSpan, HdSpuJob, HdStatus, HdSubpicture,
    native_hd_argb,
};
pub use hd_receiver::{HdReceiverJob, HdReceiverOutput, HdReceiverStatus};

mod hd_context;
mod hd_fragment;
pub use hd_context::{
    HdContextAlpha, HdContextJob, HdContextOrigin, HdContextOutput, HdContextPaletteJob,
    HdReceiverContext,
};
pub use hd_fragment::{
    HdFragmentHistory, HdFragmentIntegrity, HdFragmentJob, HdFragmentLimits, HdFragmentOutput,
    HdFragmentStamp, HdFragmentStartError, HdFragmentState, HdFragmentStatus, HdPacketClock,
    HdPendingEvidence, HdPhysicalPacket, HdReset,
};
