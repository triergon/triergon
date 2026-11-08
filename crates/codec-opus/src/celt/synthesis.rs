//! Synthesis (RFC 6716 section 4.3.7): the inverse MDCT of each long or short block with
//! the low-overlap window and overlap-add.

use super::tables::WINDOW;
use triergon_codec::mdct::Imdct;

/// Window overlap between consecutive blocks.
pub(crate) const OVERLAP: usize = WINDOW.len();
/// Coefficients of a short block.
const SHORT: usize = 120;
/// Coefficients of the longest (20 ms) frame.
pub(crate) const MAX_FRAME: usize = SHORT << 3;

/// The transforms of every frame size, shared by all channels and streams.
#[derive(Debug)]
pub(crate) struct Synthesis {
    /// IMDCTs producing `2 * 120 << lm` samples.
    imdct: [Imdct; 4],
    coefficients: [f32; MAX_FRAME],
    time: [f32; 2 * MAX_FRAME],
    /// The frame and the overlap carried into the next one.
    output: [f32; MAX_FRAME + OVERLAP],
}

impl Synthesis {
    pub(crate) fn new() -> Synthesis {
        // Every size is a product of 2, 3 and 5, which the shared transform supports.
        #[allow(clippy::expect_used)] // 240 << lm for lm < 4 always is.
        let imdct = core::array::from_fn(|lm| {
            Imdct::new((2 * SHORT) << lm, 1.0).expect("CELT transform sizes are supported")
        });
        Synthesis {
            imdct,
            coefficients: [0.0; MAX_FRAME],
            time: [0.0; 2 * MAX_FRAME],
            output: [0.0; MAX_FRAME + OVERLAP],
        }
    }

    /// Transform the `freq.len()` coefficients of one channel, `blocks` interleaved short
    /// blocks or one long block, into `out` (same length): the windowed blocks
    /// overlap-added onto `overlap`, which then holds the tail for the next frame.
    pub(super) fn inverse(
        &mut self,
        freq: &[f32],
        blocks: usize,
        lm: usize,
        out: &mut [f32],
        overlap: &mut [f32; OVERLAP],
    ) {
        let n = freq.len();
        let block = n / blocks;
        let imdct = &mut self.imdct[if blocks > 1 { 0 } else { lm }];
        let acc = &mut self.output[..n + OVERLAP];
        acc[..OVERLAP].copy_from_slice(overlap);
        // The window is zero for the first `(block - OVERLAP) / 2` samples of a block.
        let lead = (block - OVERLAP) / 2;
        for b in 0..blocks {
            let coefficients = &mut self.coefficients[..block];
            for (j, c) in coefficients.iter_mut().enumerate() {
                *c = freq[b + j * blocks];
            }
            let time = &mut self.time[..2 * block];
            imdct.imdct(coefficients, time);
            let y = &time[lead..lead + block + OVERLAP];
            let dst = &mut acc[b * block..b * block + block + OVERLAP];
            let (head, rest) = dst.split_at_mut(OVERLAP);
            let (middle, tail) = rest.split_at_mut(block - OVERLAP);
            for ((d, &v), &w) in head.iter_mut().zip(&y[..OVERLAP]).zip(&WINDOW) {
                *d += w * v;
            }
            middle.copy_from_slice(&y[OVERLAP..block]);
            for ((d, &v), &w) in tail.iter_mut().zip(&y[block..]).zip(WINDOW.iter().rev()) {
                *d = w * v;
            }
        }
        out.copy_from_slice(&acc[..n]);
        overlap.copy_from_slice(&acc[n..]);
    }
}
