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

/// The five samples a comb tap set reads for each output of a block: offsets -2..=2
/// around the period, as slices parallel to the block.
fn neighbours(past: &[f32], start: usize, period: usize, len: usize) -> [&[f32]; 5] {
    let base = start - period - 2;
    core::array::from_fn(|d| &past[base + d..base + d + len])
}

/// Filter `buf[start..start + len]` in place, cross-fading from `from` to `to` over the
/// first `WINDOW.len() <= len` samples. `buf` holds at least [`MAX_REACH`] samples before
/// `start`.
///
/// The filter is recursive (it reads its own output a period back), so it runs in blocks
/// shorter than the period less the taps' reach: every sample a block reads lies before
/// it, and the samples of a block are independent of each other.
pub(super) fn comb(buf: &mut [f32], start: usize, len: usize, from: Pitch, to: Pitch) {
    if from.gain == 0.0 && to.gain == 0.0 {
        return;
    }
    let [f0, f1, f2] = from.taps();
    let [t0, t1, t2] = to.taps();
    let (p0, p1) = (from.period, to.period);
    let overlap = WINDOW.len().min(len);
    let mut i = start;
    while i < start + overlap {
        let end = (i + p0.min(p1) - 2).min(start + overlap);
        let (past, now) = buf.split_at_mut(i);
        let [a2, a1, a0, b1, b2] = neighbours(past, i, p0, end - i);
        let [c2, c1, c0, d1, d2] = neighbours(past, i, p1, end - i);
        let window = &WINDOW[i - start..end - start];
        for (k, y) in now[..end - i].iter_mut().enumerate() {
            let f = window[k] * window[k];
            let g = 1.0 - f;
            *y = *y
                + (g * f0) * a0[k]
                + (g * f1) * a1[k]
                + (g * f1) * b1[k]
                + (g * f2) * a2[k]
                + (g * f2) * b2[k]
                + (f * t0) * c0[k]
                + (f * t1) * c1[k]
                + (f * t1) * d1[k]
                + (f * t2) * c2[k]
                + (f * t2) * d2[k];
        }
        i = end;
    }
    while i < start + len {
        let end = (i + p1 - 2).min(start + len);
        let (past, now) = buf.split_at_mut(i);
        let [c2, c1, c0, d1, d2] = neighbours(past, i, p1, end - i);
        for (k, y) in now[..end - i].iter_mut().enumerate() {
            *y = *y + t0 * c0[k] + t1 * c1[k] + t1 * d1[k] + t2 * c2[k] + t2 * d2[k];
        }
        i = end;
    }
}
