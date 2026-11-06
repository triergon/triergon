//! Coupling and spectral extension side information (5.4.3, Annex E 2.3.3).

use super::{BlockTrace, E, Substream, Trace, bad};
use crate::bits::BitReader;
use crate::error::{DecodeError, Feature};
use crate::quant::EXP_SCALE;
use crate::tables::{DEF_CPL_BNDSTRC, spx_band_start};

impl Substream {
    /// Coupling strategy flags; returns whether a new coupling strategy was read.
    #[inline(always)]
    pub(super) fn parse_cpl_use(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        trace: &mut Trace,
    ) -> Result<bool, E> {
        let eac3 = self.eac3;
        let cplstre = if eac3 {
            self.cplstre_blk[blk]
        } else {
            br.bit()
        };
        let mut new_cpl_strategy = false;
        if cplstre {
            let cplinu = if eac3 { self.cplinu_blk[blk] } else { br.bit() };
            new_cpl_strategy = true;
            self.cplinu = cplinu;
            if cplinu {
                if let Some(t) = trace.cur() {
                    t.ecplinu_bits.push(br.pos());
                }
                self.parse_cpl_strategy(br)?;
            } else {
                self.chincpl = [false; 5];
                self.firstcplcos = [true; 5];
                self.firstcplleak = true;
                self.phsflginu = false;
            }
        } else if !eac3 && blk == 0 {
            return Err(bad("coupling strategy missing in block 0"));
        }
        Ok(new_cpl_strategy)
    }

    /// Coupling coordinates and phase flags.
    #[inline(always)]
    pub(super) fn parse_cpl_coords(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        new_cpl_strategy: bool,
        trace: &mut Trace,
    ) {
        let nfch = self.nfchans;
        let eac3 = self.eac3;
        let acmod = self.acmod;
        let mut cplcoe0 = [false; 2];
        for ch in 0..nfch {
            if self.chincpl[ch] {
                let cplcoe = if eac3 && self.firstcplcos[ch] {
                    self.firstcplcos[ch] = false;
                    true
                } else {
                    br.bit()
                };
                if cplcoe {
                    if ch < 2 {
                        cplcoe0[ch] = true;
                    }
                    let mstrcplco = br.get(2) as usize;
                    for bnd in 0..self.ncplbnd {
                        let e = br.get(4) as usize;
                        let m = br.get(4) as f32;
                        let t = if e == 15 { m / 16.0 } else { (m + 16.0) / 32.0 };
                        let sh = e + 3 * mstrcplco;
                        self.cplco[ch][bnd] = t * EXP_SCALE[sh.min(31)];
                    }
                }
            } else if eac3 {
                self.firstcplcos[ch] = true;
            }
        }
        if let Some(t) = trace.cur() {
            t.blocks.push(BlockTrace {
                blk,
                cplstre: new_cpl_strategy,
                phsflginu_bit: self.phsflginu_pos,
                phsflg_bit: br.pos(),
                ncplbnd: self.ncplbnd,
                cplcoe_any: cplcoe0[0] || cplcoe0[1],
                dba_bit: 0,
            });
        }
        if acmod == 2 && self.phsflginu && (cplcoe0[0] || cplcoe0[1]) {
            for bnd in 0..self.ncplbnd {
                self.phsflg[bnd] = br.bit();
            }
            self.features.phase_flags = true;
        }
    }

    pub(super) fn parse_spx_strategy(&mut self, br: &mut BitReader<'_>) -> Result<(), E> {
        self.spxinu = br.bit();
        if !self.spxinu {
            self.chinspx = [false; 5];
            self.firstspxcos = [true; 5];
            return Ok(());
        }
        self.features.spx = true;
        if self.acmod == 1 {
            self.chinspx[0] = true;
        } else {
            for ch in 0..self.nfchans {
                self.chinspx[ch] = br.bit();
            }
        }
        self.spxstrtf = br.get(2) as usize;
        let spxbegf = br.get(3) as usize;
        let spxendf = br.get(3) as usize;
        self.spxbegf = spxbegf;
        self.spx_begin = if spxbegf < 6 {
            spxbegf + 2
        } else {
            spxbegf * 2 - 3
        };
        self.spx_end = if spxendf < 3 {
            spxendf + 5
        } else {
            spxendf * 2 + 3
        };
        if self.spx_begin >= self.spx_end {
            return Err(bad("spx begin >= end"));
        }
        if self.spxstrtf >= self.spx_begin {
            return Err(bad("spx copy start >= begin"));
        }
        if br.bit() {
            for bnd in self.spx_begin + 1..self.spx_end {
                self.spxbndstrc[bnd] = br.bit();
            }
        }
        // Band sizes (Annex E 3.6.2).
        self.nspxbnds = 1;
        self.spxbndsz[0] = 12;
        for bnd in self.spx_begin + 1..self.spx_end {
            if !self.spxbndstrc[bnd] {
                self.spxbndsz[self.nspxbnds] = 12;
                self.nspxbnds += 1;
            } else {
                self.spxbndsz[self.nspxbnds - 1] += 12;
            }
        }
        Ok(())
    }

    pub(super) fn parse_spx_coords(&mut self, br: &mut BitReader<'_>, compatible_noise: bool) {
        for ch in 0..self.nfchans {
            if !self.chinspx[ch] {
                self.firstspxcos[ch] = true;
                continue;
            }
            let spxcoe = if self.firstspxcos[ch] {
                self.firstspxcos[ch] = false;
                true
            } else {
                br.bit()
            };
            if !spxcoe {
                continue;
            }
            let spxblnd = br.get(5) as f32;
            let mstrspxco = br.get(2) as usize;
            let noffset = spxblnd / 32.0;
            let spx_end_mant = spx_band_start(self.spx_end) as f32;
            let mut spxmant = spx_band_start(self.spx_begin);
            for bnd in 0..self.nspxbnds {
                let e = br.get(4) as usize;
                let m = br.get(2) as f32;
                let t = if e == 15 { m / 4.0 } else { (m + 4.0) / 8.0 };
                self.spxco[ch][bnd] = t * EXP_SCALE[(e + 3 * mstrspxco).min(31)];
                // Annex E 3.6.4.2.1 blending factors.
                let bandsize = self.spxbndsz[bnd];
                let mut nratio = (spxmant as f32 + 0.5 * bandsize as f32) / spx_end_mant - noffset;
                nratio = nratio.clamp(0.0, 1.0);
                self.nblend[ch][bnd] = if compatible_noise {
                    (3.0 * nratio).sqrt()
                } else {
                    nratio.sqrt()
                };
                self.sblend[ch][bnd] = (1.0 - nratio).sqrt();
                spxmant += bandsize;
            }
        }
    }

    fn parse_cpl_strategy(&mut self, br: &mut BitReader<'_>) -> Result<(), E> {
        let eac3 = self.eac3;
        if eac3 && br.bit() {
            self.features.ecpl = true;
            return Err(DecodeError::Unsupported(Feature::EnhancedCoupling));
        }
        if eac3 && self.acmod == 2 {
            self.chincpl[0] = true;
            self.chincpl[1] = true;
        } else {
            for ch in 0..self.nfchans {
                self.chincpl[ch] = br.bit();
            }
        }
        for ch in self.nfchans..5 {
            self.chincpl[ch] = false;
        }
        self.phsflginu_pos = if self.acmod == 2 {
            Some(br.pos())
        } else {
            None
        };
        self.phsflginu = if self.acmod == 2 { br.bit() } else { false };
        self.cplbegf = br.get(4) as i32;
        if eac3 && self.spxinu {
            let s = self.spxbegf as i32;
            self.cplendf = if s < 6 { s - 2 } else { s * 2 - 7 };
        } else {
            self.cplendf = br.get(4) as i32;
        }
        let ncplsubnd = 3 + self.cplendf - self.cplbegf;
        if ncplsubnd < 1 {
            return Err(bad("coupling end before begin"));
        }
        let ncplsubnd = ncplsubnd as usize;
        let begf = self.cplbegf as usize;
        if begf + ncplsubnd > 18 {
            return Err(bad("coupling range"));
        }
        self.ncplsubnd = ncplsubnd;
        let read_strc = if eac3 { br.bit() } else { true };
        if read_strc {
            for bnd in 1..ncplsubnd {
                self.cplbndstrc[begf + bnd] = br.bit();
            }
            self.cplbndstrc_set_in_frame = true;
        } else if !self.cplbndstrc_set_in_frame {
            self.cplbndstrc = DEF_CPL_BNDSTRC;
            self.cplbndstrc_set_in_frame = true;
        }
        let mut nb = 0usize;
        for sb in 0..ncplsubnd {
            if sb > 0 && !self.cplbndstrc[begf + sb] {
                nb += 1;
            }
            self.cpl_sb_band[sb] = nb as u8;
        }
        self.ncplbnd = nb + 1;
        self.cplstrtmant = begf * 12 + 37;
        self.cplendmant = ((self.cplendf + 3) * 12 + 37) as usize;
        Ok(())
    }
}
