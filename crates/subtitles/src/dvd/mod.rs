//! DVD subpictures as their bytes state them.
//!
//! [`SpuAssembly`] joins arbitrary byte fragments into one unit, [`DvdPacket`]
//! views it, [`SpuPacketJob`] splits a packet into units and typed tails, and
//! [`ConfigJob`] reads VobSub `CodecPrivate` text. [`ProgramJob`] keeps every
//! control fact and raw unknown operand, [`DvdRasterJob`] expands one area's
//! 2-bit pixels and [`HdSpuJob`] reads an extended (8-bit) unit. None of them
//! applies a palette, crop or presentation policy.
//!
//! [`program_stream`] reads the MPEG-2 program streams that carry subpictures
//! (`.vob`, VobSub `.sub`) and [`vobsub`] the lines of a VobSub `.idx`. The
//! reference's complete receiver over packets, its demuxers' packets and its
//! palette sources are in [`crate::compat::dvd`].
mod assembly;
mod config;
pub(crate) mod demux;
pub(crate) mod hd;
mod packet;
mod program;
pub mod program_stream;
mod raster;
pub(crate) mod receiver;
pub mod vobsub;
mod wire;

pub use assembly::{AssembledSpu, SpuAssembly, SpuAssemblyLimits, SpuAssemblyStatus, SpuPushError};
pub use config::{Canvas, ConfigJob, ConfigLimits, ConfigStatus, VobSubConfig};
pub use hd::{
    HdCommand, HdCommands, HdControl, HdControls, HdFacts, HdFields, HdLimits, HdPacket,
    HdPaletteJob, HdPolicy, HdSpan, HdSpuJob, HdStatus, HdSubpicture,
};
pub use packet::{
    SpuEncoding, SpuPacket, SpuPacketJob, SpuPacketLimits, SpuPacketStatus, SpuSpan, SpuTail,
    SpuTailReason,
};
pub use program::{
    ControlFacts, ControlProgram, ControlState, DisplayAction, DisplayControl, DisplayControlStep,
    DisplayControls, OpaqueCommandSpan, ProgramJob, ProgramLimits, ProgramStatus,
};
pub use raster::{DvdBitmap, DvdRasterJob, DvdRasterLimits, DvdRasterStatus};
pub use wire::{Command, Commands, Control, Controls, DvdArea, DvdDate, DvdPacket, FieldOffsets};
