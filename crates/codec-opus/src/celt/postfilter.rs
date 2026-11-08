//! The pitch post-filter (RFC 6716 section 4.3.7.1): a comb filter that undoes the
//! encoder's pre-filter, cross-faded over the window overlap when its parameters change.

use super::tables::{COMB_TAPS, TAPSET_ICDF, WINDOW};
use crate::range::RangeDecoder;

/// The shortest period the filter runs at.
const MIN_PERIOD: usize = 15;
/// The longest period plus the two taps beyond it: history the filter reads.
pub(super) const MAX_REACH: usize = 1024 + 2;

/// Comb filter parameters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Pitch {
    pub(super) period: usize,
    pub(super) gain: f32,
    pub(super) tapset: usize,
}

impl Pitch {
    /// Decode the post-filter parameters of a frame header, `None` when the frame
    /// disables the filter.
    pub(super) fn decode(rc: &mut RangeDecoder<'_>, total_bits: i32) -> Option<Pitch> {
        if !rc.bit_logp(1) {
            return None;
        }
        let octave = rc.uint(6);
        let period = (16 << octave) + rc.bits(4 + octave) as usize - 1;
        let gain = rc.bits(3) as f32;
        let tapset = if rc.tell() + 2 <= total_bits {
            rc.icdf(&TAPSET_ICDF, 2)
        } else {
            0
        };
        Some(Pitch {
            period,
            gain: 0.093_75 * (gain + 1.0),
            tapset,
        })
    }

    /// The parameters as the filter runs them: periods below the minimum are raised.
    pub(super) fn bounded(self) -> Pitch {
        Pitch {
            period: self.period.max(MIN_PERIOD),
            ..self
        }
    }

    fn taps(self) -> [f32; 3] {
        let taps = COMB_TAPS[self.tapset.min(2)];
        [
            self.gain * taps[0],
            self.gain * taps[1],
            self.gain * taps[2],
        ]
    }
}

/// Filter `buf[start..start + len]` in place (the filter is recursive: it reads its own
/// earlier output), cross-fading from `from` to `to` over the first `WINDOW.len() <= len`
/// samples. `buf` holds at least [`MAX_REACH`] samples before `start`.
pub(super) fn comb(buf: &mut [f32], start: usize, len: usize, from: Pitch, to: Pitch) {
    if from.gain == 0.0 && to.gain == 0.0 {
        return;
    }
    let [f0, f1, f2] = from.taps();
    let [t0, t1, t2] = to.taps();
    let (p0, p1) = (from.period, to.period);
    let overlap = WINDOW.len().min(len);
    for i in start..start + overlap {
        let w = WINDOW[i - start];
        let f = w * w;
        let g = 1.0 - f;
        buf[i] = buf[i]
            + (g * f0) * buf[i - p0]
            + (g * f1) * buf[i - p0 - 1]
            + (g * f1) * buf[i - p0 + 1]
            + (g * f2) * buf[i - p0 - 2]
            + (g * f2) * buf[i - p0 + 2]
            + (f * t0) * buf[i - p1]
            + (f * t1) * buf[i - p1 - 1]
            + (f * t1) * buf[i - p1 + 1]
            + (f * t2) * buf[i - p1 - 2]
            + (f * t2) * buf[i - p1 + 2];
    }
    for i in start + overlap..start + len {
        buf[i] = buf[i]
            + t0 * buf[i - p1]
            + t1 * buf[i - p1 - 1]
            + t1 * buf[i - p1 + 1]
            + t2 * buf[i - p1 - 2]
            + t2 * buf[i - p1 + 2];
    }
}
