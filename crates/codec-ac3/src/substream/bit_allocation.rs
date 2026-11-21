//! Bit allocation side information (5.4.3) and the per-channel allocation run (7.2).

use super::{CPL, E, LFE, NCH, Substream, bad};
use crate::bitalloc::{self, BaParams, BaRerun, MaskParams};
use crate::tables::FASTGAIN;
use crate::trace::{BlockTrace, Trace};
use triergon_bitstream::BitReader;

impl Substream {
    /// Bit allocation parametric information, SNR offsets, fast gains and coupling leak.
    /// Returns the lowest bit-allocation stage that must be re-run for every channel.
    #[inline(always)]
    pub(super) fn parse_ba_info(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        cplinu: bool,
        new_cpl_strategy: bool,
    ) -> Result<BaRerun, E> {
        let nfch = self.nfchans;
        let eac3 = self.eac3;
        let mut rerun_all = BaRerun::Nothing;
        if !eac3 || self.bamode {
            if br.bit() {
                self.ba = BaParams {
                    sdcycod: br.read(2) as u8,
                    fdcycod: br.read(2) as u8,
                    sgaincod: br.read(2) as u8,
                    dbpbcod: br.read(2) as u8,
                    floorcod: br.read(3) as u8,
                };
                rerun_all = rerun_all.max(BaRerun::Mask);
            } else if blk == 0 && !eac3 {
                return Err(bad("bit allocation info missing in block 0"));
            }
        } else {
            let def = BaParams {
                sdcycod: 2,
                fdcycod: 1,
                sgaincod: 1,
                dbpbcod: 2,
                floorcod: 7,
            };
            if self.ba != def {
                self.ba = def;
                rerun_all = rerun_all.max(BaRerun::Mask);
            }
        }
        if !eac3 {
            if br.bit() {
                self.csnroffst = br.read(6) as u8;
                if cplinu {
                    self.fsnroffst[CPL] = br.read(4) as u8;
                    self.fgaincod[CPL] = br.read(3) as u8;
                }
                for ch in 0..nfch {
                    self.fsnroffst[ch] = br.read(4) as u8;
                    self.fgaincod[ch] = br.read(3) as u8;
                }
                if self.lfeon {
                    self.fsnroffst[LFE] = br.read(4) as u8;
                    self.fgaincod[LFE] = br.read(3) as u8;
                }
                rerun_all = rerun_all.max(BaRerun::Mask);
            } else if blk == 0 {
                return Err(bad("SNR offsets missing in block 0"));
            }
        } else {
            if self.snroffststr == 0 {
                if blk == 0 || self.csnroffst != self.frmcsnroffst {
                    rerun_all = rerun_all.max(BaRerun::Bap);
                }
                self.csnroffst = self.frmcsnroffst;
                let f = self.frmfsnroffst;
                for v in &mut self.fsnroffst {
                    if *v != f {
                        rerun_all = rerun_all.max(BaRerun::Bap);
                    }
                    *v = f;
                }
            } else {
                let snroffste = blk == 0 || br.bit();
                if snroffste {
                    self.csnroffst = br.read(6) as u8;
                    if self.snroffststr == 1 {
                        let f = br.read(4) as u8;
                        self.fsnroffst = [f; NCH];
                    } else {
                        if cplinu {
                            self.fsnroffst[CPL] = br.read(4) as u8;
                        }
                        for ch in 0..nfch {
                            self.fsnroffst[ch] = br.read(4) as u8;
                        }
                        if self.lfeon {
                            self.fsnroffst[LFE] = br.read(4) as u8;
                        }
                    }
                    rerun_all = rerun_all.max(BaRerun::Bap);
                }
            }
            let fgaincode = self.frmfgaincode && br.bit();
            let mut new = [4u8; NCH];
            if fgaincode {
                if cplinu {
                    new[CPL] = br.read(3) as u8;
                }
                for v in new.iter_mut().take(nfch) {
                    *v = br.read(3) as u8;
                }
                if self.lfeon {
                    new[LFE] = br.read(3) as u8;
                }
            }
            for ch in 0..NCH {
                if self.fgaincod[ch] != new[ch] {
                    self.fgaincod[ch] = new[ch];
                    self.ba_rerun[ch] = self.ba_rerun[ch].max(BaRerun::Mask);
                }
            }
            if self.strmtyp == 0 && br.bit() {
                br.skip(10); // convsnroffst
            }
        }
        if cplinu {
            let cplleake = if eac3 && self.firstcplleak {
                self.firstcplleak = false;
                true
            } else {
                br.bit()
            };
            if cplleake {
                self.cplfleak = br.read(3) as u8;
                self.cplsleak = br.read(3) as u8;
                self.ba_rerun[CPL] = self.ba_rerun[CPL].max(BaRerun::Mask);
            } else if !eac3 && new_cpl_strategy && blk == 0 {
                return Err(bad("coupling leak info missing"));
            }
        }
        Ok(rerun_all)
    }

    /// Delta bit allocation; returns whether it was transmitted in this block.
    #[inline(always)]
    pub(super) fn parse_delta_ba(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        cplinu: bool,
        trace: &mut Trace,
    ) -> Result<bool, E> {
        let nfch = self.nfchans;
        let eac3 = self.eac3;
        if let Some(t) = trace.cur() {
            if t.blocks.last().map(|b| b.blk) != Some(blk) {
                t.blocks.push(BlockTrace {
                    blk,
                    ..Default::default()
                });
            }
            if let Some(b) = t.blocks.last_mut() {
                b.dba_bit = br.position();
            }
        }
        let dbaie = (!eac3 || self.dbaflde) && br.bit();
        if !dbaie {
            return Ok(false);
        }
        self.features.delta_ba = true;
        let mut mode = [0u8; NCH];
        if cplinu {
            mode[CPL] = br.read(2) as u8;
        }
        for ch in 0..nfch {
            mode[ch] = br.read(2) as u8;
        }
        let order: [usize; 6] = [CPL, 0, 1, 2, 3, 4];
        for &ch in &order {
            if ch == CPL && !cplinu {
                continue;
            }
            if ch != CPL && ch >= nfch {
                continue;
            }
            match mode[ch] {
                0 => {}
                1 => {
                    let d = &mut self.dba[ch];
                    d.active = true;
                    d.nseg = br.read(3) as usize;
                    for seg in 0..=d.nseg {
                        d.offst[seg] = br.read(5) as u8;
                        d.len[seg] = br.read(4) as u8;
                        d.ba[seg] = br.read(3) as u8;
                    }
                }
                2 => self.dba[ch].active = false,
                _ => return Err(bad("reserved deltbae")),
            }
        }
        Ok(true)
    }

    pub(super) fn run_bit_allocation(&mut self, cplinu: bool) -> Result<(), E> {
        let all_zero_snr = self.csnroffst == 0
            && self.coded_channels().all(|ch| self.fsnroffst[ch] == 0)
            && (!cplinu || self.fsnroffst[CPL] == 0);
        let chans: [usize; 7] = [0, 1, 2, 3, 4, CPL, LFE];
        for &ch in &chans {
            let present = if ch == CPL {
                cplinu
            } else if ch == LFE {
                self.lfeon
            } else {
                ch < self.nfchans
            };
            let level = self.ba_rerun[ch];
            if !present || level == BaRerun::Nothing {
                continue;
            }
            self.ba_rerun[ch] = BaRerun::Nothing;
            // An AHT channel's allocation is fixed once its mantissas were read (block 0).
            if self.aht[ch] && self.aht_read[ch] {
                continue;
            }
            let (start, end) = (self.strtmant[ch], self.endmant[ch]);
            if start >= end || end > 253 {
                // E.g. reused exponents that were never transmitted (corrupt stream).
                self.ba_rerun[ch] = BaRerun::Psd;
                return Err(bad("bit allocation range"));
            }
            if all_zero_snr {
                self.bap[ch][start..end].fill(0);
                // The cached stages are stale for the next block.
                self.ba_rerun[ch] = BaRerun::Psd;
                continue;
            }
            let cache = &mut self.ba_cache[ch];
            if level >= BaRerun::Psd {
                bitalloc::stage_psd(&self.exps[ch], start, end, cache).map_err(bad)?;
            }
            if level >= BaRerun::Mask {
                let p = MaskParams {
                    start,
                    end,
                    fscod: self.hth_idx,
                    rate_shift: self.rate_shift,
                    params: self.ba,
                    fgain: FASTGAIN[self.fgaincod[ch] as usize],
                    cpl_leak: (ch == CPL).then_some((self.cplfleak, self.cplsleak)),
                    dba: &self.dba[ch],
                };
                bitalloc::stage_mask(&p, cache).map_err(bad)?;
            }
            let snroffset =
                ((((self.csnroffst as i32) - 15) << 4) + self.fsnroffst[ch] as i32) << 2;
            bitalloc::stage_bap(
                cache,
                start,
                end,
                snroffset,
                self.ba.floorcod,
                self.aht[ch],
                &mut self.bap[ch],
            );
        }
        Ok(())
    }
}
