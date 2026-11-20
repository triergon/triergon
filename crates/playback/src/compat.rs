//! Compatibility policies for playback.

pub mod matroska;

pub mod mov {
    //! MOV packet order and edit clocks using the
    //! [ISO BMFF MOV policy](triergon_isobmff::compat::mov).

    use crate::{
        Result,
        mp4_open::Movie,
        mp4_plan::{Mp4IndexJob, PlanLimits},
    };
    use triergon_isobmff::compat::mov::edits::EncodedEditPlan;

    /// Index regular edited packets from front metadata and one finite media
    /// extent. Packets follow source byte order and carry the encoded trim.
    /// Unpresented source samples remain in the physical index.
    pub fn forward_encoded_edits<'a>(
        movie: &'a Movie,
        plans: &'a [Option<EncodedEditPlan>],
        limits: PlanLimits,
    ) -> Result<Mp4IndexJob<'a>> {
        Mp4IndexJob::for_forward_encoded_edits(movie, plans, limits)
    }
}

/// Packet delivery with a selected transport-stream PES interpretation.
pub mod pes {
    use crate::{Engine, Result, TsNativeCarriage, TsNativeTrackSelection};
    use triergon_core::InputCursor;
    use triergon_mpegts::{
        PacketFormat, ParseLimits, clock::SourceTimePolicy, compat::pes::PrivateAudioCarriage,
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
            TsNativeTrackSelection {
                pid: selection.pid,
                carriage: TsNativeCarriage::PrivateAudio(selection.carriage),
            },
        )
    }
}
