//! The setup header (Vorbis I section 4.2.4).

use triergon_bitstream::LsbReader;

use crate::DecodeError;
use crate::codebook::Codebook;
use crate::floor0::Floor0;
use crate::floor1::Floor1;
use crate::mapping::{Mapping, Mode};
use crate::residue::Residue;
use crate::storage::Meter;

#[derive(Debug)]
pub(crate) enum Floor {
    Lsp(Floor0),
    Linear(Box<Floor1>),
}

/// Everything the setup header configures.
#[derive(Debug)]
pub(crate) struct Setup {
    pub(crate) codebooks: Vec<Codebook>,
    pub(crate) floors: Vec<Floor>,
    pub(crate) residues: Vec<Residue>,
    pub(crate) mappings: Vec<Mapping>,
    pub(crate) modes: Vec<Mode>,
}

impl Setup {
    /// Parses a setup header packet for a stream of `channels` channels and the given
    /// block sizes, charging its tables to `meter`. The configuration lists are capped
    /// by their field widths (256 codebooks, 64 of everything else); the codebook tables
    /// and floor 0 maps are what scale with the stream.
    pub(crate) fn parse(
        packet: &[u8],
        channels: usize,
        blocksizes: [usize; 2],
        meter: &mut Meter,
    ) -> Result<Setup, DecodeError> {
        let body = packet
            .strip_prefix(b"\x05vorbis")
            .ok_or(DecodeError::Invalid("setup header"))?;
        let r = &mut LsbReader::new(body);
        let count = r.read(8) as usize + 1;
        let mut codebooks = meter.with_capacity(count)?;
        for _ in 0..count {
            codebooks.push(Codebook::read(r, meter)?);
        }
        for _ in 0..=r.read(6) {
            if r.read(16) != 0 {
                return Err(DecodeError::Invalid("time domain transform"));
            }
        }
        let count = r.read(6) as usize + 1;
        let mut floors = meter.with_capacity(count)?;
        for _ in 0..count {
            floors.push(match r.read(16) {
                0 => Floor::Lsp(Floor0::read(r, &codebooks, blocksizes, meter)?),
                1 => {
                    meter.add::<Floor1>(1)?;
                    Floor::Linear(Box::new(Floor1::read(r, &codebooks)?))
                }
                _ => return Err(DecodeError::Invalid("floor type")),
            });
        }
        let count = r.read(6) as usize + 1;
        let mut residues = meter.with_capacity(count)?;
        for _ in 0..count {
            meter.add::<[Option<u8>; 8]>(64)?;
            residues.push(Residue::read(r, &codebooks)?);
        }
        let count = r.read(6) as usize + 1;
        let mut mappings = meter.with_capacity(count)?;
        for _ in 0..count {
            meter.add::<u8>(channels + 3 * 256)?;
            mappings.push(Mapping::read(r, channels, floors.len(), residues.len())?);
        }
        let count = r.read(6) as usize + 1;
        let mut modes = meter.with_capacity(count)?;
        for _ in 0..count {
            modes.push(Mode::read(r, mappings.len())?);
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
