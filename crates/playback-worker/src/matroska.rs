//! Feed limits and the setup loop shared by the Matroska runners (`feed-` and
//! `mkv-audio-`).

use crate::runner::Result;
use triergon_core::WorkBudget;
use triergon_framing::{
    AacConfigLimits, AvcAnnexBLimits, AvcConfigLimits, BuildStep, HvcConfigLimits,
};
use triergon_playback::matroska_feed::{
    MatroskaDolbyLimits, MatroskaFeedLimits, MatroskaHevcDolbyLimits,
};

/// Work granted to each step of a session.
pub const WORK: u64 = 4096;

const ANNEX_B: AvcAnnexBLimits = AvcAnnexBLimits {
    maximum_packet_bytes: 32768,
    maximum_output_bytes: 49152,
    maximum_nals: 64,
};

/// Limits of the AVC and AAC feeds.
pub const AVC_AAC: MatroskaFeedLimits = MatroskaFeedLimits {
    avc_config: AvcConfigLimits {
        maximum_record_bytes: 8192,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    },
    avc_packet: ANNEX_B,
    aac_config: AacConfigLimits {
        maximum_record_bytes: 128,
        maximum_pce_elements: 32,
        maximum_comment_bytes: 64,
    },
    aac_output_bytes: 8191,
};

/// Limits of the HEVC, AC-3 and E-AC-3 feeds.
pub const HEVC_DOLBY: MatroskaHevcDolbyLimits = MatroskaHevcDolbyLimits {
    hevc_config: HvcConfigLimits {
        maximum_record_bytes: 8192,
        maximum_arrays: 16,
        maximum_nals: 64,
        maximum_nal_bytes: 4096,
    },
    hevc_packet: ANNEX_B,
    dolby_packet: MatroskaDolbyLimits {
        maximum_packet_bytes: 16384,
        maximum_syncframes: 8,
    },
};

/// Step a feed setup or packet job until it is ready, at most 4096 times.
pub fn ready<T>(
    mut step: impl FnMut(&mut WorkBudget) -> triergon_playback::Result<BuildStep<T>>,
) -> Result<T> {
    for _ in 0..4096 {
        match step(&mut WorkBudget::new(WORK))? {
            BuildStep::Ready(value) => return Ok(value),
            BuildStep::Yield => {}
            BuildStep::Complete => break,
        }
    }
    Err("selected feed did not finish".into())
}
