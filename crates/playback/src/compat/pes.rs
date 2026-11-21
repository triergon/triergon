//! Packet delivery with a selected transport-stream PES interpretation.

pub mod startup;

use crate::{Engine, Result, TrackKey};
use triergon_core::InputCursor;
use triergon_mpegts::{
    PacketFormat, ParseLimits,
    clock::SourceTimePolicy,
    compat::pes::{PrivateAudioCarriage, RegisteredCarriage},
};

/// Private audio PID and carriage to check against the active program map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrivateAudioSelection {
    /// Elementary stream PID.
    pub pid: u16,
    /// Packet framing associated with this PID.
    pub carriage: PrivateAudioCarriage,
}

/// Deliver the selected private-audio PES packets from a TS188 source.
pub fn open_private_audio(
    input: InputCursor,
    program: u16,
    limits: ParseLimits,
    policy: SourceTimePolicy,
    selection: PrivateAudioSelection,
) -> Result<Engine> {
    Engine::ts_with_native(
        input,
        PacketFormat::Ts188,
        program,
        limits,
        policy,
        PidSelection {
            pid: selection.pid,
            carriage: Carriage::PrivateAudio(selection.carriage),
        },
    )
}

/// Explicit packet interpretation for one selected PID of a TS program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carriage {
    Registered(RegisteredCarriage),
    PrivateAudio(PrivateAudioCarriage),
    DvbSubtitles,
    /// Resolve the established AAC and private-audio profiles from each active PMT.
    AudioFromPmt,
}

/// Snapshot of a track's admitted carriage for one active source PMT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmittedCarriage {
    pub track: TrackKey,
    pub configuration_epoch: u64,
    /// The resolved registered or private-audio carriage.
    pub carriage: Carriage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PidSelection {
    pub pid: u16,
    pub carriage: Carriage,
}
