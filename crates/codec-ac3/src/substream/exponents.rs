//! Exponent strategies, channel bandwidths and grouped exponents (5.4.3, 7.1).

use super::{CPL, E, LFE, Substream, bad};
use crate::bitalloc::BaRerun;
use crate::tables::spx_band_start;
use triergon_bitstream::BitReader;

impl Substream {
    /// Exponent strategies (AC-3 carries them per block) and channel bandwidth codes.
    #[inline(always)]
    pub(super) fn parse_exp_strategies(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        cplinu: bool,
    ) -> Result<(), E> {
        let nfch = self.nfchans;
        let eac3 = self.eac3;
        if !eac3 {
            self.expstr[CPL][blk] = if cplinu {
                ExpStrategy::from_code(br.read(2))
            } else {
                ExpStrategy::Reuse
            };
            for ch in 0..nfch {
                self.expstr[ch][blk] = ExpStrategy::from_code(br.read(2));
            }
            if self.lfeon {
                self.expstr[LFE][blk] = ExpStrategy::from_code(br.read(1));
            }
        }
        if blk == 0
            && !eac3
            && self
                .coded_channels()
                .any(|ch| self.expstr[ch][0] == ExpStrategy::Reuse)
        {
            return Err(bad("exponent reuse in block 0"));
        }
        // Channel bandwidth codes.
        for ch in 0..nfch {
            if self.expstr[ch][blk] != ExpStrategy::Reuse {
                if self.chincpl[ch] && cplinu {
                    self.endmant[ch] = self.cplstrtmant;
                } else if eac3 && self.spxinu && self.chinspx[ch] {
                    self.endmant[ch] = spx_band_start(self.spx_begin);
                } else {
                    let chbwcod = br.read(6) as usize;
                    if chbwcod > 60 {
                        return Err(bad("chbwcod > 60"));
                    }
                    self.endmant[ch] = (chbwcod + 12) * 3 + 37;
                }
                self.strtmant[ch] = 0;
            }
        }
        Ok(())
    }

    /// Coupling, full-bandwidth and LFE exponents.
    #[inline(always)]
    pub(super) fn parse_exponents(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        cplinu: bool,
        new_cpl_strategy: bool,
    ) -> Result<(), E> {
        let nfch = self.nfchans;
        let eac3 = self.eac3;
        if cplinu && self.expstr[CPL][blk] != ExpStrategy::Reuse {
            let cplabsexp = (br.read(4) as u8) << 1;
            let gs = self.expstr[CPL][blk].group_size();
            let ngrps = (self.cplendmant - self.cplstrtmant) / (3 * gs);
            let start = self.cplstrtmant;
            decode_exponents(br, &mut self.exps[CPL], cplabsexp, start, ngrps, gs)?;
            self.strtmant[CPL] = self.cplstrtmant;
            self.endmant[CPL] = self.cplendmant;
            self.ba_rerun[CPL] = BaRerun::Psd;
        } else if cplinu
            && new_cpl_strategy
            && !eac3
            && self.expstr[CPL][blk] == ExpStrategy::Reuse
            && blk == 0
        {
            return Err(bad("coupling exponents missing"));
        }
        for ch in 0..nfch {
            let strategy = self.expstr[ch][blk];
            if strategy != ExpStrategy::Reuse {
                let absexp = br.read(4) as u8;
                let gs = strategy.group_size();
                let end = self.endmant[ch];
                let ngrps = (end - 1 + 3 * gs - 3) / (3 * gs);
                self.exps[ch][0] = absexp;
                decode_exponents(br, &mut self.exps[ch], absexp, 1, ngrps, gs)?;
                br.skip(2); // gainrng
                self.ba_rerun[ch] = BaRerun::Psd;
            }
        }
        if self.lfeon && self.expstr[LFE][blk] != ExpStrategy::Reuse {
            let absexp = br.read(4) as u8;
            self.exps[LFE][0] = absexp;
            decode_exponents(br, &mut self.exps[LFE], absexp, 1, 2, 1)?;
            self.strtmant[LFE] = 0;
            self.endmant[LFE] = 7;
            self.ba_rerun[LFE] = BaRerun::Psd;
        }
        Ok(())
    }
}

/// Exponent strategy of one channel in one block (5.4.3.21-23, Table 5.16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExpStrategy {
    /// Reuse the previous block's exponents.
    Reuse = 0,
    /// One exponent per transform coefficient.
    D15 = 1,
    /// One exponent per two coefficients.
    D25 = 2,
    /// One exponent per four coefficients.
    D45 = 3,
}

impl ExpStrategy {
    /// The strategy of a chexpstr/cplexpstr code (2 bits) or lfeexpstr (1 bit).
    #[inline(always)]
    pub(crate) fn from_code(code: u32) -> Self {
        match code & 3 {
            0 => ExpStrategy::Reuse,
            1 => ExpStrategy::D15,
            2 => ExpStrategy::D25,
            _ => ExpStrategy::D45,
        }
    }

    /// Coefficients sharing one exponent (7.1.3).
    #[inline]
    fn group_size(self) -> usize {
        match self {
            ExpStrategy::D15 => 1,
            ExpStrategy::D25 => 2,
            _ => 4,
        }
    }
}

/// Decode grouped differential exponents (7.1.3) into exps[first..].
fn decode_exponents(
    br: &mut BitReader<'_>,
    exps: &mut [u8; 256],
    absexp: u8,
    first: usize,
    ngrps: usize,
    gs: usize,
) -> Result<(), E> {
    let mut prev = absexp as i32;
    let mut pos = first;
    if pos + ngrps * 3 * gs > 256 {
        return Err(bad("exponent range"));
    }
    for _ in 0..ngrps {
        let gexp = br.read(7) as usize;
        if gexp > 124 {
            return Err(bad("grouped exponent > 124"));
        }
        let d = [gexp / 25, (gexp % 25) / 5, gexp % 5];
        for &m in &d {
            prev += m as i32 - 2;
            if !(0..=24).contains(&prev) {
                return Err(bad("exponent out of range"));
            }
            let v = prev as u8;
            match gs {
                1 => exps[pos] = v,
                2 => {
                    exps[pos] = v;
                    exps[pos + 1] = v;
                }
                _ => {
                    exps[pos] = v;
                    exps[pos + 1] = v;
                    exps[pos + 2] = v;
                    exps[pos + 3] = v;
                }
            }
            pos += gs;
        }
    }
    Ok(())
}
