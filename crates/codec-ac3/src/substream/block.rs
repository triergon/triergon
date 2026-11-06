//! One audio block (5.3.3, Annex E 2.3.3): block switch and dither flags, dynamic
//! range, rematrixing and the sequence of side-information sections up to the mantissas.

use super::{E, NoiseMode, Substream, Trace};
use crate::bits::BitReader;
use crate::noise::Rng;
use crate::quant::GroupTables;

impl Substream {
    /// Side-information sections in bitstream order, bit allocation, then mantissas.
    pub(super) fn parse_block(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        rng: &mut Rng,
        noise: NoiseMode,
        groups: &GroupTables,
        trace: &mut Trace,
    ) -> Result<(), E> {
        self.parse_block_switch_and_dither(br, trace);
        self.parse_dynrng(br);

        // Spectral extension strategy and coordinates (E-AC-3).
        if self.eac3 {
            let spxstre = blk == 0 || br.bit();
            if spxstre {
                self.parse_spx_strategy(br)?;
            }
            if self.spxinu {
                self.parse_spx_coords(br, rng.compatible());
            }
        }

        let new_cpl_strategy = self.parse_cpl_use(br, blk, trace)?;
        let cplinu = self.cplinu;
        self.features.coupling |= cplinu;
        if cplinu {
            self.parse_cpl_coords(br, blk, new_cpl_strategy, trace);
        }
        if self.acmod == 2 {
            self.parse_rematrix(br, blk);
        }
        self.parse_exp_strategies(br, blk, cplinu)?;
        self.parse_exponents(br, blk, cplinu, new_cpl_strategy)?;
        let mut all_dirty = self.parse_ba_info(br, blk, cplinu, new_cpl_strategy)?;
        if self.parse_delta_ba(br, blk, cplinu, trace)? {
            all_dirty = all_dirty.max(2);
        }

        // Skip field.
        if (!self.eac3 || self.skipflde) && br.bit() {
            let skipl = br.get(9) as usize;
            br.skip(skipl * 8);
            self.features.skip_fields = true;
        }

        for d in self.ba_dirty.iter_mut() {
            *d = (*d).max(all_dirty);
        }
        self.run_bit_allocation(cplinu)?;

        self.unpack_mantissas(br, blk, rng, noise, groups)
    }

    /// Block switch and dither flags.
    #[inline(always)]
    fn parse_block_switch_and_dither(&mut self, br: &mut BitReader<'_>, trace: &mut Trace) {
        let nfch = self.nfchans;
        let eac3 = self.eac3;
        if !eac3 || self.blkswe {
            for ch in 0..nfch {
                self.blksw[ch] = br.bit();
            }
        } else {
            self.blksw = [false; 5];
        }
        if let Some(t) = trace.cur() {
            t.dithflag_insert_bits.push(br.pos());
        }
        if !eac3 || self.dithflage {
            for ch in 0..nfch {
                if let Some(t) = trace.cur() {
                    t.dithflag_bits.push(br.pos());
                }
                self.dithflag[ch] = br.bit();
            }
        } else {
            self.dithflag = [true; 5];
        }
        self.features.block_switch |= self.blksw[..nfch].iter().any(|&b| b);
        self.features.dither |= self.dithflag[..nfch].iter().any(|&b| b);
    }

    /// Dynamic range gain words.
    #[inline(always)]
    fn parse_dynrng(&mut self, br: &mut BitReader<'_>) {
        if br.bit() {
            self.dynrng[0] = dynrng_gain(br.get(8));
            self.features.dynrng = true;
        }
        if self.acmod == 0 && br.bit() {
            self.dynrng[1] = dynrng_gain(br.get(8));
        }
    }

    /// Rematrixing strategy and flags (2/0 mode only).
    #[inline(always)]
    fn parse_rematrix(&mut self, br: &mut BitReader<'_>, blk: usize) {
        let eac3 = self.eac3;
        let rematstr = if eac3 { blk == 0 || br.bit() } else { br.bit() };
        if rematstr {
            self.nrematbd = self.rematrix_bands();
            for b in 0..self.nrematbd {
                self.rematflg[b] = br.bit();
            }
        } else if !eac3 && blk == 0 {
            self.nrematbd = 0;
        }
        if self.rematflg[..self.nrematbd].iter().any(|&f| f) {
            self.features.rematrix = true;
        }
    }

    fn rematrix_bands(&self) -> usize {
        if self.cplinu {
            if self.cplbegf == 0 {
                2
            } else if self.cplbegf < 3 {
                3
            } else {
                4
            }
        } else if self.eac3 && self.spxinu {
            if self.spxbegf < 2 { 3 } else { 4 }
        } else {
            4
        }
    }
}

#[inline]
fn dynrng_gain(v: u32) -> f32 {
    // 7.7.1.2: X0X1X2.Y3..Y7, gain = 2^(X+1) * 0.1Y (base 2), X signed 3-bit.
    let x = ((v as u8 as i8) >> 5) as i32;
    let y = (v & 31) as f32;
    let m = (32.0 + y) / 64.0;
    if x + 1 >= 0 {
        m * (1u32 << (x + 1)) as f32
    } else {
        m / (1u32 << (-(x + 1))) as f32
    }
}
