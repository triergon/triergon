//! Exponent strategies, channel bandwidths and grouped exponents (5.4.3, 7.1).

use super::{CPL, E, LFE, REUSE, Substream, bad};
use crate::bits::BitReader;
use crate::tables::spx_band_start;

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
            self.expstr[CPL][blk] = if cplinu { br.get(2) as u8 } else { REUSE };
            for ch in 0..nfch {
                self.expstr[ch][blk] = br.get(2) as u8;
            }
            if self.lfeon {
                self.expstr[LFE][blk] = br.get(1) as u8;
            }
        }
        if blk == 0 {
            for ch in self.coded_channels().collect::<ArrayVec6>().iter() {
                if self.expstr[*ch][0] == REUSE && !eac3 {
                    return Err(bad("exponent reuse in block 0"));
                }
            }
        }
        // Channel bandwidth codes.
        for ch in 0..nfch {
            if self.expstr[ch][blk] != REUSE {
                if self.chincpl[ch] && cplinu {
                    self.endmant[ch] = self.cplstrtmant;
                } else if eac3 && self.spxinu && self.chinspx[ch] {
                    self.endmant[ch] = spx_band_start(self.spx_begin);
                } else {
                    let chbwcod = br.get(6) as usize;
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
        if cplinu && self.expstr[CPL][blk] != REUSE {
            let strategy = self.expstr[CPL][blk];
            let cplabsexp = (br.get(4) as u8) << 1;
            let gs = grp_size(strategy);
            let ngrps = (self.cplendmant - self.cplstrtmant) / (3 * gs);
            let start = self.cplstrtmant;
            decode_exponents(br, &mut self.exps[CPL], cplabsexp, start, ngrps, gs)?;
            self.strtmant[CPL] = self.cplstrtmant;
            self.endmant[CPL] = self.cplendmant;
            self.ba_dirty[CPL] = 3;
        } else if cplinu && new_cpl_strategy && !eac3 && self.expstr[CPL][blk] == REUSE && blk == 0
        {
            return Err(bad("coupling exponents missing"));
        }
        for ch in 0..nfch {
            let strategy = self.expstr[ch][blk];
            if strategy != REUSE {
                let absexp = br.get(4) as u8;
                let gs = grp_size(strategy);
                let end = self.endmant[ch];
                let ngrps = (end - 1 + 3 * gs - 3) / (3 * gs);
                self.exps[ch][0] = absexp;
                decode_exponents(br, &mut self.exps[ch], absexp, 1, ngrps, gs)?;
                br.skip(2); // gainrng
                self.ba_dirty[ch] = 3;
            }
        }
        if self.lfeon && self.expstr[LFE][blk] != REUSE {
            let absexp = br.get(4) as u8;
            self.exps[LFE][0] = absexp;
            decode_exponents(br, &mut self.exps[LFE], absexp, 1, 2, 1)?;
            self.strtmant[LFE] = 0;
            self.endmant[LFE] = 7;
            self.ba_dirty[LFE] = 3;
        }
        Ok(())
    }
}

/// Resolve the exponent group count (7.1.3) for D15/D25/D45.
#[inline]
fn grp_size(strategy: u8) -> usize {
    match strategy {
        1 => 1,
        2 => 2,
        _ => 4,
    }
}

/// Small stack vector of coded channel indices.
struct ArrayVec6 {
    v: [usize; 6],
    n: usize,
}

impl FromIterator<usize> for ArrayVec6 {
    fn from_iter<I: IntoIterator<Item = usize>>(iter: I) -> Self {
        let mut a = ArrayVec6 { v: [0; 6], n: 0 };
        for x in iter {
            if a.n < 6 {
                a.v[a.n] = x;
                a.n += 1;
            }
        }
        a
    }
}

impl ArrayVec6 {
    fn iter(&self) -> core::slice::Iter<'_, usize> {
        self.v[..self.n].iter()
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
        let gexp = br.get(7) as usize;
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
