//! Original bounded DVD subpicture packet views and indexed-pixel expansion.
//!
//! Assembly accepts arbitrary SPU byte fragments. DvdPacket, raster and receiver
//! jobs accept one assembled SPU. SpuPacketJob retains complete multi-unit packets
//! and typed tails; ConfigJob reads original VobSub CodecPrivate text.
//! ProgramJob preserves ordered control facts and raw unknown operands. The
//! receiver's crop and palette ([`crate::compat::dvd`]) borrow source rows using
//! an explicitly supplied ARGB palette; they do not establish palette generation
//! or a complete decoder's policy.
//! Source pixels, control words and configuration remain observable; complete
//! presentation policy and display scheduling are separate.
mod assembly;
mod config;
mod packet;
mod program;
pub(crate) mod projection;
mod raster;
pub(crate) mod receiver_palette;
pub(crate) mod traditional;
mod wire;

pub use traditional::{
    DvdDisplayBoundary, ReceiverDisplayTimes, TraditionalLimits, TraditionalSpuJob,
    TraditionalStatus, TraditionalStorage, TraditionalSubpicture,
};

pub use config::{Canvas, ConfigJob, ConfigLimits, ConfigStatus, VobSubConfig};
pub use packet::{
    SpuEncoding, SpuPacket, SpuPacketJob, SpuPacketLimits, SpuPacketStatus, SpuSpan, SpuTail,
    SpuTailReason,
};
pub use program::{
    ControlFacts, ControlPolicy, ControlProgram, ControlState, DisplayAction, DisplayControl,
    DisplayControlStep, DisplayControls, OpaqueCommandSpan, ProgramJob, ProgramLimits,
    ProgramStatus, TruncatedControl,
};
pub(crate) use projection::{CropJob, CropMode, CroppedView};
pub use projection::{ProjectedBitmap, ProjectionLimits, ProjectionStatus};
pub use raster::{DvdBitmap, DvdRasterJob, DvdRasterLimits, DvdRasterStatus};
pub(crate) use receiver_palette::{Palette, resolve_palette};
pub use wire::{Command, Commands, Control, Controls, DvdArea, DvdDate, DvdPacket, FieldOffsets};

pub use assembly::{AssembledSpu, SpuAssembly, SpuAssemblyLimits, SpuAssemblyStatus, SpuPushError};

pub(crate) mod hd;
mod hd_receiver;
pub(crate) use hd::hd_argb;
pub use hd::{
    HdAlphaOrigin, HdCommand, HdCommands, HdControl, HdControls, HdFacts, HdFields, HdFourPalette,
    HdLimits, HdPacket, HdPaletteJob, HdPolicy, HdSpan, HdSpuJob, HdStatus, HdSubpicture,
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
