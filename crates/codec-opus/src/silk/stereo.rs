//! Mid/side stereo (RFC 6716 sections 4.2.7.1, 4.2.7.2 and 4.2.8): prediction weights, the
//! mid-only flag and unmixing to left/right.

use super::tables::{MID_ONLY, STEREO_WEIGHT_STAGE1, STEREO_WEIGHTS_Q13, UNIFORM3, UNIFORM5};
use super::{Bandwidth, MAX_FRAME};
use crate::range::RangeDecoder;

/// The side-channel prediction weights of one SILK frame, Q13.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Weights {
    /// Weight of the low-passed mid channel.
    w0_q13: i32,
    /// Weight of the mid channel.
    w1_q13: i32,
}

impl Weights {
    pub(super) fn parse(rc: &mut RangeDecoder<'_>) -> Weights {
        let joint = rc.icdf(&STEREO_WEIGHT_STAGE1, 8);
        let mut weight = |high: usize| {
            let index = rc.icdf(&UNIFORM3, 8) + 3 * high;
            let fraction = rc.icdf(&UNIFORM5, 8) as i32;
            let low = i32::from(STEREO_WEIGHTS_Q13[index]);
            let step = ((i32::from(STEREO_WEIGHTS_Q13[index + 1]) - low) * 6554) >> 16;
            low + step * (2 * fraction + 1)
        };
        let w0 = weight(joint / 5);
        let w1_q13 = weight(joint % 5);
        Weights {
            w0_q13: w0 - w1_q13,
            w1_q13,
        }
    }
}

/// Whether only the mid channel is coded for this frame.
pub(super) fn parse_mid_only(rc: &mut RangeDecoder<'_>) -> bool {
    rc.icdf(&MID_ONLY, 8) == 1
}

/// Unmixing state: the previous weights and the delayed mid and side samples.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct StereoState {
    prev: Weights,
    /// The last two mid (or mono) samples, oldest first.
    mid: [f32; 2],
    /// The last side sample.
    side: f32,
}

impl StereoState {
    /// Forget the side channel when stereo starts after mono.
    pub(super) fn reset_prediction(&mut self) {
        self.prev = Weights::default();
        self.side = 0.0;
    }

    /// Unmix one frame into `left` and `right`, interpolating from the previous weights
    /// over the first 8 ms. The output is delayed by one sample. `side` is `None` for a
    /// mid-only frame; its side signal is zero, and so (as in FFmpeg) is the delayed side
    /// sample carried over from the previous frame.
    pub(super) fn unmix(
        &mut self,
        weights: Weights,
        bandwidth: Bandwidth,
        mid: &[f32],
        side: Option<&[f32]>,
        left: &mut [f32],
        right: &mut [f32],
    ) {
        const SILENCE: [f32; MAX_FRAME] = [0.0; MAX_FRAME];
        let side = side.unwrap_or_else(|| {
            self.side = 0.0;
            &SILENCE[..mid.len()]
        });
        let n1 = 8 * bandwidth.khz();
        let prev = [self.prev.w0_q13, self.prev.w1_q13].map(|w| w as f32 / 8192.0);
        let cur = [weights.w0_q13, weights.w1_q13].map(|w| w as f32 / 8192.0);
        let step = [0, 1].map(|k| (cur[k] - prev[k]) / n1 as f32);
        let [mut m0, mut m1] = self.mid;
        let mut s1 = self.side;
        for (i, (((l, r), &m2), &s)) in left
            .iter_mut()
            .zip(right.iter_mut())
            .zip(mid)
            .zip(side)
            .enumerate()
        {
            let t = i.min(n1) as f32;
            let w0 = prev[0] + t * step[0];
            let w1 = prev[1] + t * step[1];
            let p0 = (m0 + 2.0 * m1 + m2) * 0.25;
            let shared = s1 + w0 * p0;
            *l = ((1.0 + w1) * m1 + shared).clamp(-1.0, 1.0);
            *r = ((1.0 - w1) * m1 - shared).clamp(-1.0, 1.0);
            (m0, m1, s1) = (m1, m2, s);
        }
        self.mid = [m0, m1];
        self.side = s1;
        self.prev = weights;
    }

    /// Pass a mono frame through, delayed by two samples. RFC 6716 delays mono by one
    /// sample like stereo; FFmpeg delays it by two, which the decoded output follows.
    pub(super) fn mono(&mut self, mid: &[f32], out: &mut [f32]) {
        let [mut m0, mut m1] = self.mid;
        for (o, &m2) in out.iter_mut().zip(mid) {
            *o = m0;
            (m0, m1) = (m1, m2);
        }
        self.mid = [m0, m1];
    }
}
