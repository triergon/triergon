//! Reconstruction of one SILK channel (RFC 6716 section 4.2.7.9): parameter dequantization,
//! long-term (pitch) prediction and short-term (LPC) synthesis. Bit-exact integer steps end
//! with the Q12 filter; the filters run in floating point as section 4.2.7.9 describes.

use super::frame::SideInfo;
use super::gain::{self, RESET_LOG_GAIN};
use super::lpc;
use super::ltp::Ltp;
use super::nlsf::{self, Nlsf};
use super::{Bandwidth, MAX_FRAME, MAX_ORDER};

/// Output history kept for rewhitening: 20 ms at 16 kHz, more than the longest pitch lag
/// (18 ms) plus the filter orders need.
const HISTORY: usize = MAX_FRAME;
/// Longest pitch lag plus the two taps beyond it, at 16 kHz.
const MAX_LAG_SPAN: usize = 18 * 16 + 2;
const MAX_SUBFRAME: usize = 80;

/// Decoder state of one coded channel (mid/mono or side).
#[derive(Clone, Debug)]
pub(super) struct ChannelState {
    /// Clamped output: `HISTORY` samples of history, then the frame being decoded.
    out: [f32; HISTORY + MAX_FRAME],
    /// Unclamped LPC synthesis output of the last `MAX_ORDER` samples, oldest first. All
    /// of them are kept whatever the current order, so that a switch to wideband
    /// continues from the true signal history (as FFmpeg does).
    lpc: [f32; MAX_ORDER],
    /// LPC residual of the frame being decoded, scaled by its subframe gain.
    residual: [f32; MAX_FRAME],
    prev_nlsf: Nlsf,
    prev_log_gain: i32,
    /// No previous frame to interpolate LSFs from.
    after_reset: bool,
}

impl ChannelState {
    pub(super) fn new() -> ChannelState {
        ChannelState {
            out: [0.0; HISTORY + MAX_FRAME],
            lpc: [0.0; MAX_ORDER],
            residual: [0.0; MAX_FRAME],
            prev_nlsf: [0; MAX_ORDER],
            prev_log_gain: RESET_LOG_GAIN,
            after_reset: true,
        }
    }

    /// Return to silence: after a decoder reset, for a new side channel, or when the side
    /// channel resumes after uncoded frames. (A change of internal rate keeps the state, as
    /// FFmpeg does.)
    pub(super) fn reset(&mut self) {
        *self = ChannelState::new();
    }

    /// Decode one SILK frame of `subframes` subframes from its side information and
    /// excitation into `out` (clamped to the nominal range).
    pub(super) fn decode(
        &mut self,
        info: &SideInfo,
        excitation: &[f32],
        bandwidth: Bandwidth,
        subframes: usize,
        out: &mut [f32],
    ) {
        let order = bandwidth.order();
        let n = bandwidth.subframe_len();
        let len = n * subframes;
        let codebook = bandwidth.codebook();

        let mut gains_q16 = [0; 4];
        gain::dequantize(
            &info.gains[..subframes],
            &mut self.prev_log_gain,
            &mut gains_q16,
        );

        let nlsf = info.nlsf.decode(codebook);
        let interpolate = subframes == 4 && info.interpolation_q2 < 4 && !self.after_reset;
        let filters = {
            let second = Predictor::new(&lpc::from_nlsf(&nlsf[..order], codebook.ordering), order);
            let first = if interpolate {
                let lsf = nlsf::interpolate(&self.prev_nlsf, &nlsf, info.interpolation_q2, order);
                Predictor::new(&lpc::from_nlsf(&lsf[..order], codebook.ordering), order)
            } else {
                second
            };
            [first, second]
        };
        self.prev_nlsf = nlsf;
        self.after_reset = false;
        let ltp = info
            .pitch
            .map(|pitch| Ltp::new(&pitch, bandwidth, subframes));

        // lpc[MAX_ORDER + i] is the synthesis output at frame offset i.
        let mut lpc = [0.0f32; MAX_ORDER + MAX_FRAME];
        lpc[..MAX_ORDER].copy_from_slice(&self.lpc);
        for s in 0..subframes {
            let filter = &filters[s / 2];
            let gain_q16 = gains_q16[s] as f32;
            let j = s * n;
            let mut res = [0.0f32; MAX_SUBFRAME];
            let res = &mut res[..n];
            let excitation = &excitation[j..j + n];
            match &ltp {
                Some(ltp) => {
                    // The second half of an interpolated frame rewhitens with its own
                    // filter from its own start, without LTP scaling.
                    let (rewhiten_end, scale_q14) = if interpolate && s >= 2 {
                        (2 * n, 16384)
                    } else {
                        (0, ltp.scale_q14)
                    };
                    let history = LtpHistory {
                        start: j,
                        rewhiten_end,
                        out_scale: 4.0 * scale_q14 as f32 / gain_q16,
                        residual_scale: 65536.0 / gain_q16,
                    };
                    self.long_term(filter, ltp.lags[s], &ltp.taps[s], &history, excitation, res);
                }
                None => res.copy_from_slice(excitation),
            }

            // Short-term synthesis.
            let gain = gain_q16 / 65536.0;
            for (t, &r) in res.iter().enumerate() {
                let i = j + t;
                let residual = gain * r;
                let at = MAX_ORDER + i;
                let value = residual + filter.predict(&lpc[at - order..at]);
                lpc[at] = value;
                self.residual[i] = residual;
                self.out[HISTORY + i] = value.clamp(-1.0, 1.0);
            }
        }
        self.lpc.copy_from_slice(&lpc[len..len + MAX_ORDER]);
        out.copy_from_slice(&self.out[HISTORY..HISTORY + len]);
        self.out.copy_within(len..len + HISTORY, 0);
    }

    /// The LPC residual `res` of a voiced subframe after long-term prediction with `lag`
    /// and `taps` from the residual history `history` describes (section 4.2.7.9.1).
    fn long_term(
        &self,
        filter: &Predictor,
        lag: usize,
        taps: &[f32; 5],
        history: &LtpHistory,
        excitation: &[f32],
        res: &mut [f32],
    ) {
        let order = filter.order;
        let LtpHistory {
            start: j,
            rewhiten_end,
            out_scale,
            residual_scale,
        } = *history;
        // buf[t] is the residual at frame offset j - lag - 2 + t.
        let span = lag + 2;
        let mut buf = [0.0f32; MAX_LAG_SPAN + MAX_SUBFRAME];
        for (t, value) in buf[..span].iter_mut().enumerate() {
            // Offsets relative to the frame start; negative ones are in the history.
            let i = (j + t) as isize - span as isize;
            *value = if i < rewhiten_end as isize {
                let at = (HISTORY as isize + i) as usize;
                let whitened = self.out[at] - filter.predict(&self.out[at - order..at]);
                out_scale * whitened.clamp(-1.0, 1.0)
            } else {
                residual_scale * self.residual[i as usize]
            };
        }
        for (t, (r, &e)) in res.iter_mut().zip(excitation).enumerate() {
            // Taps apply to the residual at lags +2 down to -2 around the pitch period.
            let history = &buf[t..t + 5];
            let prediction: f32 = history.iter().rev().zip(taps).map(|(h, b)| h * b).sum();
            *r = e + prediction;
            buf[span + t] = *r;
        }
    }
}

/// Where the residual history of a voiced subframe comes from.
#[derive(Clone, Copy, Debug)]
struct LtpHistory {
    /// Frame offset of the subframe.
    start: usize,
    /// History before this frame offset is rewhitened from the clamped output and scaled
    /// by `out_scale`; later history is the frame's own residual, scaled by
    /// `residual_scale`.
    rewhiten_end: usize,
    out_scale: f32,
    residual_scale: f32,
}

/// A short-term prediction filter in the form the synthesis loop applies it.
#[derive(Clone, Copy, Debug)]
struct Predictor {
    order: usize,
    /// Coefficients reversed, so that `predict` is a dot product with the oldest sample
    /// first: `reversed[order - 1 - k] = a_Q12[k] / 4096`.
    reversed: [f32; MAX_ORDER],
}

impl Predictor {
    fn new(a_q12: &lpc::LpcQ12, order: usize) -> Predictor {
        let mut reversed = [0.0; MAX_ORDER];
        for (out, &a) in reversed[..order].iter_mut().rev().zip(a_q12) {
            *out = f32::from(a) / 4096.0;
        }
        Predictor { order, reversed }
    }

    /// The prediction from the `order` preceding samples `history`, oldest first.
    #[inline(always)]
    fn predict(&self, history: &[f32]) -> f32 {
        history
            .iter()
            .zip(&self.reversed[..self.order])
            .map(|(x, a)| x * a)
            .sum()
    }
}
