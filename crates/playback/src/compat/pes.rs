//! Packet delivery with a selected transport-stream PES interpretation.

pub mod startup;

use crate::{Engine, Result, TrackKey, pes_feed::TsEngine};
use triergon_core::InputCursor;
use triergon_mpegts::{
    ParseLimits,
    clock::SourceTimePolicy,
    compat::pes::{PrivateAudioCarriage, RegisteredCarriage},
    psi::DescriptorNamespace,
};

/// Private audio PID and carriage to check against the active program map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrivateAudioSelection {
    /// Elementary stream PID.
    pub pid: u16,
    /// Packet framing associated with this PID.
    pub carriage: PrivateAudioCarriage,
}

/// The source events of one TS188 `program` and the demux PES of the selected
/// PIDs, cut and timed as FFmpeg's demuxer does. The input carrier order sets
/// the order of completed packets; at the logical end the rest drain by
/// ascending PID. Every selected packet shares the program's demux clock,
/// independently of the source clock view.
///
/// # Errors
///
/// `Invalid` for no selection, a PID outside 1 to 0x1ffe or a PID selected twice;
/// `Limit` for more than 64 selections; allocation failures.
pub fn open(
    input: InputCursor,
    program: u16,
    limits: ParseLimits,
    policy: SourceTimePolicy,
    namespace: DescriptorNamespace,
    selections: &[PidSelection],
) -> Result<Engine> {
    TsEngine::with_selections(input, program, limits, policy, namespace, selections).map(Engine::Ts)
}

/// The source events of one independent TS188 media segment with an in-band PAT
/// and PMT, and the demux PES of its tracks. The segment's single program is
/// discovered, each supported track's carriage is announced before its packets,
/// and other tracks stay source events. Make a new engine for each segment.
pub fn open_segment(
    input: InputCursor,
    limits: ParseLimits,
    policy: SourceTimePolicy,
    namespace: DescriptorNamespace,
) -> Result<Engine> {
    TsEngine::discovered_segment(input, limits, policy, namespace).map(Engine::Ts)
}

/// [`open`] for one private-audio PID, its carriage checked against each active
/// program map.
pub fn open_private_audio(
    input: InputCursor,
    program: u16,
    limits: ParseLimits,
    policy: SourceTimePolicy,
    selection: PrivateAudioSelection,
) -> Result<Engine> {
    open(
        input,
        program,
        limits,
        policy,
        DescriptorNamespace::Unspecified,
        &[PidSelection {
            pid: selection.pid,
            carriage: Carriage::PrivateAudio(selection.carriage),
        }],
    )
}

/// Explicit packet interpretation for one selected PID of a TS program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carriage {
    /// A registered stream type, such as AVC, HEVC or ADTS AAC.
    Registered(RegisteredCarriage),
    /// AC-3, E-AC-3 or DVB audio in private-stream PES.
    PrivateAudio(PrivateAudioCarriage),
    /// DVB subtitles.
    DvbSubtitles,
    /// Resolve the established AAC and private-audio profiles from each active PMT.
    AudioFromPmt,
}

/// Snapshot of a track's admitted carriage for one active source PMT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmittedCarriage {
    /// The track.
    pub track: TrackKey,
    /// The PMT epoch the carriage was admitted in.
    pub configuration_epoch: u64,
    /// The resolved registered or private-audio carriage.
    pub carriage: Carriage,
}

/// One selected PID of a TS program and how its packets are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PidSelection {
    /// Elementary stream PID.
    pub pid: u16,
    /// How its PES packets are interpreted.
    pub carriage: Carriage,
}

impl PidSelection {
    /// A PID with a registered carriage.
    pub const fn registered(pid: u16, carriage: RegisteredCarriage) -> Self {
        Self {
            pid,
            carriage: Carriage::Registered(carriage),
        }
    }
}
