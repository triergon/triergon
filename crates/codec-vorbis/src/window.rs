//! Block sizes, window slopes and overlap-add (Vorbis I sections 1.3.2, 4.3.1, 4.3.8).

use triergon_codec::{Budget, BudgetVec, StorageError, vorbis_window};

/// Which of the stream's two block sizes a packet uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Block {
    Short = 0,
    Long = 1,
}

/// The window slope two blocks of one size overlap by, `m = blocksize / 2` samples of
/// `sin(π/2 sin²((i + 1/2) / m · π/2))`, rising and (reversed) falling.
#[derive(Debug)]
pub(crate) struct Slope {
    rising: BudgetVec<f32>,
    falling: BudgetVec<f32>,
}

impl Slope {
    fn new(blocksize: usize, budget: &Budget) -> Result<Slope, StorageError> {
        let m = blocksize / 2;
        let mut rising = BudgetVec::try_filled(budget, 0.0, m)?;
        vorbis_window(&mut rising);
        let mut falling = BudgetVec::try_filled(budget, 0.0, m)?;
        for (falling, &rising) in falling.iter_mut().zip(rising.iter().rev()) {
            *falling = rising;
        }
        Ok(Slope { rising, falling })
    }

    /// Samples the slope spans.
    pub(crate) fn len(&self) -> usize {
        self.rising.len()
    }
}

/// The slope of each block size.
#[derive(Debug)]
pub(crate) struct Slopes([Slope; 2]);

impl Slopes {
    pub(crate) fn new(blocksizes: [usize; 2], budget: &Budget) -> Result<Slopes, StorageError> {
        Ok(Slopes([
            Slope::new(blocksizes[0], budget)?,
            Slope::new(blocksizes[1], budget)?,
        ]))
    }

    /// The slope joining a block of `previous` size to one of `current` size: the
    /// smaller of the two sets the overlap.
    pub(crate) fn between(&self, previous: Block, current: Block) -> &Slope {
        let block = if previous == Block::Short || current == Block::Short {
            Block::Short
        } else {
            Block::Long
        };
        &self.0[block as usize]
    }
}

/// Overlap-adds two blocks straight from their DCT-IVs, writing the finished samples
/// from the centre of the previous block to the centre of the current one into `out`
/// (a quarter of each block size).
///
/// A block's inverse MDCT is its DCT-IV `w = [a | b]` (a quarter block each) unfolded:
/// the left half of the output is `b` then `-b` reversed, the right half `-a` reversed
/// then `-a`. `previous` is the first quarter of the previous block's right half, `-a`
/// reversed (see [`fold`]), and `current` the whole DCT-IV of the current block; the
/// unfolding is folded into the windowing, each pair of output samples mirrored about
/// the overlap's centre taking one value of each block.
///
/// The previous block's three-quarter point aligns with the current block's quarter
/// point; outside the overlap each window is one on the side of its centre that is output
/// and zero beyond.
pub(crate) fn overlap_add(previous: &[f32], current: &[f32], slope: &Slope, out: &mut [f32]) {
    let half = slope.len() / 2;
    let quarter = current.len() / 2;
    let from = quarter - half;
    let b = &current[quarter..];
    let lap = Lap::new(previous, half, out);
    let b_lapped = &b[from..from + half];
    let (w_in, w_out) = (&slope.rising[..half], &slope.falling[..half]);
    for j in 0..half {
        let (p, b) = (lap.previous[j], b_lapped[j]);
        lap.rising[j] = p * w_out[j] + b * w_in[j];
        lap.falling[half - 1 - j] = p * w_in[j] + -b * w_out[j];
    }
    for (out, &b) in lap.tail.iter_mut().zip(b[..from].iter().rev()) {
        *out = -b;
    }
}

/// [`overlap_add`] with a silent current block: the previous block fades out.
pub(crate) fn fade_out(previous: &[f32], slope: &Slope, out: &mut [f32]) {
    let half = slope.len() / 2;
    let lap = Lap::new(previous, half, out);
    let (w_in, w_out) = (&slope.rising[..half], &slope.falling[..half]);
    for j in 0..half {
        let p = lap.previous[j];
        lap.rising[j] = p * w_out[j] + 0.0;
        lap.falling[half - 1 - j] = p * w_in[j] + 0.0;
    }
    lap.tail.fill(0.0);
}

/// The regions of one overlap-add's output, with the samples before the overlap, where
/// only the previous block contributes, already written.
struct Lap<'a, 'p> {
    /// The previous block's samples over the overlap.
    previous: &'p [f32],
    rising: &'a mut [f32],
    falling: &'a mut [f32],
    tail: &'a mut [f32],
}

impl<'a, 'p> Lap<'a, 'p> {
    fn new(previous: &'p [f32], half: usize, out: &'a mut [f32]) -> Lap<'a, 'p> {
        let start = previous.len() - half;
        let (head, rest) = out.split_at_mut(start);
        head.copy_from_slice(&previous[..start]);
        let (lapped, tail) = rest.split_at_mut(2 * half);
        let (rising, falling) = lapped.split_at_mut(half);
        Lap {
            previous: &previous[start..],
            rising,
            falling,
            tail,
        }
    }
}

/// The first quarter of the right half of the inverse MDCT of the DCT-IV `current`:
/// `-a` reversed, which with its reverse makes the right half the next block laps.
pub(crate) fn fold(current: &[f32], saved: &mut [f32]) {
    let quarter = current.len() / 2;
    for (saved, &a) in saved.iter_mut().zip(current[..quarter].iter().rev()) {
        *saved = -a;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slopes_are_power_complementary() {
        let budget = Budget::new(1 << 16).unwrap();
        let slopes = Slopes::new([64, 256], &budget).unwrap();
        for slope in [
            slopes.between(Block::Short, Block::Long),
            slopes.between(Block::Long, Block::Long),
        ] {
            for (up, down) in slope.rising.iter().zip(slope.falling.iter()) {
                assert!((up * up + down * down - 1.0).abs() < 1e-6);
            }
        }
        assert_eq!(slopes.between(Block::Long, Block::Short).len(), 32);
        assert_eq!(slopes.between(Block::Long, Block::Long).len(), 128);
    }

    /// The inverse MDCT output of the DCT-IV `w` (as `Imdct::imdct` unfolds it).
    fn unfold(w: &[f32]) -> Vec<f32> {
        let (a, b) = w.split_at(w.len() / 2);
        let neg = |v: &[f32]| v.iter().map(|&x| -x).collect::<Vec<_>>();
        let mut y = b.to_vec();
        y.extend(neg(b).iter().rev());
        y.extend(neg(a).iter().rev());
        y.extend(neg(a));
        y
    }

    /// Overlap-add on the unfolded outputs, window by window.
    fn reference(previous: &[f32], current: &[f32], slope: &[f32]) -> Vec<f32> {
        let previous = unfold(previous);
        let current = unfold(current);
        let right = &previous[previous.len() / 2..];
        let ov = slope.len();
        let start = right.len() / 2 - ov / 2;
        let from = current.len() / 4 - ov / 2;
        let len = previous.len() / 4 + current.len() / 4;
        (0..len)
            .map(|i| match i {
                i if i < start => right[i],
                i if i < start + ov => {
                    let k = i - start;
                    right[i] * slope[ov - 1 - k] + current[from + k] * slope[k]
                }
                i => current[from + i - start],
            })
            .collect()
    }

    #[test]
    fn folded_overlap_matches_the_unfolded_windows_bit_for_bit() {
        let values = |n: usize, seed: f32| -> Vec<f32> {
            (0..n)
                .map(|i| ((i as f32 + seed) * 0.37).sin() * 3.0)
                .collect()
        };
        let slope =
            |m: usize| -> Vec<f32> { (0..m).map(|i| (i as f32 + 0.5) / m as f32).collect() };
        let (short, long) = (slope(32), slope(128));
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        // Blocks of 64 and 256 samples: DCT-IVs of 32 and 128 values.
        let budget = Budget::new(1 << 16).unwrap();
        let charged = |values: Vec<f32>| {
            let mut out = BudgetVec::new(&budget);
            out.try_extend_from_slice(&values).unwrap();
            out
        };
        let lap = |rising: &[f32]| Slope {
            rising: charged(rising.to_vec()),
            falling: charged(rising.iter().rev().copied().collect()),
        };
        let (short_lap, long_lap) = (lap(&short), lap(&long));
        for (p, c, slope, lap) in [
            (32, 32, &short, &short_lap),
            (128, 128, &long, &long_lap),
            (32, 128, &short, &short_lap),
            (128, 32, &short, &short_lap),
        ] {
            let previous = values(p, 1.0);
            let mut saved = vec![0.0; p / 2];
            fold(&previous, &mut saved);
            let mut out = vec![0.0; p / 2 + c / 2];
            let current = values(c, 7.0);
            overlap_add(&saved, &current, lap, &mut out);
            assert_eq!(
                bits(&out),
                bits(&reference(&previous, &current, slope)),
                "{p} then {c}"
            );
            // A silent block's inverse MDCT is +0.0 throughout.
            fade_out(&saved, lap, &mut out);
            let want = reference(&previous, &vec![0.0; c], slope);
            let lapped: Vec<f32> = want.iter().map(|&v| v + 0.0).collect();
            assert_eq!(bits(&out), bits(&lapped), "{p} then silence");
        }
    }
}
