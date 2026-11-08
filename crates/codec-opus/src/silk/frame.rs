//! The side information at the start of each SILK frame (RFC 6716 sections 4.2.7.3 to
//! 4.2.7.7): frame type, gains, LSF indices, pitch parameters and the noise seed.

use super::Bandwidth;
use super::gain::GainIndex;
use super::ltp::PitchIndices;
use super::nlsf::NlsfIndices;
use super::tables::{
    FRAME_TYPE_ACTIVE, FRAME_TYPE_INACTIVE, GAIN_DELTA, GAIN_MSB, NLSF_INTERPOLATION, UNIFORM4,
    UNIFORM8,
};
use crate::range::RangeDecoder;

/// Signal type of a SILK frame (RFC 6716 Table 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SignalType {
    Inactive,
    Unvoiced,
    Voiced,
}

/// Quantization offset type of a SILK frame (RFC 6716 Table 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum QuantOffset {
    Low,
    High,
}

/// The jointly coded signal type and quantization offset type (RFC 6716 section 4.2.7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameType {
    pub(super) signal: SignalType,
    pub(super) offset: QuantOffset,
}

impl FrameType {
    fn parse(rc: &mut RangeDecoder<'_>, active: bool) -> FrameType {
        let symbol = if active {
            rc.icdf(&FRAME_TYPE_ACTIVE, 8) + 2
        } else {
            rc.icdf(&FRAME_TYPE_INACTIVE, 8)
        };
        let signal = match symbol >> 1 {
            0 => SignalType::Inactive,
            1 => SignalType::Unvoiced,
            _ => SignalType::Voiced,
        };
        let offset = if symbol & 1 == 0 {
            QuantOffset::Low
        } else {
            QuantOffset::High
        };
        FrameType { signal, offset }
    }

    /// Row of the tables that list inactive and unvoiced frames together, then voiced ones.
    pub(super) fn voicing(self) -> usize {
        usize::from(self.signal == SignalType::Voiced)
    }
}

/// How a SILK frame's parameters relate to the previous frame of the same kind (LBRR or
/// regular) in the same channel and Opus frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Coding {
    /// The first gain and the pitch lag are coded absolutely. `ltp_scaling` says whether
    /// the LTP scaling parameter is present (it is not after uncoded side frames).
    Independent { ltp_scaling: bool },
    /// The gain and (after a voiced frame) the pitch lag are deltas.
    Conditional,
}

/// The side information of one SILK frame.
#[derive(Clone, Copy, Debug)]
pub(super) struct SideInfo {
    pub(super) frame_type: FrameType,
    pub(super) gains: [GainIndex; 4],
    pub(super) nlsf: NlsfIndices,
    /// LSF interpolation factor of the first half of a 20 ms frame, Q2; 4 means none.
    pub(super) interpolation_q2: u8,
    pub(super) pitch: Option<PitchIndices>,
    /// Seed of the excitation's pseudorandom sign inversion.
    pub(super) seed: u32,
}

impl SideInfo {
    /// Parse the side information of a frame with `subframes` subframes. `active` is the
    /// frame's VAD flag (always set for LBRR frames); `prev_lag` is the lag index of the
    /// previous frame in the same chain if that frame was voiced.
    pub(super) fn parse(
        rc: &mut RangeDecoder<'_>,
        bandwidth: Bandwidth,
        subframes: usize,
        active: bool,
        coding: Coding,
        prev_lag: Option<i32>,
    ) -> SideInfo {
        let frame_type = FrameType::parse(rc, active);
        let mut gains = [GainIndex::Delta(0); 4];
        for (subframe, gain) in gains[..subframes].iter_mut().enumerate() {
            *gain = if subframe == 0 && matches!(coding, Coding::Independent { .. }) {
                let msb = rc.icdf(&GAIN_MSB[frame_type.signal as usize], 8);
                let lsb = rc.icdf(&UNIFORM8, 8);
                GainIndex::Absolute((msb << 3 | lsb) as u8)
            } else {
                GainIndex::Delta(rc.icdf(&GAIN_DELTA, 8) as u8)
            };
        }
        let nlsf = NlsfIndices::parse(rc, bandwidth, frame_type);
        let interpolation_q2 = if subframes == 4 {
            rc.icdf(&NLSF_INTERPOLATION, 8) as u8
        } else {
            4
        };
        let pitch = (frame_type.signal == SignalType::Voiced)
            .then(|| PitchIndices::parse(rc, bandwidth, subframes, coding, prev_lag));
        let seed = rc.icdf(&UNIFORM4, 8) as u32;
        SideInfo {
            frame_type,
            gains,
            nlsf,
            interpolation_q2,
            pitch,
            seed,
        }
    }

    /// The lag index a following conditionally coded frame predicts from.
    pub(super) fn pitch_lag(&self) -> Option<i32> {
        self.pitch.map(|pitch| pitch.lag_index)
    }
}
