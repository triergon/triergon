//! Mappings and modes (Vorbis I section 4.2.4): which floor and residue each channel
//! uses, square-polar channel coupling, and the block size of each mode.

use triergon_bitstream::LsbReader;
use triergon_codec::{Budget, BudgetVec};

use crate::window::Block;
use crate::{DecodeError, ilog, push};

/// One square-polar coupling step.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Coupling {
    pub(crate) magnitude: u8,
    pub(crate) angle: u8,
}

/// The floor and residue configuration numbers of one submap.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Submap {
    pub(crate) floor: u8,
    pub(crate) residue: u8,
}

#[derive(Debug)]
pub(crate) struct Mapping {
    pub(crate) coupling: BudgetVec<Coupling>,
    /// Submap of each channel.
    pub(crate) mux: BudgetVec<u8>,
    pub(crate) submaps: BudgetVec<Submap>,
}

impl Mapping {
    /// Reads a type 0 mapping for `channels` channels, given the numbers of floors and
    /// residues configured.
    pub(crate) fn read(
        r: &mut LsbReader<'_>,
        channels: usize,
        floors: usize,
        residues: usize,
        budget: &Budget,
    ) -> Result<Mapping, DecodeError> {
        if r.read(16) != 0 {
            return Err(DecodeError::Invalid("mapping type"));
        }
        let submap_count = if r.bit() { r.read(4) as usize + 1 } else { 1 };
        let mut coupling = BudgetVec::new(budget);
        if r.bit() {
            let bits = ilog(channels as u32 - 1);
            // At most 256 steps.
            let steps = r.read(8) as usize + 1;
            coupling.try_reserve_exact(steps)?;
            for _ in 0..steps {
                let step = Coupling {
                    magnitude: r.read(bits) as u8,
                    angle: r.read(bits) as u8,
                };
                let valid = |channel: u8| usize::from(channel) < channels;
                if step.magnitude == step.angle || !valid(step.magnitude) || !valid(step.angle) {
                    return Err(DecodeError::Invalid("coupling channels"));
                }
                push(&mut coupling, step)?;
            }
        }
        if r.read(2) != 0 {
            return Err(DecodeError::Invalid("mapping reserved field"));
        }
        let mut mux = BudgetVec::try_filled(budget, 0, channels)?;
        if submap_count > 1 {
            for submap in mux.iter_mut() {
                *submap = r.read(4) as u8;
            }
        }
        if mux
            .iter()
            .any(|&submap| usize::from(submap) >= submap_count)
        {
            return Err(DecodeError::Invalid("mapping submap number"));
        }
        let mut submaps = BudgetVec::new(budget);
        submaps.try_reserve_exact(submap_count)?;
        for _ in 0..submap_count {
            r.read(8);
            let submap = Submap {
                floor: r.read(8) as u8,
                residue: r.read(8) as u8,
            };
            if usize::from(submap.floor) >= floors || usize::from(submap.residue) >= residues {
                return Err(DecodeError::Invalid("mapping floor or residue number"));
            }
            push(&mut submaps, submap)?;
        }
        if r.overrun() {
            return Err(DecodeError::Invalid("truncated mapping"));
        }
        Ok(Mapping {
            coupling,
            mux,
            submaps,
        })
    }
}

/// One mode: block size and mapping.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Mode {
    pub(crate) block: Block,
    pub(crate) mapping: u8,
}

impl Mode {
    pub(crate) fn read(r: &mut LsbReader<'_>, mappings: usize) -> Result<Mode, DecodeError> {
        let block = if r.bit() { Block::Long } else { Block::Short };
        let window = r.read(16);
        let transform = r.read(16);
        let mapping = r.read(8);
        if r.overrun() {
            return Err(DecodeError::Invalid("truncated mode"));
        }
        if window != 0 || transform != 0 || mapping as usize >= mappings {
            return Err(DecodeError::Invalid("mode"));
        }
        Ok(Mode {
            block,
            mapping: mapping as u8,
        })
    }
}

/// Inverse square-polar coupling (section 4.3.5) of one magnitude and angle vector.
///
/// A zero magnitude takes the positive branch, as in FFmpeg: the specification's
/// "greater than zero" gives the opposite signs when the magnitude is exactly zero and
/// the angle is not, which FFmpeg's own encoder produces.
///
/// The four cases of the specification fold into two: with the angle's sign flipped
/// for a negative magnitude, a positive angle gives `(m, m - a)` and any other
/// `(m + a, m)` (`x + -y` and `x - y` round alike).
pub(crate) fn decouple(magnitude: &mut [f32], angle: &mut [f32]) {
    for (m, a) in magnitude.iter_mut().zip(angle) {
        let (mv, av) = (*m, *a);
        let signed = if mv >= 0.0 { av } else { -av };
        (*m, *a) = if av > 0.0 {
            (mv, mv - signed)
        } else {
            (mv + signed, mv)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::decouple;

    #[test]
    fn decoupling_follows_the_four_quadrants() {
        let mut m = [2.0, 2.0, -2.0, -2.0, 0.0, 0.0];
        let mut a = [1.0, -1.0, 1.0, -1.0, 1.0, -1.0];
        decouple(&mut m, &mut a);
        assert_eq!(m, [2.0, 1.0, -2.0, -1.0, 0.0, -1.0]);
        assert_eq!(a, [1.0, 2.0, -1.0, -2.0, -1.0, 0.0]);
    }
}
