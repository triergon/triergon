//! The SILK layer (RFC 6716 section 4.2): a linear-prediction speech codec that runs at 8,
//! 12 or 16 kHz internally. [`SilkDecoder`] decodes the SILK frames of one Opus frame,
//! unmixes mid/side stereo and resamples the result to 48 kHz.

mod excitation;
mod frame;
mod gain;
mod header;
mod lpc;
mod ltp;
mod nlsf;
mod resampler;
mod stereo;
mod synthesis;
mod tables;
#[cfg(test)]
mod tests;

use crate::range::RangeDecoder;
use frame::{Coding, SideInfo};
use header::Header;
use resampler::Resampler;
use stereo::{StereoState, Weights};
use synthesis::ChannelState;

/// Linear-prediction order of NB and MB frames.
const ORDER_NB_MB: usize = 10;
/// Linear-prediction order of WB frames.
const MAX_ORDER: usize = 16;
/// Samples of one 20 ms SILK frame at 16 kHz.
const MAX_FRAME: usize = 320;
/// Samples of one Opus frame (up to 60 ms) at the internal rate.
const MAX_PACKET: usize = 3 * MAX_FRAME;

/// Why the SILK layer refused a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SilkError {
    Invalid(&'static str),
}

/// The internal sample rate of the SILK layer (RFC 6716 section 4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bandwidth {
    /// 8 kHz.
    Narrow,
    /// 12 kHz.
    Medium,
    /// 16 kHz, also used by hybrid frames.
    Wide,
}

impl Bandwidth {
    fn from_rate(rate: u32) -> Option<Bandwidth> {
        match rate {
            8000 => Some(Bandwidth::Narrow),
            12000 => Some(Bandwidth::Medium),
            16000 => Some(Bandwidth::Wide),
            _ => None,
        }
    }

    const fn khz(self) -> usize {
        match self {
            Bandwidth::Narrow => 8,
            Bandwidth::Medium => 12,
            Bandwidth::Wide => 16,
        }
    }

    /// Samples in a 5 ms subframe.
    const fn subframe_len(self) -> usize {
        5 * self.khz()
    }

    /// Order of the short-term prediction filter.
    const fn order(self) -> usize {
        match self {
            Bandwidth::Wide => MAX_ORDER,
            Bandwidth::Narrow | Bandwidth::Medium => ORDER_NB_MB,
        }
    }
}

/// How an Opus frame divides into SILK frames (RFC 6716 section 4.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    /// SILK frames per channel: 1 for 10 and 20 ms, 2 for 40 ms, 3 for 60 ms.
    frames: usize,
    /// Subframes per SILK frame: 2 for 10 ms, otherwise 4.
    subframes: usize,
}

impl Layout {
    fn from_duration_48k(duration: usize) -> Option<Layout> {
        let (frames, subframes) = match duration {
            480 => (1, 2),
            960 => (1, 4),
            1920 => (2, 4),
            2880 => (3, 4),
            _ => return None,
        };
        Some(Layout { frames, subframes })
    }
}

/// SILK decoder state of one Opus stream (at most one coded stereo pair).
#[derive(Clone, Debug)]
pub(crate) struct SilkDecoder {
    bandwidth: Option<Bandwidth>,
    coded_channels: usize,
    /// Mid (or mono) and side channel.
    channels: [ChannelState; 2],
    stereo: StereoState,
    /// Whether the previous SILK frame coded only the mid channel.
    prev_mid_only: bool,
    /// The right output channel replays the left resampler's input for one frame after
    /// a stereo to mono switch, so its history stays continuous.
    right_follows_mono: bool,
    resamplers: [Resampler; 2],
    /// Scratch: excitation pulses and reconstructed excitation of one SILK frame.
    pulses: [i32; MAX_FRAME],
    excitation: [f32; MAX_FRAME],
    /// Scratch: decoded mid and side signal of one SILK frame.
    decoded: [[f32; MAX_FRAME]; 2],
    /// Scratch: the whole Opus frame at the internal rate, after stereo unmixing.
    internal: [[f32; MAX_PACKET]; 2],
    /// Samples per channel in `internal`.
    internal_len: usize,
}

impl SilkDecoder {
    /// A decoder in the reset state. Its state has a fixed size.
    pub(crate) fn new() -> SilkDecoder {
        SilkDecoder {
            bandwidth: None,
            coded_channels: 1,
            channels: [ChannelState::new(), ChannelState::new()],
            stereo: StereoState::default(),
            prev_mid_only: false,
            right_follows_mono: false,
            resamplers: [Resampler::new(), Resampler::new()],
            pulses: [0; MAX_FRAME],
            excitation: [0.0; MAX_FRAME],
            decoded: [[0.0; MAX_FRAME]; 2],
            internal: [[0.0; MAX_PACKET]; 2],
            internal_len: 0,
        }
    }

    /// Drop all signal history (RFC 6716 section 4.5.2, with the stereo state reset of
    /// RFC 8251 section 3).
    pub(crate) fn reset(&mut self) {
        self.bandwidth = None;
        self.coded_channels = 1;
        for channel in &mut self.channels {
            channel.reset();
        }
        self.stereo = StereoState::default();
        self.prev_mid_only = false;
        self.right_follows_mono = false;
        for resampler in &mut self.resamplers {
            resampler.reset();
        }
    }

    /// Decode the SILK layer of one Opus frame. `internal_rate` is 8000, 12000 or 16000
    /// (16000 for hybrid frames), `duration_48k` is 480, 960, 1920 or 2880 and
    /// `coded_channels` is 2 for stereo packets. `out` has one slice per output channel
    /// (1 or 2), each `duration_48k` samples, filled at 48 kHz with full scale 1.0 from the
    /// front. Returns the samples written per channel: `duration_48k`, except 24 fewer for
    /// the first narrowband frame after a start (FFmpeg's resampler latency; see
    /// [`SilkDecoder::flush`]).
    pub(crate) fn decode(
        &mut self,
        rc: &mut RangeDecoder<'_>,
        internal_rate: u32,
        duration_48k: usize,
        coded_channels: usize,
        out: &mut [&mut [f32]],
    ) -> Result<usize, SilkError> {
        let bandwidth =
            Bandwidth::from_rate(internal_rate).ok_or(SilkError::Invalid("SILK internal rate"))?;
        let layout = Layout::from_duration_48k(duration_48k)
            .ok_or(SilkError::Invalid("SILK frame duration"))?;
        if !matches!(coded_channels, 1 | 2) {
            return Err(SilkError::Invalid("SILK channel count"));
        }
        if !matches!(out.len(), 1 | 2) || out.iter().any(|plane| plane.len() != duration_48k) {
            return Err(SilkError::Invalid("SILK output planes"));
        }
        self.configure(bandwidth, coded_channels);

        let header = Header::parse(rc, coded_channels, layout.frames);
        self.skip_lbrr(rc, &header, bandwidth, layout);
        let len = self.decode_frames(rc, &header, bandwidth, layout, out.len());
        self.internal_len = len;

        let [left, right] = &self.internal;
        let [r0, r1] = &mut self.resamplers;
        let (first, rest) = out.split_at_mut(1);
        let out_left = &mut *first[0];
        let produced = r0.process(&left[..len], out_left);
        if let Some(out_right) = rest.first_mut() {
            if coded_channels == 2 {
                r1.process(&right[..len], out_right);
            } else if self.right_follows_mono {
                r1.process(&left[..len], out_right);
            } else {
                out_right[..produced].copy_from_slice(&out_left[..produced]);
            }
        }
        self.right_follows_mono = false;
        Ok(produced)
    }

    /// Emit the 48 kHz samples the resampler still holds back (only after narrowband
    /// frames), as when the SILK layer stops or changes rate; returns the samples written
    /// per channel, at most 24. `out` has one slice per output channel.
    pub(crate) fn flush(&mut self, out: &mut [&mut [f32]]) -> usize {
        let [r0, r1] = &mut self.resamplers;
        let Some((left, rest)) = out.split_first_mut() else {
            return 0;
        };
        let produced = r0.flush(left);
        if let Some(right) = rest.first_mut() {
            if self.coded_channels == 2 {
                r1.flush(right);
            } else {
                // A mono stream drives only the left resampler.
                right[..produced].copy_from_slice(&left[..produced]);
                r1.reset();
            }
        }
        produced
    }

    /// Apply the state changes a new internal rate or channel count implies. A new rate
    /// restarts the resamplers but, as in FFmpeg, keeps the channel state (RFC 6716's
    /// reference decoder resets it); the caller flushes the resamplers first.
    fn configure(&mut self, bandwidth: Bandwidth, coded_channels: usize) {
        if self.bandwidth != Some(bandwidth) {
            for resampler in &mut self.resamplers {
                resampler.set_bandwidth(bandwidth);
            }
            self.bandwidth = Some(bandwidth);
        } else if coded_channels == 1 && self.coded_channels == 2 {
            self.right_follows_mono = true;
        }
        if coded_channels == 2 && self.coded_channels == 1 {
            // A new side channel starts from silence, with the left resampler's history.
            self.channels[1].reset();
            self.stereo.reset_prediction();
            self.resamplers[1] = self.resamplers[0].clone();
        }
        self.coded_channels = coded_channels;
    }

    /// Parse the LBRR frames (RFC 6716 section 4.2.5). Their redundant copy of the previous
    /// packet is only needed for loss recovery, so they are parsed and discarded; they do
    /// not touch the decoder state.
    fn skip_lbrr(
        &mut self,
        rc: &mut RangeDecoder<'_>,
        header: &Header,
        bandwidth: Bandwidth,
        layout: Layout,
    ) {
        let len = bandwidth.subframe_len() * layout.subframes;
        let mut prev_lag = [None; 2];
        for frame in 0..layout.frames {
            for channel in 0..self.coded_channels {
                if !header.lbrr[channel][frame] {
                    continue;
                }
                if self.coded_channels == 2 && channel == 0 {
                    Weights::parse(rc);
                    if !header.lbrr[1][frame] {
                        stereo::parse_mid_only(rc);
                    }
                }
                let coding = if frame > 0 && header.lbrr[channel][frame - 1] {
                    Coding::Conditional
                } else {
                    Coding::Independent { ltp_scaling: true }
                };
                let info = SideInfo::parse(
                    rc,
                    bandwidth,
                    layout.subframes,
                    true,
                    coding,
                    prev_lag[channel],
                );
                prev_lag[channel] = info.pitch_lag();
                excitation::parse_pulses(rc, info.frame_type, len, &mut self.pulses);
            }
        }
    }

    /// Decode the regular SILK frames into `self.internal`; returns the samples per channel.
    fn decode_frames(
        &mut self,
        rc: &mut RangeDecoder<'_>,
        header: &Header,
        bandwidth: Bandwidth,
        layout: Layout,
        out_channels: usize,
    ) -> usize {
        let len = bandwidth.subframe_len() * layout.subframes;
        let stereo = self.coded_channels == 2;
        let mut prev_lag = [None; 2];
        for frame in 0..layout.frames {
            let (weights, mid_only) = if stereo {
                let weights = Weights::parse(rc);
                (weights, !header.vad[1][frame] && stereo::parse_mid_only(rc))
            } else {
                (Weights::default(), false)
            };
            if stereo && !mid_only && self.prev_mid_only {
                // The side channel resumes after uncoded frames from a known, silent state.
                self.channels[1].reset();
            }
            for channel in 0..self.coded_channels {
                if channel == 1 && mid_only {
                    continue;
                }
                let coding = if frame == 0 {
                    Coding::Independent { ltp_scaling: true }
                } else if channel == 1 && self.prev_mid_only {
                    Coding::Independent { ltp_scaling: false }
                } else {
                    Coding::Conditional
                };
                let info = SideInfo::parse(
                    rc,
                    bandwidth,
                    layout.subframes,
                    header.vad[channel][frame],
                    coding,
                    prev_lag[channel],
                );
                prev_lag[channel] = info.pitch_lag();
                excitation::parse_pulses(rc, info.frame_type, len, &mut self.pulses);
                excitation::reconstruct(
                    &self.pulses[..len],
                    info.frame_type,
                    info.seed,
                    &mut self.excitation[..len],
                );
                self.channels[channel].decode(
                    &info,
                    &self.excitation[..len],
                    bandwidth,
                    layout.subframes,
                    &mut self.decoded[channel][..len],
                );
            }
            let range = frame * len..(frame + 1) * len;
            let [left, right] = &mut self.internal;
            let [mid, side] = &self.decoded;
            if stereo && out_channels == 2 {
                let side = (!mid_only).then(|| &side[..len]);
                self.stereo.unmix(
                    weights,
                    bandwidth,
                    &mid[..len],
                    side,
                    &mut left[range.clone()],
                    &mut right[range],
                );
            } else {
                self.stereo.mono(&mid[..len], &mut left[range]);
            }
            self.prev_mid_only = mid_only;
        }
        layout.frames * len
    }
}
