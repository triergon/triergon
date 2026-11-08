//! The setup header (Vorbis I section 4.2.4).

use triergon_bitstream::LsbReader;
use triergon_codec::{Budget, BudgetVec, Charged};

use crate::codebook::Codebook;
use crate::floor0::Floor0;
use crate::floor1::Floor1;
use crate::mapping::{Mapping, Mode};
use crate::residue::Residue;
use crate::{DecodeError, push};

#[derive(Debug)]
pub(crate) enum Floor {
    Lsp(Floor0),
    Linear(Charged<Box<Floor1>>),
}

/// Everything the setup header configures.
#[derive(Debug)]
pub(crate) struct Setup {
    pub(crate) codebooks: BudgetVec<Codebook>,
    pub(crate) floors: BudgetVec<Floor>,
    pub(crate) residues: BudgetVec<Residue>,
    pub(crate) mappings: BudgetVec<Mapping>,
    pub(crate) modes: BudgetVec<Mode>,
}

impl Setup {
    /// Parses a setup header packet for a stream of `channels` channels and the given
    /// block sizes, charging its tables to `budget`. The configuration lists are capped
    /// by their field widths (256 codebooks, 64 of everything else); the codebook tables
    /// and floor 0 maps are what scale with the stream.
    pub(crate) fn parse(
        packet: &[u8],
        channels: usize,
        blocksizes: [usize; 2],
        budget: &Budget,
    ) -> Result<Setup, DecodeError> {
        let body = packet
            .strip_prefix(b"\x05vorbis")
            .ok_or(DecodeError::Invalid("setup header"))?;
        let r = &mut LsbReader::new(body);
        let count = r.read(8) as usize + 1;
        let mut codebooks = list(budget, count)?;
        for _ in 0..count {
            push(&mut codebooks, Codebook::read(r, budget)?)?;
        }
        for _ in 0..=r.read(6) {
            if r.read(16) != 0 {
                return Err(DecodeError::Invalid("time domain transform"));
            }
        }
        let count = r.read(6) as usize + 1;
        let mut floors = list(budget, count)?;
        for _ in 0..count {
            let floor = match r.read(16) {
                0 => Floor::Lsp(Floor0::read(r, &codebooks, blocksizes, budget)?),
                1 => Floor::Linear(Charged::boxed(
                    budget,
                    Floor1::read(r, &codebooks, budget)?,
                )?),
                _ => return Err(DecodeError::Invalid("floor type")),
            };
            push(&mut floors, floor)?;
        }
        let count = r.read(6) as usize + 1;
        let mut residues = list(budget, count)?;
        for _ in 0..count {
            push(&mut residues, Residue::read(r, &codebooks, budget)?)?;
        }
        let count = r.read(6) as usize + 1;
        let mut mappings = list(budget, count)?;
        for _ in 0..count {
            let mapping = Mapping::read(r, channels, floors.len(), residues.len(), budget)?;
            push(&mut mappings, mapping)?;
        }
        let count = r.read(6) as usize + 1;
        let mut modes = list(budget, count)?;
        for _ in 0..count {
            push(&mut modes, Mode::read(r, mappings.len())?)?;
        }
        if !r.bit() || r.overrun() {
            return Err(DecodeError::Invalid("setup header framing"));
        }
        Ok(Setup {
            codebooks,
            floors,
            residues,
            mappings,
            modes,
        })
    }
}

/// An empty list with room for `count` configurations, charged first.
fn list<T>(budget: &Budget, count: usize) -> Result<BudgetVec<T>, DecodeError> {
    let mut list = BudgetVec::new(budget);
    list.try_reserve_exact(count)?;
    Ok(list)
}
