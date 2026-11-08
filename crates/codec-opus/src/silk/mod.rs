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

use crate::packet::{self, FrameDuration, Mode, Toc};
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

/// The SILK layer of an Opus frame as its TOC describes it: the internal bandwidth, the
/// SILK frames it divides into and the coded channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SilkConfig {
    bandwidth: Bandwidth,
    layout: Layout,
    /// 1, or 2 for stereo packets.
    coded_channels: usize,
}

impl SilkConfig {
    /// The SILK layer of frames with `toc`, `None` for CELT-only frames. Hybrid frames
    /// code SILK at wideband.
    pub(crate) fn of(toc: Toc) -> Option<SilkConfig> {
        if toc.mode == Mode::Celt {
            return None;
        }
        let bandwidth = match toc.bandwidth {
            packet::Bandwidth::Narrow => Bandwidth::Narrow,
            packet::Bandwidth::Medium => Bandwidth::Medium,
            _ => Bandwidth::Wide,
        };
        let (frames, subframes) = match toc.duration {
            FrameDuration::Ms10 => (1, 2),
            FrameDuration::Ms20 => (1, 4),
            FrameDuration::Ms40 => (2, 4),
            FrameDuration::Ms60 => (3, 4),
            // No TOC of a SILK or hybrid frame names these.
            FrameDuration::Ms2_5 | FrameDuration::Ms5 => return None,
        };
        Some(SilkConfig {
            bandwidth,
            layout: Layout { frames, subframes },
            coded_channels: toc.channels(),
        })
    }

    /// The internal sample rate.
    pub(crate) fn rate(self) -> u32 {
        1000 * self.bandwidth.khz() as u32
    }

    /// Samples per channel of the frame at 48 kHz.
    pub(crate) fn duration_48k(self) -> usize {
        // A subframe lasts 5 ms.
        self.layout.frames * self.layout.subframes * 240
    }

    /// The 48 kHz samples a decode after a start comes out short by, which
    /// [`SilkDecoder::flush`] later returns.
    pub(crate) fn start_latency(self) -> usize {
        Resampler::start_latency(self.bandwidth)
    }

    /// The pulses [`SilkLayer::parse`] stores for one Opus frame: whole shell blocks of
    /// every coded channel of every SILK frame.
    pub(crate) fn pulses_len(self) -> usize {
        self.layout.frames * self.coded_channels * self.padded_len()
    }

    /// Samples per channel of one SILK frame at the internal rate.
    fn frame_len(self) -> usize {
        self.bandwidth.subframe_len() * self.layout.subframes
    }

    /// The pulses of one channel of one SILK frame, in whole shell blocks.
    fn padded_len(self) -> usize {
        self.frame_len().next_multiple_of(excitation::BLOCK)
    }

    /// Where the pulses of `channel` of SILK frame `frame` lie in an Opus frame's pulses.
    fn pulses_at(self, frame: usize, channel: usize) -> core::ops::Range<usize> {
        let start = (frame * self.coded_channels + channel) * self.padded_len();
        start..start + self.padded_len()
    }
}

/// Most pulses [`SilkLayer::parse`] stores for one packet: 120 ms of two channels at
/// 16 kHz, in whole shell blocks (a 10 ms medium-band frame pads 120 samples to 128).
pub(crate) const MAX_PACKET_PULSES: usize = 2 * 120 * 16;

/// The symbols of one SILK frame, read ahead of its synthesis.
#[derive(Clone, Copy, Debug, Default)]
struct FrameSymbols {
    weights: Weights,
    mid_only: bool,
    /// Side information of each coded channel; none for an uncoded side channel.
    info: [Option<SideInfo>; 2],
}

/// The SILK layer of one Opus frame as read from its range coder, apart from the pulses,
/// which go to a buffer of [`SilkConfig::pulses_len`]: everything synthesis needs. The
/// reading depends on no decoder state, so every frame of a packet can be read before
/// the first is synthesized.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SilkLayer {
    frames: [FrameSymbols; 3],
}

impl SilkLayer {
    /// Read the SILK layer of one Opus frame from `rc`, leaving `rc` after it.
    pub(crate) fn parse(rc: &mut RangeDecoder<'_>, config: SilkConfig, pulses: &mut [i32]) -> Self {
        let SilkConfig {
            bandwidth,
            layout,
            coded_channels,
        } = config;
        let header = Header::parse(rc, coded_channels, layout.frames);
        // The first pulses serve as scratch for the discarded LBRR frames.
        skip_lbrr(rc, &header, config, &mut pulses[config.pulses_at(0, 0)]);
        let mut layer = SilkLayer::default();
        let mut prev_lag = [None; 2];
        let mut prev_mid_only = false;
        for (frame, symbols) in layer.frames[..layout.frames].iter_mut().enumerate() {
            if coded_channels == 2 {
                (symbols.weights, symbols.mid_only) = read_stereo(rc, &header, frame);
            }
            for channel in 0..coded_channels {
                if channel == 1 && symbols.mid_only {
                    continue;
                }
                symbols.info[channel] = Some(read_channel(
                    rc,
                    bandwidth,
                    layout,
                    header.vad[channel][frame],
                    coding(frame, channel, prev_mid_only),
                    &mut prev_lag[channel],
                    &mut pulses[config.pulses_at(frame, channel)],
                ));
            }
            prev_mid_only = symbols.mid_only;
        }
        layer
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
    /// Scratch: reconstructed excitation of one SILK frame.
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

    /// Synthesize the SILK layer `layer` of one Opus frame, with its `pulses`, as
    /// [`SilkLayer::parse`] read them with `config`. `out` has one slice per output
    /// channel (1 or 2), each [`SilkConfig::duration_48k`] samples, filled at 48 kHz with
    /// full scale 1.0 from the front. Returns the samples written per channel: the
    /// duration, except [`SilkConfig::start_latency`] fewer for the first frame after a
    /// start (FFmpeg's resampler latency; see [`SilkDecoder::flush`]).
    pub(crate) fn decode(
        &mut self,
        config: SilkConfig,
        layer: &SilkLayer,
        pulses: &[i32],
        out: &mut [&mut [f32]],
    ) -> usize {
        debug_assert!(matches!(out.len(), 1 | 2));
        debug_assert!(out.iter().all(|plane| plane.len() == config.duration_48k()));
        self.configure(config.bandwidth, config.coded_channels);
        let len = self.decode_frames(config, layer, pulses, out.len());
        self.internal_len = len;

        let [left, right] = &self.internal;
        let [r0, r1] = &mut self.resamplers;
        let (first, rest) = out.split_at_mut(1);
        let out_left = &mut *first[0];
        let produced = r0.process(&left[..len], out_left);
        if let Some(out_right) = rest.first_mut() {
            if config.coded_channels == 2 {
                r1.process(&right[..len], out_right);
            } else if self.right_follows_mono {
                r1.process(&left[..len], out_right);
            } else {
                out_right[..produced].copy_from_slice(&out_left[..produced]);
            }
        }
        self.right_follows_mono = false;
        produced
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

    /// Synthesize the regular SILK frames into `self.internal`; returns the samples per
    /// channel.
    fn decode_frames(
        &mut self,
        config: SilkConfig,
        layer: &SilkLayer,
        pulses: &[i32],
        out_channels: usize,
    ) -> usize {
        let SilkConfig {
            bandwidth, layout, ..
        } = config;
        let len = config.frame_len();
        let stereo = config.coded_channels == 2;
        for (frame, symbols) in layer.frames[..layout.frames].iter().enumerate() {
            if stereo && !symbols.mid_only && self.prev_mid_only {
                // The side channel resumes after uncoded frames from a known, silent state.
                self.channels[1].reset();
            }
            for (channel, info) in symbols.info.iter().enumerate() {
                let Some(info) = info else {
                    continue;
                };
                let pulses = &pulses[config.pulses_at(frame, channel)];
                excitation::reconstruct(
                    &pulses[..len],
                    info.frame_type,
                    info.seed,
                    &mut self.excitation[..len],
                );
                self.channels[channel].decode(
                    info,
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
                let side = (!symbols.mid_only).then(|| &side[..len]);
                self.stereo.unmix(
                    symbols.weights,
                    bandwidth,
                    &mid[..len],
                    side,
                    &mut left[range.clone()],
                    &mut right[range],
                );
            } else {
                self.stereo.mono(&mid[..len], &mut left[range]);
            }
            self.prev_mid_only = symbols.mid_only;
        }
        layout.frames * len
    }
}

/// How the side information of `channel` in SILK frame `frame` is coded, given whether
/// the previous frame coded only the mid channel.
fn coding(frame: usize, channel: usize, prev_mid_only: bool) -> Coding {
    if frame == 0 {
        Coding::Independent { ltp_scaling: true }
    } else if channel == 1 && prev_mid_only {
        Coding::Independent { ltp_scaling: false }
    } else {
        Coding::Conditional
    }
}

/// The stereo prediction weights of SILK frame `frame` and whether it codes only the mid
/// channel.
fn read_stereo(rc: &mut RangeDecoder<'_>, header: &Header, frame: usize) -> (Weights, bool) {
    let weights = Weights::parse(rc);
    (weights, !header.vad[1][frame] && stereo::parse_mid_only(rc))
}

/// The side information and excitation pulses of one channel of one SILK frame;
/// `prev_lag` carries the pitch lag of the channel's previous frame to the next.
fn read_channel(
    rc: &mut RangeDecoder<'_>,
    bandwidth: Bandwidth,
    layout: Layout,
    active: bool,
    coding: Coding,
    prev_lag: &mut Option<i32>,
    pulses: &mut [i32],
) -> SideInfo {
    let info = SideInfo::parse(rc, bandwidth, layout.subframes, active, coding, *prev_lag);
    *prev_lag = info.pitch_lag();
    let len = bandwidth.subframe_len() * layout.subframes;
    excitation::parse_pulses(rc, info.frame_type, len, pulses);
    info
}

/// Parse the LBRR frames (RFC 6716 section 4.2.5). Their redundant copy of the previous
/// packet is only needed for loss recovery, so they are parsed and discarded; they do not
/// touch the decoder state.
fn skip_lbrr(rc: &mut RangeDecoder<'_>, header: &Header, config: SilkConfig, pulses: &mut [i32]) {
    let SilkConfig {
        bandwidth,
        layout,
        coded_channels,
    } = config;
    let mut prev_lag = [None; 2];
    for frame in 0..layout.frames {
        for channel in 0..coded_channels {
            if !header.lbrr[channel][frame] {
                continue;
            }
            if coded_channels == 2 && channel == 0 {
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
            read_channel(
                rc,
                bandwidth,
                layout,
                true,
                coding,
                &mut prev_lag[channel],
                pulses,
            );
        }
    }
}
