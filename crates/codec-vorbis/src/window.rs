//! Block sizes, window slopes and overlap-add (Vorbis I sections 1.3.2, 4.3.1, 4.3.8).

use core::f64::consts::FRAC_PI_2;

use crate::storage::Meter;
use triergon_codec::StorageError;

/// Which of the stream's two block sizes a packet uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Block {
    Short = 0,
    Long = 1,
}

/// The rising window slope of each block size: `sin(π/2 sin²((i + 1/2) / m · π/2))` over
/// the `m = blocksize / 2` samples two blocks of that size overlap by.
#[derive(Debug)]
pub(crate) struct Slopes([Vec<f32>; 2]);

impl Slopes {
    pub(crate) fn new(blocksizes: [usize; 2], meter: &mut Meter) -> Result<Slopes, StorageError> {
        let slope = |meter: &mut Meter, blocksize: usize| -> Result<Vec<f32>, StorageError> {
            let m = blocksize / 2;
            let mut slope = meter.with_capacity(m)?;
            slope.extend((0..m).map(|i| {
                let s = ((i as f64 + 0.5) / m as f64 * FRAC_PI_2).sin();
                (FRAC_PI_2 * s * s).sin() as f32
            }));
            Ok(slope)
        };
        Ok(Slopes([
            slope(meter, blocksizes[0])?,
            slope(meter, blocksizes[1])?,
        ]))
    }

    /// The slope joining a block of `previous` size to one of `current` size: the
    /// smaller of the two sets the overlap.
    pub(crate) fn between(&self, previous: Block, current: Block) -> &[f32] {
        let block = if previous == Block::Short || current == Block::Short {
            Block::Short
        } else {
            Block::Long
        };
        &self.0[block as usize]
    }
}

/// Overlap-adds the right half of the previous block's IMDCT output, `previous`, with
/// the current block's output `current` over `slope`, writing the finished samples
/// from the centre of the previous block to the centre of the current one into `out`
/// (`previous.len() / 2 + current.len() / 4` samples).
///
/// The previous block's quarter point aligns with the current block's quarter point;
/// outside the overlap each window is one on the side of its centre that is output and
/// zero beyond.
pub(crate) fn overlap_add(previous: &[f32], current: &[f32], slope: &[f32], out: &mut [f32]) {
    let overlap = slope.len();
    let previous_quarter = previous.len() / 2;
    let current_quarter = current.len() / 4;
    // Output position of the overlap's start, and the current block's sample there.
    let start = previous_quarter - overlap / 2;
    let from = current_quarter - overlap / 2;
    let (head, rest) = out.split_at_mut(start);
    let (lapped, tail) = rest.split_at_mut(overlap);
    head.copy_from_slice(&previous[..start]);
    let fading = previous[start..start + overlap]
        .iter()
        .zip(slope.iter().rev());
    let rising = current[from..from + overlap].iter().zip(slope);
    for (out, ((&p, &down), (&c, &up))) in lapped.iter_mut().zip(fading.zip(rising)) {
        *out = p * down + c * up;
    }
    let after = from + overlap;
    tail.copy_from_slice(&current[after..after + tail.len()]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_codec::{Budget, Reservation};

    #[test]
    fn slopes_are_power_complementary() {
        let budget = Budget::new(1 << 16).unwrap();
        let mut meter = Meter(Reservation::empty(&budget));
        let slopes = Slopes::new([64, 256], &mut meter).unwrap();
        for slope in [
            slopes.between(Block::Short, Block::Long),
            slopes.between(Block::Long, Block::Long),
        ] {
            for (up, down) in slope.iter().zip(slope.iter().rev()) {
                assert!((up * up + down * down - 1.0).abs() < 1e-6);
            }
        }
        assert_eq!(slopes.between(Block::Long, Block::Short).len(), 32);
        assert_eq!(slopes.between(Block::Long, Block::Long).len(), 128);
    }

    #[test]
    fn mixed_sizes_return_the_span_between_centres() {
        let slope = [0.125, 0.25, 0.5, 0.75];
        let lap = |p: f32, c: f32, k: usize| p * slope[3 - k] + c * slope[k];
        // Long (16) after short (8): 8/4 + 16/4 = 6 samples. The short block's right
        // half laps samples 2..6 of the long block, whose hybrid slope is zero before.
        let previous = [1.0, 2.0, 3.0, 4.0];
        let current: Vec<f32> = (0..16).map(|i| 10.0 + i as f32).collect();
        let mut out = [0.0; 6];
        overlap_add(&previous, &current, &slope, &mut out);
        let want: Vec<f32> = (0..4)
            .map(|k| lap(previous[k], current[2 + k], k))
            .chain([16.0, 17.0])
            .collect();
        assert_eq!(out[..], want[..]);
        // Short after long: 16/4 + 8/4 = 6 samples; the long block's flat part first.
        let previous: Vec<f32> = (0..8).map(|i| i as f32).collect();
        let current: Vec<f32> = (0..8).map(|i| 10.0 + i as f32).collect();
        overlap_add(&previous, &current, &slope, &mut out);
        let want: Vec<f32> = [0.0, 1.0]
            .into_iter()
            .chain((0..4).map(|k| lap(previous[2 + k], current[k], k)))
            .collect();
        assert_eq!(out[..], want[..]);
    }
}
