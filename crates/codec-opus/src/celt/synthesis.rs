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
    /// The DCT-IV of one block, from which the IMDCT output unfolds.
    dct: [f32; MAX_FRAME],
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
            dct: [0.0; MAX_FRAME],
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
        const HALF: usize = OVERLAP / 2;
        for (b, out) in out.chunks_exact_mut(block).enumerate() {
            let d = &mut self.dct[..block];
            if blocks == 1 {
                imdct.dct4(freq, d);
            } else {
                // Short blocks interleave their coefficients.
                let coefficients = &mut self.coefficients[..block];
                for (c, &f) in coefficients
                    .iter_mut()
                    .zip(freq[b..].iter().step_by(blocks))
                {
                    *c = f;
                }
                imdct.dct4(coefficients, d);
            }
            // The 2 * block outputs of the IMDCT unfold from its DCT-IV d (K = block):
            // y[m] = d[m + K/2] below K/2, -d[3K/2 - 1 - m] up to 3K/2, -d[m - 3K/2]
            // after. The window is zero before m = K/2 - OVERLAP/2, so output sample t
            // of the block is y[K/2 - OVERLAP/2 + t]: the overlap-added head, the
            // unwindowed middle and the windowed tail carried into the next block.
            let (head, middle) = out.split_at_mut(OVERLAP);
            let (rising, falling) = WINDOW.split_at(HALF);
            let (head1, head2) = head.split_at_mut(HALF);
            let (carry1, carry2) = overlap.split_at(HALF);
            for (((o, &c), &w), &v) in head1
                .iter_mut()
                .zip(carry1)
                .zip(rising)
                .zip(&d[block - HALF..])
            {
                *o = c + w * v;
            }
            for (((o, &c), &w), &v) in head2
                .iter_mut()
                .zip(carry2)
                .zip(falling)
                .zip(d[block - HALF..].iter().rev())
            {
                *o = c + w * -v;
            }
            for (o, &v) in middle.iter_mut().zip(d[HALF..block - HALF].iter().rev()) {
                *o = -v;
            }
            let (tail1, tail2) = overlap.split_at_mut(HALF);
            for ((o, &w), &v) in tail1
                .iter_mut()
                .zip(falling.iter().rev())
                .zip(d[..HALF].iter().rev())
            {
                *o = w * -v;
            }
            for ((o, &w), &v) in tail2.iter_mut().zip(rising.iter().rev()).zip(&d[..HALF]) {
                *o = w * -v;
            }
        }
    }
}
